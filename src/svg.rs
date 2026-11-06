//! SVG through resvg with no file or network access, `.svgz`/`.emz`/`.wmz` inflated under a cap,
//! and text drawn with the system's fonts or the bundled Noto Sans.

use std::collections::HashMap;
use std::io::Read;
use std::path::Path;
use std::sync::{Arc, Mutex};

use flate2::read::GzDecoder;
use resvg::tiny_skia::{Pixmap, Rect, Transform};
use resvg::usvg::filter::{Filter, Kind as Primitive};
use resvg::usvg::fontdb::Source;
use resvg::usvg::{
    ClipPath, Group, ImageHrefResolver, ImageKind, Mask, Node, Options as Parse, Paint, Pattern,
    Tree,
};

use crate::color::to_srgb;
use crate::detect::{Kind, detect, find};
use crate::encode::encode;
use crate::error::{Error, Warning};
use crate::svg_demand::{self, Demand, MAX_COST, MAX_DEPTH};
use crate::{Format, Layout, Options, Pixels, Samples, jpeg, resize};

static NOTO_SANS: &[u8] = include_bytes!("../assets/fonts/NotoSans-Regular.ttf");
const MAX_INFLATED: u64 = 64 << 20;
const MAX_NESTED_SVGS: u32 = 1000;
/// What resvg may hold at once: it sizes layers, masks and pattern tiles itself, and tiny-skia
/// aborts the process on a pixmap it can't allocate.
const MAX_RENDER: u64 = 2 << 30;

/// What the images an SVG embeds add up to, over every use of each at every depth.
#[derive(Default)]
struct Embedded {
    pixels: u64,
    svgs: u32,
    svg_bytes: u64,
    /// The cost of every image, and the depth of the images being parsed: parsing an image
    /// happens inside the parsing of the file around it.
    demand: Demand,
    error: Option<Error>,
}

