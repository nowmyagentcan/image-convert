//! What usvg will build from an SVG, found before it builds it. usvg limits its nodes and nothing
//! else: it converts a mask or a marker again for each use and follows references by recursion,
//! and running out of memory or stack ends the process. So this walks the file the way usvg will,
//! adding up what each visit costs, and stops at the limits.

use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use resvg::usvg::roxmltree::{self, Document, NodeId, ParsingOptions};
use simplecss::{AttributeOperator, DeclarationTokenizer, PseudoClass, StyleSheet};

/// How deep the walk may go: usvg's stack grows with each element and more with each reference.
pub const MAX_DEPTH: u32 = 4500;
const REFERENCE_DEPTH: u32 = 16;
/// What the walk may add up to, in bytes of tree. Each estimate errs on the high side.
pub const MAX_COST: u64 = 3 << 29;
const ELEMENT_COST: u64 = 1024;
/// Per byte of an attribute: the points of a path or the bytes of a data image.
const ATTRIBUTE_COST: u64 = 4;
/// Per byte of text: the outline of a glyph and its place in the layout.
const TEXT_COST: u64 = 1024;
/// Per stop of a gradient: usvg copies them for each shape it fills in the shape's own units.
const STOP_COST: u64 = 32;

/// The elements usvg draws only when something refers to them.
const UNDRAWN: [&str; 14] = [
    "clipPath",
    "defs",
    "desc",
    "filter",
    "linearGradient",
    "marker",
    "mask",
    "metadata",
    "pattern",
    "radialGradient",
    "script",
    "style",
    "symbol",
    "title",
];
const SHAPES: [&str; 7] = [
    "circle", "ellipse", "line", "path", "polygon", "polyline", "rect",
];
const TEXT: [&str; 5] = ["a", "text", "textPath", "tref", "tspan"];
/// The elements whose `href` usvg follows to another element; on `a` or `animate` it goes nowhere.
const LINKING: [&str; 6] = ["feImage", "filter", "pattern", "textPath", "tref", "use"];

/// What building an SVG takes. A loop of references is past any depth.
#[derive(Clone, Copy, Default)]
pub struct Demand {
    pub cost: u64,
    pub depth: u32,
}

impl Demand {
    fn past_limits(&self) -> bool {
        self.cost > MAX_COST || self.depth > MAX_DEPTH
    }
}

type Element<'a> = roxmltree::Node<'a, 'a>;

/// A marker or paint, for the element naming it and all below it, as these are inherited.
#[derive(Clone, Copy)]
struct Mark<'a> {
    target: Element<'a>,
    drawn: Drawn,
}

#[derive(Clone, Copy, PartialEq)]
enum Drawn {
    Once,
    AtEachVertex,
    /// A paint, on a shape or on each run of text, which can be a single character.
    InEachSpan,
}

/// What an element refers to.
#[derive(Default)]
struct Links<'a> {
    targets: Vec<Element<'a>>,
    marks: Vec<Mark<'a>>,
}

struct Walk<'a> {
    links: Rc<HashMap<NodeId, Links<'a>>>,
    any_marks: bool,
    /// What a copy of the longest gradient costs.
    stops: u64,
    /// The references being followed, outermost first.
    following: Vec<NodeId>,
    entered: HashSet<NodeId>,
    /// Above 0 inside a copy of a pattern already built: it shares its paths and images.
    copying: u32,
    demand: Demand,
}

/// What building this SVG takes; a file that doesn't parse is left to usvg to refuse.
pub fn demand(data: &[u8]) -> Demand {
    let options = ParsingOptions {
        allow_dtd: true,
        ..ParsingOptions::default()
    };
    let doc = std::str::from_utf8(data)
        .ok()
        .and_then(|text| Document::parse_with_options(text, options).ok());
    let Some(doc) = doc else {
        return Demand::default();
    };
    let links = links(&doc);
    let gradients = doc.descendants().filter(|node| is_gradient(*node));
    let stops = gradients.map(|gradient| gradient.children().count() as u64);
    let mut walk = Walk {
        stops: stops.max().unwrap_or(0).saturating_mul(STOP_COST),
        any_marks: links.values().any(|links| !links.marks.is_empty()),
        links: Rc::new(links),
        following: Vec::new(),
        entered: HashSet::new(),
        copying: 0,
        demand: Demand::default(),
    };
    walk.visit(doc.root_element(), 1, 1, &[]);
    walk.demand
}

