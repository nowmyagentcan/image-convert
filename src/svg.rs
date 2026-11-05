//! SVG through resvg with no file or network access, `.svgz`/`.emz`/`.wmz` inflated under a cap,
//! and text drawn with the system's fonts or the bundled Noto Sans.

use std::io::Read;
use std::path::Path;
use std::sync::{Arc, Mutex};

use flate2::read::GzDecoder;
use resvg::tiny_skia::{Pixmap, Transform};
use resvg::usvg::fontdb::Source;
use resvg::usvg::{ImageHrefResolver, ImageKind, Options as Parse, Tree};

use crate::color::to_srgb;
use crate::detect::{Kind, detect, find};
use crate::encode::encode;
use crate::error::{Error, Warning};
use crate::{Format, Layout, Options, Pixels, Samples, jpeg, resize};

static NOTO_SANS: &[u8] = include_bytes!("../assets/fonts/NotoSans-Regular.ttf");
const MAX_INFLATED: u64 = 64 << 20;
const MAX_NESTED_SVGS: u32 = 1000;

/// What the images an SVG embeds add up to, over every use of each at every depth.
#[derive(Default)]
struct Embedded {
    pixels: u64,
    svgs: u32,
    svg_bytes: u64,
    error: Option<Error>,
}

/// The SVG, EMF or WMF inside a gzip stream, at most 64 MiB of it.
pub fn ungzip(data: &[u8]) -> Result<(Vec<u8>, Kind), Error> {
    let mut inner = Vec::new();
    GzDecoder::new(data)
        .take(MAX_INFLATED + 1)
        .read_to_end(&mut inner)
        .map_err(|e| Error::corrupt("gzip", e))?;
    if inner.len() as u64 > MAX_INFLATED {
        return Err(Error::inflates_too_large());
    }
    match detect(&inner, Path::new("")) {
        Ok(kind @ (Kind::Svg | Kind::Emf | Kind::Wmf)) => Ok((inner, kind)),
        _ => Err(Error::unknown_format()),
    }
}

/// Renders an SVG at its own size (at most 8192 px a side unless `--width`/`--height` asks) or
/// the size `options` gives it; returns the pixels and the drawing's own size in px.
pub fn render(data: &[u8], options: &Options) -> Result<(Pixels, (u32, u32)), Error> {
    refuse(data)?;
    let skipped = Mutex::new(Vec::new());
    let embedded = Mutex::new(Embedded::default());
    let mut parse = Parse {
        font_family: "serif".into(),
        image_href_resolver: ImageHrefResolver {
            resolve_data: Box::new(|_, data, parse| {
                embed(data, parse, &embedded).unwrap_or_else(|e| {
                    embedded.lock().unwrap().error.get_or_insert(e);
                    None
                })
            }),
            resolve_string: Box::new(|href, _| {
                skipped.lock().unwrap().push(href.to_string());
                None
            }),
        },
        ..Parse::default()
    };
    let fonts = parse.fontdb_mut();
    if find(data, b"<text").is_some() {
        fonts.load_system_fonts();
    }
    fonts.load_font_source(Source::Binary(Arc::new(NOTO_SANS)));
    fonts.set_serif_family("Noto Sans");
    fonts.set_sans_serif_family("Noto Sans");
    fonts.set_monospace_family("Noto Sans");
    fonts.set_cursive_family("Noto Sans");
    fonts.set_fantasy_family("Noto Sans");
    let tree = Tree::from_data(data, &parse);
    drop(parse);
    if let Some(error) = embedded.into_inner().unwrap().error {
        return Err(error);
    }
    let tree = tree.map_err(|e| Error::corrupt("SVG", e))?;

    let size = tree.size();
    let own = [size.width(), size.height()].map(|s| s.ceil().min(u32::MAX as f32) as u32);
    let (width, height, reduced) = resize::vector_target(own, options)?;
    let mut warnings: Vec<_> = reduced.into_iter().collect();
    let skipped = skipped.into_inner().unwrap();
    if let Some(first) = skipped.first() {
        warnings.push(Warning::external_image_skipped(first, skipped.len() - 1));
    }

    let mut pixmap = Pixmap::new(width, height).expect("the size is checked above");
    let scale = Transform::from_scale(width as f32 / size.width(), height as f32 / size.height());
    resvg::render(&tree, scale, &mut pixmap.as_mut());
    let pixels = Pixels {
        width,
        height,
        layout: Layout::Rgba,
        samples: Samples::U8(pixmap.take_demultiplied()),
        bit_depth: 8,
        colour: None,
        exif: None,
        orientation: 1,
        frames: 1,
        warnings,
    };
    Ok((pixels, (own[0], own[1])))
}