impl Embedded {
    fn add(&mut self, demand: Demand) -> Result<(), Error> {
        self.demand.cost = self.demand.cost.saturating_add(demand.cost);
        self.demand.depth = self.demand.depth.saturating_add(demand.depth);
        if self.demand.depth > MAX_DEPTH {
            let variant = "references in a loop, or nesting past what can be followed";
            return Err(Error::unsupported_variant("SVG", variant));
        }
        if self.demand.cost > MAX_COST {
            return Err(Error::drawing_too_large());
        }
        Ok(())
    }
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
    let mut budget = Embedded::default();
    budget.add(refuse(data)?)?;
    let skipped = Mutex::new(Vec::new());
    let embedded = Mutex::new(budget);
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
    let embedded = embedded.into_inner().unwrap();
    if let Some(error) = embedded.error {
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

    let scale = Transform::from_scale(width as f32 / size.width(), height as f32 / size.height());
    let target = (width, height);
    let held = held_by_nodes(tree.root(), scale, target, layer_bounds(target));
    if bytes(target)
        .saturating_add(held)
        .saturating_add(embedded.demand.cost)
        > MAX_RENDER
    {
        return Err(Error::render_too_large(width, height));
    }
    let mut pixmap = Pixmap::new(width, height).expect("the size is checked above");
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

/// Refuses what parsing has no limit for, UTF-16 text and entities that expand other entities or
/// expand past 64 MiB, and gives what building it takes, for [`Embedded::add`].
fn refuse(data: &[u8]) -> Result<Demand, Error> {
    if data.starts_with(b"\xff\xfe") || data.starts_with(b"\xfe\xff") || data.contains(&0) {
        return Err(Error::unsupported_variant("SVG", "UTF-16 text"));
    }
    let declared = entities(data);
    if declared.iter().any(|(_, value)| value.contains(&b'&')) {
        let variant = "entities that reference other entities";
        return Err(Error::unsupported_variant("SVG", variant));
    }
    if expansion(data, &declared) > MAX_INFLATED {
        let variant = "entities that expand past 64 MiB";
        return Err(Error::unsupported_variant("SVG", variant));
    }
    Ok(svg_demand::demand(data))
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
            let demand = refuse(svg)?;
            let mut b = budget.lock().unwrap();
            b.add(demand)?;
            b.svgs += 1;
            b.svg_bytes += svg.len() as u64;
            if b.svgs > MAX_NESTED_SVGS || b.svg_bytes > MAX_INFLATED {
                let what = "hold more than 1,000 SVG images, or more than 64 MiB of them";
                return Err(Error::embeds_too_much(what));
            }
            drop(b);
            let tree = Tree::from_data_nested(svg, parse).ok();
            budget.lock().unwrap().demand.depth -= demand.depth;
            return Ok(tree.map(ImageKind::SVG));
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

/// Each entity the prolog declares, with its value. Steps over the prolog as an XML parser does,
/// so that no comment or quoted string can hide a declaration.
fn entities(data: &[u8]) -> Vec<(&[u8], &[u8])> {
    let mut found = Vec::new();
    let mut at = 0;
    while let Some(open) = data[at..].iter().position(|&b| b == b'<') {
        at += open;
        let rest = &data[at..];
        let past = |end: &[u8]| find(rest, end).map_or(rest.len(), |i| i + end.len());
        at += if rest.starts_with(b"<!--") {
            past(b"-->")
        } else if rest.starts_with(b"<?") {
            past(b"?>")
        } else if rest.starts_with(b"<!") {
            let (len, value) = declaration(rest);
            let mut words = rest[2..].split(u8::is_ascii_whitespace);
            let keyword = words.next();
            let name = words.find(|word| !word.is_empty());
            if let (Some(b"ENTITY"), Some(name), Some(value)) = (keyword, name, value) {
                found.push((name, value));
            }
            len
        } else {
            break;
        };
    }
    found
}

/// Where the declaration at the start of `rest` ends (at `>`, or the `[` of a DOCTYPE's subset),
/// and its first quoted string.
fn declaration(rest: &[u8]) -> (usize, Option<&[u8]>) {
    let mut value = None;
    let mut quote = None;
    for (i, &b) in rest.iter().enumerate() {
        match quote {
            Some((mark, open)) if b == mark => {
                value.get_or_insert(&rest[open..i]);
                quote = None;
            }
            Some(_) => {}
            None if matches!(b, b'"' | b'\'') => quote = Some((b, i + 1)),
            None if matches!(b, b'>' | b'[') => return (i + 1, value),
            None => {}
        }
    }
    (rest.len(), value)
}

/// How many bytes the references to `entities` expand to.
fn expansion(data: &[u8], entities: &[(&[u8], &[u8])]) -> u64 {
    let mut longest = HashMap::new();
    for (name, value) in entities {
        let length = longest.entry(*name).or_default();
        *length = value.len().max(*length);
    }
    data.split(|&b| b == b'&')
        .skip(1)
        .filter_map(|reference| reference.split(|&b| b == b';').next())
        .filter_map(|name| longest.get(name))
        .map(|&length| length as u64)
        .sum()
}

/// The bytes of a pixmap of this size.
fn bytes((width, height): (u32, u32)) -> u64 {
    u64::from(width)
        .saturating_mul(height.into())
        .saturating_mul(4)
}

/// What resvg keeps layers within on a pixmap of this size: 5 times its size, centred on it.
fn layer_bounds((width, height): (u32, u32)) -> Rect {
    let (width, height) = (width as f32, height as f32);
    Rect::from_ltrb(-2.0 * width, -2.0 * height, 3.0 * width, 3.0 * height)
        .expect("a pixmap has a size")
}

// The functions below add up the pixmaps resvg holds at once, in the steps of its own `render.rs`,
// `clip.rs`, `mask.rs`, `path.rs`, `image.rs` and `filter/mod.rs`. `on` is the size of the pixmap
// being drawn on, and `bounds` what layers are kept within.

fn held_by_nodes(parent: &Group, ts: Transform, on: (u32, u32), bounds: Rect) -> u64 {
    let children = parent.children().iter();
    children
        .map(|node| held_by_node(node, ts, on, bounds))
        .max()
        .unwrap_or(0)
}

fn held_by_node(node: &Node, ts: Transform, on: (u32, u32), bounds: Rect) -> u64 {
    match node {
        Node::Group(group) => held_by_group(group, ts, on, bounds),
        Node::Text(text) => held_by_group(text.flattened(), ts, on, bounds),
        Node::Path(path) => {
            let fill = path.fill().map(|fill| fill.paint());
            let stroke = path.stroke().map(|stroke| stroke.paint());
            let paints = [fill, stroke].into_iter().flatten();
            let tiles = paints.map(|paint| match paint {
                Paint::Pattern(pattern) => held_by_tile(pattern, ts, bounds),
                _ => 0,
            });
            tiles.max().unwrap_or(0)
        }
        Node::Image(image) => match image.kind() {
            ImageKind::SVG(tree) => {
                bytes(on).saturating_add(held_by_nodes(tree.root(), ts, on, layer_bounds(on)))
            }
            _ => {
                let size = image.size();
                bytes((size.width() as u32, size.height() as u32))
            }
        },
    }
}

fn held_by_group(group: &Group, ts: Transform, on: (u32, u32), bounds: Rect) -> u64 {
    let ts = ts.pre_concat(group.transform());
    if !group.should_isolate() {
        return held_by_nodes(group, ts, on, bounds);
    }
    // A filter runs unrotated: its layer turns with the group, and so do the bounds.
    let rest = if group.filters().is_empty() {
        Transform::identity()
    } else {
        let (sx, sy) = (ts.sx.hypot(ts.ky), ts.kx.hypot(ts.sy));
        Transform::from_row(ts.sx / sx, ts.ky / sx, ts.kx / sy, ts.sy / sy, 0.0, 0.0)
    };
    let Some(inverse) = rest.invert() else {
        return 0;
    };
    let ts = inverse.pre_concat(ts);
    let bbox = group.layer_bounding_box().transform(ts);
    let (Some(bounds), Some(bbox)) = (bounds.transform(inverse), bbox) else {
        return 0;
    };
    let (left, top) = (bbox.left().max(bounds.left()), bbox.top().max(bounds.top()));
    let (right, bottom) = (
        bbox.right().min(bounds.right()),
        bbox.bottom().min(bounds.bottom()),
    );
    if left >= right || top >= bottom {
        return 0;
    }
    let layer = (
        (right - left).ceil() as u32 + 4,
        (bottom - top).ceil() as u32 + 4,
    );
    let ts = ts.post_translate(-left, -top);

    let filters = group.filters().iter();
    let filters = filters
        .map(|filter| held_by_filter(filter, ts, layer))
        .max();
    let clip = group.clip_path().map(|clip| held_by_clip(clip, ts, layer));
    let mask = group
        .mask()
        .map(|mask| held_by_mask(mask, ts, layer, bounds));
    let held = held_by_nodes(group, ts, layer, bounds);
    let held = [filters, clip, mask]
        .into_iter()
        .flatten()
        .fold(held, u64::max);
    bytes(layer).saturating_add(held)
}

fn held_by_tile(pattern: &Pattern, ts: Transform, bounds: Rect) -> u64 {
    let (sx, sy) = ts.pre_concat(pattern.transform()).get_scale();
    let rect = pattern.rect();
    let size = (
        (rect.width() * sx).round() as u32,
        (rect.height() * sy).round() as u32,
    );
    if size.0 == 0 || size.1 == 0 {
        return 0;
    }
    let scale = Transform::from_scale(sx, sy);
    bytes(size).saturating_add(held_by_nodes(pattern.root(), scale, size, bounds))
}

/// Every result stays until the filter ends, and the input and a blur's buffers add to them.
fn held_by_filter(filter: &Filter, ts: Transform, on: (u32, u32)) -> u64 {
    let (sx, sy) = ts.get_scale();
    let scale = Transform::from_scale(sx, sy);
    let primitives = filter.primitives().iter();
    let images = primitives.filter_map(|primitive| match primitive.kind() {
        Primitive::Image(image) => Some(held_by_nodes(image.root(), scale, on, layer_bounds(on))),
        _ => None,
    });
    let images = images.max().unwrap_or(0);
    let results = filter.primitives().len() as u64 + 3;
    bytes(on).saturating_mul(results).saturating_add(images)
}

fn held_by_clip(clip: &ClipPath, ts: Transform, on: (u32, u32)) -> u64 {
    let held = held_by_clip_children(clip.root(), ts.pre_concat(clip.transform()), on);
    let next = clip
        .clip_path()
        .map_or(0, |next| held_by_clip(next, ts, on));
    (bytes(on) / 4 * 5).saturating_add(held.max(next))
}

fn held_by_clip_children(parent: &Group, ts: Transform, on: (u32, u32)) -> u64 {
    let children = parent.children().iter().map(|child| match child {
        Node::Text(text) => held_by_clip_children(text.flattened(), ts, on),
        Node::Group(group) => {
            let ts = ts.pre_concat(group.transform());
            let children = held_by_clip_children(group, ts, on);
            match group.clip_path() {
                Some(clip) => bytes(on).saturating_add(held_by_clip(clip, ts, on).max(children)),
                None => children,
            }
        }
        _ => 0,
    });
    children.max().unwrap_or(0)
}

fn held_by_mask(mask: &Mask, ts: Transform, on: (u32, u32), bounds: Rect) -> u64 {
    if mask.root().children().is_empty() {
        return 0;
    }
    let held = held_by_nodes(mask.root(), ts, on, bounds);
    let next = mask
        .mask()
        .map_or(0, |next| held_by_mask(next, ts, on, bounds));
    (bytes(on) / 4 * 5).saturating_add(held.max(next))
}
