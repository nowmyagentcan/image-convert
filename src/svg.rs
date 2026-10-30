//! SVG through resvg with no file or network access, `.svgz`/`.emz`/`.wmz` inflated under a cap,
//! and text drawn with the system's fonts or the bundled Noto Sans.

use std::io::Read;
use std::path::Path;
use std::sync::{Arc, Mutex};

use flate2::read::GzDecoder;
use resvg::tiny_skia::{Pixmap, Transform};
use resvg::usvg::fontdb::Source;
use resvg::usvg::{ImageHrefResolver, Options as Parse, Tree};

use crate::detect::{Kind, detect, find};
use crate::error::{Error, Warning};
use crate::{Layout, Options, Pixels, Samples, resize};

static NOTO_SANS: &[u8] = include_bytes!("../assets/fonts/NotoSans-Regular.ttf");
const MAX_INFLATED: u64 = 64 << 20;

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
    if data.starts_with(b"\xff\xfe") || data.starts_with(b"\xfe\xff") || data.contains(&0) {
        return Err(Error::unsupported_variant("SVG", "UTF-16 text"));
    }
    if nested_entities(data) {
        let variant = "entities that reference other entities";
        return Err(Error::unsupported_variant("SVG", variant));
    }
    let skipped = Mutex::new(Vec::new());
    let mut parse = Parse {
        font_family: "serif".into(),
        image_href_resolver: ImageHrefResolver {
            resolve_data: ImageHrefResolver::default_data_resolver(),
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
    let tree = Tree::from_data(data, &parse).map_err(|e| Error::corrupt("SVG", e))?;
    drop(parse);

    let size = tree.size();
    let own = [size.width(), size.height()].map(|s| s.ceil().min(u32::MAX as f32) as u32);
    let explicit = options.width.is_some() || options.height.is_some();
    let base = match own[0].max(own[1]) {
        longer if !explicit && longer > 8192 => {
            let scale = 8192.0 / f64::from(longer);
            own.map(|s| resize::scaled(s, scale))
        }
        _ => own,
    };
    let (width, height) = resize::target(base[0], base[1], options, false);
    if width.max(height) > 16384 || u64::from(width) * u64::from(height) > 200_000_000 {
        return Err(Error::image_too_large(width.into(), height.into(), true));
    }
    let mut warnings = Vec::new();
    if base != own && [width, height] == base {
        let from = (own[0].into(), own[1].into());
        warnings.push(Warning::render_reduced(from, (width, height)));
    }
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