/// What each element refers to, by attribute, `style` and stylesheet, with its ids resolved as
/// usvg does: `use` takes the first element with an id and other references the last.
fn links<'a>(doc: &'a Document<'a>) -> HashMap<NodeId, Links<'a>> {
    let elements = || doc.descendants().filter(|node| node.is_element());
    let mut ids = HashMap::new();
    for node in elements() {
        if let Some(id) = node.attribute("id") {
            ids.entry(id).or_insert((node, node)).1 = node;
        }
    }
    let mut sheet = StyleSheet::new();
    for style in elements().filter(|node| node.has_tag_name("style")) {
        if matches!(style.attribute("type"), None | Some("text/css")) {
            sheet.parse_more(style.text().unwrap_or_default());
        }
    }
    sheet
        .rules
        .retain(|rule| rule.declarations.iter().any(|d| d.value.contains("url")));

    let mut all = HashMap::new();
    for node in elements() {
        let mut links = Links::default();
        let mut add = |name: &str, id: &str| {
            let Some(&(first, last)) = ids.get(id) else {
                return;
            };
            for &target in [first, last].iter().take(if first == last { 1 } else { 2 }) {
                let drawn = match name {
                    "marker" | "marker-mid" => Drawn::AtEachVertex,
                    "marker-start" | "marker-end" => Drawn::Once,
                    "fill" | "stroke" => Drawn::InEachSpan,
                    _ => {
                        links.targets.push(target);
                        continue;
                    }
                };
                links.marks.push(Mark { target, drawn });
            }
        };
        let mut inherits = false;
        let mut declare = |name: &str, value: &str| {
            inherits |= value == "inherit";
            urls(value).for_each(|id| add(name, id));
        };
        for attribute in node.attributes().filter(|a| a.name() != "style") {
            declare(attribute.name(), attribute.value());
        }
        for rule in sheet
            .rules
            .iter()
            .filter(|r| r.selector.matches(&Xml(node)))
        {
            rule.declarations
                .iter()
                .for_each(|d| declare(d.name, d.value));
        }
        for declaration in DeclarationTokenizer::from(node.attribute("style").unwrap_or_default()) {
            declare(declaration.name, declaration.value);
        }
        if LINKING.contains(&node.tag_name().name()) {
            let hrefs = node.attributes().filter(|a| a.name() == "href");
            hrefs
                .filter_map(|href| href.value().trim_start().strip_prefix('#'))
                .filter_map(|id| id.split(' ').next())
                .for_each(|id| add("href", id));
        }
        // `mask="inherit"` and the like take the parent's value, for as many children as it has.
        let parent = node.parent().and_then(|parent| all.get(&parent.id()));
        if let Some(parent) = parent.filter(|_| inherits) {
            let parent: &Links = parent;
            links.targets.extend(&parent.targets);
        }
        if !links.targets.is_empty() || !links.marks.is_empty() {
            all.insert(node.id(), links);
        }
    }
    all
}

/// The ids named by `url(#id)` in a value, read as usvg reads them.
fn urls(value: &str) -> impl Iterator<Item = &str> {
    value.match_indices("url").filter_map(|(at, _)| {
        let rest = value[at + 3..].trim_start().strip_prefix('(')?.trim_start();
        let quote = rest.chars().next().filter(|c| matches!(c, '"' | '\''));
        let rest = rest.trim_start_matches(['"', '\'']).trim_start();
        let id = rest.strip_prefix('#')?;
        let end = match quote {
            Some(quote) => id.find(quote),
            None => id.find([' ', ')']),
        };
        Some(id[..end.unwrap_or(id.len())].trim_end())
    })
}

fn is_gradient(node: Element) -> bool {
    node.tag_name().name().ends_with("Gradient")
}

/// Whether usvg builds `target` once and shares it between uses.
fn shared(target: Element) -> bool {
    let in_user_space = |attribute, default| {
        let units = target.attribute(attribute);
        units.map_or(default, |units| units == "userSpaceOnUse")
    };
    let (units, content) = match target.tag_name().name() {
        "clipPath" => return in_user_space("clipPathUnits", true),
        "filter" => ("filterUnits", "primitiveUnits"),
        "mask" => ("maskUnits", "maskContentUnits"),
        "pattern" => ("patternUnits", "patternContentUnits"),
        _ => return false,
    };
    in_user_space(units, false) && in_user_space(content, true)
}