/// Refuses UTF-16 text and entities that expand other entities, before parsing.
fn refuse(data: &[u8]) -> Result<(), Error> {
    if data.starts_with(b"\xff\xfe") || data.starts_with(b"\xfe\xff") || data.contains(&0) {
        return Err(Error::unsupported_variant("SVG", "UTF-16 text"));
    }
    if nested_entities(data) {
        let variant = "entities that reference other entities";
        return Err(Error::unsupported_variant("SVG", variant));
    }
    Ok(())
}

/// A data image for usvg, held to the limits with every image resolved before it; a nested SVG
/// resolves its own images through here again, and a JPEG resvg would misread is passed on as
/// PNG. Anything else is skipped, as usvg does.
fn embed(
    data: Arc<Vec<u8>>,
    parse: &Parse,
    budget: &Mutex<Embedded>,
) -> Result<Option<ImageKind>, Error> {
    if budget.lock().unwrap().error.is_some() {
        return Ok(None);
    }
    let kind = detect(&data, Path::new("")).ok();
    let raster = match kind {
        Some(Kind::Png) => ImageKind::PNG,
        Some(Kind::Jpeg) => ImageKind::JPEG,
        Some(Kind::Gif) => ImageKind::GIF,
        Some(Kind::Webp) => ImageKind::WEBP,
        Some(kind @ (Kind::Svg | Kind::Gzip)) => {
            let inflated = match kind {
                Kind::Svg => None,
                _ => match ungzip(&data)? {
                    (svg, Kind::Svg) => Some(svg),
                    _ => return Ok(None),
                },
            };
            let svg = inflated.as_deref().unwrap_or(&data);
            refuse(svg)?;
            let mut b = budget.lock().unwrap();
            b.svgs += 1;
            b.svg_bytes += svg.len() as u64;
            if b.svgs > MAX_NESTED_SVGS || b.svg_bytes > MAX_INFLATED {
                let what = "hold more than 1,000 SVG images, or more than 64 MiB of them";
                return Err(Error::embeds_too_much(what));
            }
            drop(b);
            return Ok(Tree::from_data_nested(svg, parse).ok().map(ImageKind::SVG));
        }
        _ => return Ok(None),
    };
    let size = match kind {
        Some(Kind::Jpeg) => jpeg::decoder_size(&data, jpeg::misread_by_zune(&data)).ok(),
        _ => imagesize::blob_size(&data)
            .ok()
            .map(|size| (size.width as u32, size.height as u32)),
    };
    let Some((width, height)) = size else {
        return Ok(None);
    };
    let (width, height) = (u64::from(width), u64::from(height));
    let mut b = budget.lock().unwrap();
    b.pixels += width * height;
    if width > 65535 || height > 65535 || b.pixels > 200_000_000 {
        let what = "add up to more than 200 megapixels, or one is over 65,535 pixels a side";
        return Err(Error::embeds_too_much(what));
    }
    if kind == Some(Kind::Jpeg) && jpeg::misread_by_zune(&data) {
        let (image, _) = to_srgb(&mut jpeg::decode(&data)?);
        let png = encode(&image, Format::Png, 0, None);
        return Ok(Some(ImageKind::PNG(Arc::new(png))));
    }
    Ok(Some(raster(data)))
}

/// Whether an `<!ENTITY` declaration's quoted value holds `&`, so expanding it expands others.
fn nested_entities(data: &[u8]) -> bool {
    let mut rest = data;
    while let Some(at) = find(rest, b"<!ENTITY") {
        rest = &rest[at + 8..];
        let Some(open) = rest.iter().position(|&b| b == b'"' || b == b'\'') else {
            return false;
        };
        let quote = rest[open];
        rest = &rest[open + 1..];
        let close = rest.iter().position(|&b| b == quote).unwrap_or(rest.len());
        if rest[..close].contains(&b'&') {
            return true;
        }
        rest = &rest[close..];
    }
    false
}
