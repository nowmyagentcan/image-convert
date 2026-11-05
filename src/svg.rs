//! SVG through resvg with no file or network access, `.svgz`/`.emz`/`.wmz` inflated under a cap,
//! and text drawn with the system's fonts or the bundled Noto Sans.

use std::collections::HashMap;
use std::io::Read;
use std::path::Path;
use std::sync::{Arc, Mutex};

use flate2::read::GzDecoder;
use resvg::tiny_skia::{Pixmap, Transform};
use resvg::usvg::fontdb::Source;
use resvg::usvg::roxmltree::{self, Document, NodeId, ParsingOptions};
use resvg::usvg::{ImageHrefResolver, ImageKind, Options as Parse, Tree};

use crate::color::to_srgb;
use crate::detect::{Kind, detect, find};
use crate::encode::encode;
use crate::error::{Error, Warning};
use crate::{Format, Layout, Options, Pixels, Samples, jpeg, resize};

static NOTO_SANS: &[u8] = include_bytes!("../assets/fonts/NotoSans-Regular.ttf");
const MAX_INFLATED: u64 = 64 << 20;
const MAX_NESTED_SVGS: u32 = 1000;
/// How long a chain of references can be: usvg follows them by recursion, and each link of some
/// chains (a filter's feImage) costs more than the last.
const MAX_CHAIN: u32 = 256;
/// How many references the bodies of referenced elements can come to, each counted as often as
/// it is reached: usvg converts a mask again for each use, so two uses at every level double.
const MAX_EXPANDED: u64 = 100_000;

/// What the images an SVG embeds add up to, over every use of each at every depth.
#[derive(Default)]
struct Embedded {
    pixels: u64,
    svgs: u32,
    svg_bytes: u64,
    /// Every image's expansion, and the chains of the images being parsed: parsing an image
    /// happens inside the parsing of the file around it.
    links: Links,
    error: Option<Error>,
}

/// What following an SVG's references takes.
#[derive(Clone, Copy, Default)]
struct Links {
    expanded: u64,
    chain: u32,
}

impl Embedded {
    fn link(&mut self, links: Links) -> Result<(), Error> {
        self.links.expanded = self.links.expanded.saturating_add(links.expanded);
        self.links.chain = self.links.chain.saturating_add(links.chain);
        if self.links.expanded > MAX_EXPANDED || self.links.chain > MAX_CHAIN {
            let variant = "references nested over 256 deep or expanding past 100,000";
            return Err(Error::unsupported_variant("SVG", variant));
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
    budget.link(refuse(data)?)?;
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

/// Refuses what parsing has no limit for, UTF-16 text and entities that expand other entities or
/// expand past 64 MiB, and gives what its references take, for [`Embedded::link`].
fn refuse(data: &[u8]) -> Result<Links, Error> {
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
    Ok(links(data))
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
            let links = refuse(svg)?;
            let mut b = budget.lock().unwrap();
            b.link(links)?;
            b.svgs += 1;
            b.svg_bytes += svg.len() as u64;
            if b.svgs > MAX_NESTED_SVGS || b.svg_bytes > MAX_INFLATED {
                let what = "hold more than 1,000 SVG images, or more than 64 MiB of them";
                return Err(Error::embeds_too_much(what));
            }
            drop(b);
            let tree = Tree::from_data_nested(svg, parse).ok();
            budget.lock().unwrap().links.chain -= links.chain;
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

/// What following the references in an SVG takes; a file that doesn't parse is left to usvg.
fn links(data: &[u8]) -> Links {
    let options = ParsingOptions {
        allow_dtd: true,
        ..ParsingOptions::default()
    };
    let doc = std::str::from_utf8(data)
        .ok()
        .and_then(|text| Document::parse_with_options(text, options).ok());
    let Some(doc) = doc else {
        return Links::default();
    };
    let mut css: HashMap<&str, Vec<&str>> = HashMap::new();
    let elements = || doc.descendants().filter(|node| node.is_element());
    for style in elements().filter(|node| node.tag_name().name() == "style") {
        for rule in style
            .children()
            .filter_map(|text| text.text())
            .flat_map(|t| t.split('}'))
        {
            let Some((selectors, body)) = rule.split_once('{') else {
                continue;
            };
            for selector in selectors.split(',') {
                css.entry(css_class(selector))
                    .or_default()
                    .extend(urls(body));
            }
        }
    }
    let ids = elements().filter_map(|node| Some((node.attribute("id")?, node)));
    let mut walk = References {
        ids: ids.collect(),
        css,
        seen: HashMap::new(),
    };
    walk.follow(doc.root(), 0)
}

/// The class a CSS selector's last part names; empty, for any element, when it names none.
fn css_class(selector: &str) -> &str {
    let last = selector.rsplit(|c: char| c.is_whitespace() || matches!(c, '>' | '+' | '~'));
    let last = last.into_iter().find(|part| !part.is_empty());
    let plain = |class: &&str| {
        class
            .chars()
            .all(|c| c.is_alphanumeric() || "-_".contains(c))
    };
    let class = last.and_then(|part| part.rsplit_once('.'));
    class.map(|(_, class)| class).filter(plain).unwrap_or("")
}

/// The ids named by `url(#id)` in a value.
fn urls(value: &str) -> impl Iterator<Item = &str> {
    value.split("url(").skip(1).filter_map(|rest| {
        let (target, _) = rest.split_once(')')?;
        target.trim().trim_matches(['"', '\'']).strip_prefix('#')
    })
}

type Element<'a> = roxmltree::Node<'a, 'a>;

/// A walk of an SVG's references, with the rules of its stylesheets by class.
struct References<'a> {
    ids: HashMap<&'a str, Element<'a>>,
    css: HashMap<&'a str, Vec<&'a str>>,
    seen: HashMap<NodeId, Links>,
}

impl<'a> References<'a> {
    /// What `element`'s own references name, in attributes and through stylesheets. A gradient
    /// holds nothing to follow, and usvg shares it.
    fn targets(&self, element: Element<'a>) -> Vec<Element<'a>> {
        let classes = element.attribute("class").unwrap_or_default();
        let rules = classes.split_ascii_whitespace().chain([""]);
        let rules = rules
            .filter_map(|class| self.css.get(class))
            .flatten()
            .copied();
        let attributes = element.attributes().flat_map(|attribute| {
            let value = attribute.value();
            let href = value
                .strip_prefix('#')
                .filter(|_| attribute.name() == "href");
            urls(value).chain(href)
        });
        rules
            .chain(attributes)
            .filter_map(|id| self.ids.get(id).copied())
            .filter(|target| !target.tag_name().name().ends_with("Gradient"))
            .collect()
    }

    /// What following everything inside `element` takes, stopping once it is past the limits.
    /// A reference back to an element being followed counts for nothing: usvg drops those.
    fn follow(&mut self, element: Element<'a>, depth: u32) -> Links {
        if let Some(&known) = self.seen.get(&element.id()) {
            return known;
        }
        let mut links = Links::default();
        if depth > MAX_CHAIN {
            links.chain = depth;
            return links;
        }
        self.seen.insert(element.id(), links);
        'all: for node in element.descendants().filter(|node| node.is_element()) {
            for target in self.targets(node) {
                let inner = self.follow(target, depth + 1);
                let reached = u64::from(depth > 0);
                links.expanded = links.expanded.saturating_add(inner.expanded + reached);
                links.chain = links.chain.max(inner.chain.saturating_add(1));
                if links.expanded > MAX_EXPANDED || links.chain > MAX_CHAIN {
                    break 'all;
                }
            }
        }
        self.seen.insert(element.id(), links);
        links
    }
}