impl<'a> Walk<'a> {
    /// Adds up `node` and everything it draws, `times` over.
    fn visit(&mut self, node: Element<'a>, times: u64, depth: u32, inherited: &[Mark<'a>]) {
        if self.demand.past_limits() {
            return;
        }
        self.demand.depth = self.demand.depth.max(depth);
        let attributes = node.attributes().map(|a| a.value().len() as u64).sum();
        self.charge(times, ELEMENT_COST, attributes, ATTRIBUTE_COST);

        let tag = node.tag_name().name();
        let links = Rc::clone(&self.links);
        let links = links.get(&node.id());
        let own = links.iter().flat_map(|links| &links.marks);
        let marks: Vec<_> = own.chain(inherited).copied().collect();
        for &target in links.iter().flat_map(|links| &links.targets) {
            let passed_on = if tag == "use" { &marks[..] } else { &[] };
            self.enter(target, times, depth + REFERENCE_DEPTH, passed_on);
        }
        let (is_shape, is_text) = (SHAPES.contains(&tag), TEXT.contains(&tag));
        if is_shape || is_text {
            let points = ["d", "points"].map(|a| node.attribute(a).unwrap_or_default().len());
            let vertices = (points[0] + points[1]) as u64 / 2 + 1;
            let texts = node.children().filter_map(|child| child.text());
            let spans = texts.map(|text| text.len() as u64).sum::<u64>() + 1;
            let paints = marks.iter().filter(|mark| mark.drawn == Drawn::InEachSpan);
            // A marker can draw with the paint of the shape it is on: `context-fill`.
            let paints: Vec<_> = paints.copied().collect();
            for mark in &marks {
                let (each, passed_on) = match mark.drawn {
                    Drawn::Once => (1, &paints[..]),
                    Drawn::AtEachVertex => (vertices, &paints[..]),
                    Drawn::InEachSpan if is_text => (spans, &[][..]),
                    Drawn::InEachSpan => (1, &[][..]),
                };
                let times = times.saturating_mul(each);
                self.enter(mark.target, times, depth + REFERENCE_DEPTH, passed_on);
            }
        }
        for child in node.children() {
            if child.is_element() && !UNDRAWN.contains(&child.tag_name().name()) {
                self.visit(child, times, depth + 1, &marks);
            }
            let text = child.text().filter(|_| child.is_text()).unwrap_or_default();
            self.charge(times, 0, text.trim().len() as u64, TEXT_COST);
        }
    }

    /// Follows a reference to `target`.
    fn enter(&mut self, target: Element<'a>, times: u64, depth: u32, passed_on: &[Mark<'a>]) {
        if is_gradient(target) {
            return self.charge(times, self.stops, 0, 0);
        }
        if let Some(at) = self.following.iter().rposition(|&id| id == target.id()) {
            // usvg drops a reference to itself and any loop of markers, and only a few other
            // loops: the rest it follows until the stack runs out.
            if self.following.len() - at > 1 && target.tag_name().name() != "marker" {
                self.demand.depth = u32::MAX;
            }
            return;
        }
        let again = !self.entered.insert(target.id());
        let shared = shared(target);
        if again && shared {
            return;
        }
        let copy = again && target.tag_name().name() == "pattern";
        self.copying += u32::from(copy);
        self.following.push(target.id());
        let mut marks = passed_on.to_vec();
        if self.any_marks {
            let links = Rc::clone(&self.links);
            let above = target.ancestors().skip(1);
            let above = above.filter_map(|node| links.get(&node.id()));
            marks.extend(above.flat_map(|links| &links.marks));
        }
        self.visit(target, if shared { 1 } else { times }, depth, &marks);
        self.following.pop();
        self.copying -= u32::from(copy);
    }

    /// Adds `fixed` and, unless this is a copy sharing it, `bytes` at `per_byte`, `times` over.
    fn charge(&mut self, times: u64, fixed: u64, bytes: u64, per_byte: u64) {
        let bytes = if self.copying > 0 { 0 } else { bytes };
        let once = fixed.saturating_add(bytes.saturating_mul(per_byte));
        let cost = self.demand.cost.saturating_add(times.saturating_mul(once));
        self.demand.cost = cost;
    }
}

/// An element as stylesheet selectors see it, as usvg matches them.
struct Xml<'a>(Element<'a>);

impl simplecss::Element for Xml<'_> {
    fn parent_element(&self) -> Option<Self> {
        self.0.parent_element().map(Xml)
    }

    fn prev_sibling_element(&self) -> Option<Self> {
        self.0.prev_sibling_element().map(Xml)
    }

    fn has_local_name(&self, name: &str) -> bool {
        self.0.tag_name().name() == name
    }

    fn attribute_matches(&self, name: &str, operator: AttributeOperator) -> bool {
        self.0.attribute(name).is_some_and(|v| operator.matches(v))
    }

    fn pseudo_class_matches(&self, class: PseudoClass) -> bool {
        class == PseudoClass::FirstChild && self.prev_sibling_element().is_none()
    }
}
