//! Small files that ask for far more than they hold. Each must end in a plain refusal from a
//! process that lives to give it: neither a failed allocation nor a stack overflow can be caught.
//! Runs that could really take the memory are unix-only, under `ulimit -v`.

#![cfg_attr(not(unix), allow(dead_code, unused_imports))]

mod common;

use std::fs;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::thread::sleep;
use std::time::{Duration, Instant};

use base64::prelude::*;
use common::*;
use jpeg_encoder::{ColorType as Jpeg, SamplingFactor};
use serde_json::{Value, json};

const BIN: &str = env!("CARGO_BIN_EXE_image-convert");
const CLEAN: &[&str] = &["image_too_large", "corrupt_image", "unsupported_variant"];

/// The binary, held to `megabytes` of address space where the shell can do that.
fn capped(megabytes: u32) -> Command {
    if cfg!(unix) {
        let mut command = Command::new("sh");
        let script = format!("ulimit -v {}; exec \"$0\" \"$@\"", megabytes * 1024);
        command.args(["-c", &script, BIN]);
        command
    } else {
        Command::new(BIN)
    }
}

/// A folder of its own for each call, as tests run side by side.
fn folder(test: &str) -> PathBuf {
    static CALLS: AtomicU32 = AtomicU32::new(0);
    let call = CALLS.fetch_add(1, Ordering::Relaxed);
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("limits-{call}-{test}"));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// Converts `data` as `name` in a capped process stopped after `seconds`; gives its exit status
/// and the JSON it printed.
fn convert(name: &str, data: &[u8], megabytes: u32, seconds: u64) -> (ExitStatus, String) {
    let dir = folder(name);
    fs::write(dir.join(name), data).unwrap();
    let mut child = capped(megabytes)
        .args([name, "--to", "png", "-o", "out.png", "--json"])
        .current_dir(&dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let start = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if start.elapsed() > Duration::from_secs(seconds) {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("{name} still running after {seconds} s");
        }
        sleep(Duration::from_millis(50));
    };
    let mut json = String::new();
    child
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut json)
        .unwrap();
    (status, json)
}

/// Checks that `data` is refused within 10 s with one of the codes for a bad file.
fn refused(name: &str, data: &[u8]) {
    let (status, json) = convert(name, data, 3000, 10);
    assert_eq!(status.code(), Some(1), "{name}: {status}");
    let json: Value = serde_json::from_str(&json).unwrap();
    let code = json["files"][0]["error"]["code"].as_str().unwrap();
    assert!(CLEAN.contains(&code), "{name}: {code}");
}

/// The JPEG with its SOF0 segment saying `width` × `height`.
fn sized(jpeg: &[u8], width: u16, height: u16) -> Vec<u8> {
    let at = jpeg.windows(2).position(|w| w == [0xff, 0xc0]).unwrap();
    let mut out = jpeg.to_vec();
    out[at + 5..at + 7].copy_from_slice(&height.to_be_bytes());
    out[at + 7..at + 9].copy_from_slice(&width.to_be_bytes());
    out
}

fn interleaved() -> Vec<u8> {
    jpeg(64, 64, Jpeg::Rgb, &gradient(64, 64, 3), |_| {})
}

fn separate_scans() -> Vec<u8> {
    jpeg(64, 64, Jpeg::Rgb, &gradient(64, 64, 3), |e| {
        e.set_sampling_factor(SamplingFactor::F_2_2);
        e.set_optimized_huffman_tables(true);
    })
}

/// A JPEG whose first frame header says 64×64 to a reader that takes `marker` to have no length;
/// a reader that gives it one skips that header and meets a 65,500×65,500 frame.
fn second_frame(marker: u8) -> Vec<u8> {
    let whole = interleaved();
    let big = sized(&whole, 65500, 65500);
    let sof = whole.windows(2).position(|w| w == [0xff, 0xc0]).unwrap();
    let length = usize::from(u16::from_be_bytes([whole[sof + 2], whole[sof + 3]]));
    let small = &whole[sof..sof + 2 + length];
    let mut out = vec![0xff, 0xd8, 0xff, marker];
    out.extend(small);
    // The other reader lands where the length FF C0, read at offset 4, sends it.
    let land = 4 + 0xffc0;
    let filler = land - out.len();
    out.extend([0xff, 0xfe]);
    out.extend(((filler - 2) as u16).to_be_bytes());
    out.resize(land, b'x');
    out.extend(&big[2..]);
    out
}

/// A JPEG with a 65,500×65,500 image behind `FF 00` and a small one-scan-per-component image
/// after it: the first for a reader that scans for markers, the second for one that skips.
fn behind_stuffing() -> Vec<u8> {
    let big = sized(&interleaved(), 65500, 65500);
    let mut out = vec![0xff, 0xd8, 0xff, 0x00];
    out.extend((big.len() as u16).to_be_bytes());
    out.extend(&big[2..]);
    out.extend(&separate_scans()[2..]);
    out
}

/// A 64-wide TIFF page of `count` strips, all the same bytes, `rows` rows each.
fn strips(strip: &[u8], rows: u32, count: u32) -> Vec<u8> {
    let short = |tag, n| (tag, 3, 1, V::N(n));
    let (offsets, lengths): (Vec<u8>, Vec<u8>) = (
        8u32.to_le_bytes().repeat(count as usize),
        (strip.len() as u32).to_le_bytes().repeat(count as usize),
    );
    let list = |tag, blob, value| {
        let value = if count == 1 {
            V::N(value)
        } else {
            V::Blob(blob)
        };
        (tag, 4, count, value)
    };
    let ifd = [
        short(256, 64),
        short(257, rows * count),
        (258, 3, 3, V::Blob(1)),
        short(259, 7),
        short(262, 6),
        list(273, 2, 8),
        short(277, 3),
        short(278, rows),
        list(279, 3, strip.len() as u32),
        (530, 3, 2, V::N(1 | 1 << 16)),
    ];
    raw_tiff(
        42,
        &[strip, &[8, 0, 8, 0, 8, 0], &offsets, &lengths],
        &ifd,
        &[],
    )
}

/// A 64×64 page whose StripOffsets tag comes twice: `first`, then `last`, which a TIFF reader
/// that takes the last of a repeated tag reads.
fn repeated_strip_tag(first: &[u8], last: &[u8]) -> Vec<u8> {
    let short = |tag, n| (tag, 3, 1, V::N(n));
    let ifd = [
        short(256, 64),
        short(257, 64),
        (258, 3, 3, V::Blob(2)),
        short(259, 7),
        short(262, 6),
        (273, 4, 1, V::Blob(0)),
        (273, 4, 1, V::Blob(1)),
        short(277, 3),
        short(278, 64),
        (279, 4, 1, V::N(first.len().max(last.len()) as u32)),
        (530, 3, 2, V::N(1 | 1 << 16)),
    ];
    raw_tiff(42, &[first, last, &[8, 0, 8, 0, 8, 0]], &ifd, &[])
}

/// An ICO whose one BMP entry has a 12-byte core header saying `width` × `height`.
fn core_header_ico(width: u16, height: u16) -> Vec<u8> {
    let mut image = 12u32.to_le_bytes().to_vec();
    image.extend([width.to_le_bytes(), height.to_le_bytes(), [1, 0], [1, 0]].concat());
    image.resize(76, 0);
    let mut ico = vec![0, 0, 1, 0, 1, 0, 0, 0, 0, 0, 1, 0, 1, 0];
    ico.extend((image.len() as u32).to_le_bytes());
    ico.extend(22u32.to_le_bytes());
    ico.extend(image);
    ico
}

const SVG: &str = "<svg xmlns='http://www.w3.org/2000/svg'";

fn huge_pattern() -> String {
    format!(
        "{SVG} width='64' height='64'><pattern id='p' patternUnits='userSpaceOnUse' \
        width='1000000' height='1000000'><rect width='9' height='9'/></pattern>
        <rect width='64' height='64' fill='url(#p)'/></svg>"
    )
}

fn huge_layer() -> String {
    format!(
        "{SVG} width='8192' height='8192'><g opacity='.5'><rect x='-100000' y='-100000' \
        width='1000000' height='1000000'/></g></svg>"
    )
}

fn nested_layers() -> String {
    let (open, close) = ("<g opacity='.5'>".repeat(200), "</g>".repeat(200));
    format!(
        "{SVG} width='8192' height='8192'>{open}<rect width='8192' height='8192'/>{close}</svg>"
    )
}

/// An SVG drawing `svg` through a data image.
fn as_data_image(svg: &str) -> String {
    let data = BASE64_STANDARD.encode(svg);
    format!(
        "{SVG} xmlns:x='http://www.w3.org/1999/xlink' width='64' height='64'>\
        <image width='64' height='64' x:href='data:image/svg+xml;base64,{data}'/></svg>"
    )
}

/// `levels` masks, each drawing the one before twice: 2^levels renders of the first.
fn doubling_masks(levels: u32) -> String {
    let mut doc =
        format!("{SVG} width='64' height='64'><mask id='m0'><rect width='9' height='9'/></mask>");
    for i in 1..levels {
        let use_it = format!("<rect width='9' height='9' mask='url(#m{})'/>", i - 1);
        doc += &format!("<mask id='m{i}'>{use_it}{use_it}</mask>");
    }
    doc + &format!(
        "<rect width='9' height='9' mask='url(#m{})'/></svg>",
        levels - 1
    )
}

/// [`doubling_masks`] with each reference in a stylesheet.
fn doubling_masks_by_class(levels: u32) -> String {
    let rules: String = (0..levels)
        .map(|i| format!(".l{i}{{mask:url(#m{i})}}"))
        .collect();
    let mut doc = format!(
        "{SVG} width='64' height='64'><style>{rules}</style>\
        <mask id='m0'><rect width='9' height='9'/></mask>"
    );
    for i in 1..levels {
        let use_it = format!("<rect width='9' height='9' class='l{}'/>", i - 1);
        doc += &format!("<mask id='m{i}'>{use_it}{use_it}</mask>");
    }
    doc + &format!("<rect width='9' height='9' class='l{}'/></svg>", levels - 1)
}

/// A chain of `links` `element`s, each using the one before through `attribute`, and a rect at
/// the end of it.
fn chain(element: &str, attribute: &str, links: u32) -> String {
    let mut doc = format!("{SVG} width='64' height='64'><{element} id='c0'><rect/></{element}>");
    for i in 1..links {
        let before = i - 1;
        doc += &format!("<{element} id='c{i}' {attribute}='url(#c{before})'><rect/></{element}>");
    }
    let last = links - 1;
    doc + &format!("<rect width='9' height='9' {attribute}='url(#c{last})'/></svg>")
}

fn pattern_chain(links: u32) -> String {
    let tile = "patternUnits='userSpaceOnUse' width='9' height='9'";
    let mut doc = format!(
        "{SVG} width='64' height='64'><pattern id='c0' {tile}><rect width='9' height='9'/></pattern>"
    );
    for i in 1..links {
        let before = i - 1;
        doc += &format!(
            "<pattern id='c{i}' {tile}><rect width='9' height='9' fill='url(#c{before})'/></pattern>"
        );
    }
    let last = links - 1;
    doc + &format!("<rect width='9' height='9' fill='url(#c{last})'/></svg>")
}

/// `links` filters, each drawing through feImage a group that has the filter before.
fn filter_chain(links: u32) -> String {
    let group =
        |i: u32, filter: &str| format!("<g id='g{i}' {filter}><rect width='9' height='9'/></g>");
    let mut doc = format!(
        "{SVG} xmlns:x='http://www.w3.org/1999/xlink' width='64' height='64'>{}",
        group(0, "")
    );
    for i in 1..links {
        let before = i - 1;
        doc += &format!("<filter id='f{i}'><feImage x:href='#g{before}'/></filter>");
        doc += &group(i, &format!("filter='url(#f{i})'"));
    }
    doc + "</svg>"
}

/// `references` uses of one entity of 3,000 bytes: 4.8 GB of text at 1.6 million.
fn flat_entity(references: usize, in_attribute: bool) -> String {
    let (value, uses) = ("A".repeat(3000), "&a;".repeat(references));
    let body = if in_attribute {
        format!("<rect id='{uses}' width='9' height='9'/>")
    } else {
        format!("<desc>{uses}</desc><rect width='9' height='9'/>")
    };
    format!("<!DOCTYPE svg [<!ENTITY a '{value}'>]>{SVG} width='64' height='64'>{body}</svg>")
}

/// A 14,000² RGBA PNG of 16-bit zeros: 196 MP, inside every limit, about 1.5 MB.
fn legal_png16() -> Vec<u8> {
    let (side, row) = (14_000, vec![0; 14_000 * 8]);
    let mut out = Vec::new();
    let mut encoder = png::Encoder::new(&mut out, side, side);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Sixteen);
    let mut writer = encoder.write_header().unwrap();
    let mut stream = writer.stream_writer().unwrap();
    (0..side).for_each(|_| stream.write_all(&row).unwrap());
    stream.finish().unwrap();
    writer.finish().unwrap();
    out
}

#[test]
#[cfg(unix)]
fn a_jpeg_strip_on_the_fallback_decoder_is_held_to_the_page() {
    refused(
        "a.tif",
        &strips(&sized(&separate_scans(), 65500, 65500), 64, 1),
    );
}

#[test]
fn a_pattern_tile_too_big_to_allocate_is_refused() {
    refused("a.svg", huge_pattern().as_bytes());
    refused("nested.svg", as_data_image(&huge_pattern()).as_bytes());
}

#[test]
#[cfg(unix)]
fn svg_layers_are_held_to_a_budget() {
    for (name, doc) in [("a.svg", huge_layer()), ("b.svg", nested_layers())] {
        let (status, _) = convert(name, doc.as_bytes(), 3000, 60);
        assert!(matches!(status.code(), Some(0 | 1)), "{name}: {status}");
    }
}

#[test]
#[cfg(unix)]
fn a_filter_on_a_turned_group_is_held_to_its_turned_layer() {
    let doc = format!(
        "{SVG} width='2000' height='2000'><filter id='f' filterUnits='userSpaceOnUse' x='-9000' \
        y='-9000' width='30000' height='30000'><feGaussianBlur stdDeviation='1'/></filter>\
        <g transform='rotate(30)' filter='url(#f)'><rect x='-100000' y='-100000' width='300000' \
        height='300000'/></g></svg>"
    );
    refused("a.svg", doc.as_bytes());
}

#[test]
fn a_layer_within_the_budget_still_renders() {
    let doc = huge_layer().replace("8192", "2000");
    let (status, _) = convert("a.svg", doc.as_bytes(), 3000, 60);
    assert!(status.success(), "{status}");
}

#[test]
fn a_mask_drawn_twice_at_every_level_is_refused() {
    refused("a.svg", doubling_masks(40).as_bytes());
    refused("css.svg", doubling_masks_by_class(40).as_bytes());
    refused("nested.svg", as_data_image(&doubling_masks(40)).as_bytes());
    for (name, doc) in [
        ("few.svg", doubling_masks(6)),
        ("few-css.svg", doubling_masks_by_class(6)),
    ] {
        let (status, _) = convert(name, doc.as_bytes(), 3000, 10);
        assert!(status.success(), "{name}: {status}");
    }
}

#[test]
#[cfg(unix)]
fn a_jpeg_is_held_to_the_size_its_decoder_reads() {
    for (name, marker) in [("tem.jpg", 0x01), ("rst.jpg", 0xd0), ("soi.jpg", 0xd8)] {
        refused(name, &second_frame(marker));
    }
    refused("stuffed.jpg", &behind_stuffing());
}

#[test]
#[cfg(unix)]
fn an_ico_with_a_core_header_is_held_to_its_size() {
    refused("a.ico", &core_header_ico(0xffff, 0xffff));
    refused("b.ico", &core_header_ico(0xfffe, 0xffff));
}

#[test]
fn a_jpeg_strip_on_the_tiff_crates_path_is_held_to_the_page() {
    let big = sized(&interleaved(), 16000, 16000);
    refused("one.tif", &strips(&big, 64, 1));
    refused("many.tif", &strips(&big, 1, 100));
    let smaller = sized(&interleaved(), 4000, 4000);
    refused("listed.tif", &strips(&smaller, 1, 60_000));
}

#[test]
fn a_repeated_strip_tag_cant_hide_a_strip_from_the_page() {
    let (good, big) = (interleaved(), sized(&interleaved(), 16000, 16000));
    refused("last.tif", &repeated_strip_tag(&good, &big));
    let (status, _) = convert("twin.tif", &repeated_strip_tag(&big, &good), 3000, 10);
    assert!(status.success(), "{status}");
}

#[test]
fn a_right_sized_strip_listed_again_and_again_is_refused() {
    refused("a.tif", &strips(&interleaved(), 64, 1000));
    let (status, _) = convert("twin.tif", &strips(&interleaved(), 64, 1), 3000, 10);
    assert!(status.success(), "{status}");
}

#[test]
#[cfg(unix)]
fn one_entity_cant_expand_past_the_limits() {
    refused("text.svg", flat_entity(1_600_000, false).as_bytes());
    refused("attribute.svg", flat_entity(1_600_000, true).as_bytes());
    let (status, _) = convert("few.svg", flat_entity(500, false).as_bytes(), 3000, 10);
    assert!(status.success());
}

#[test]
#[cfg(unix)]
fn no_comment_or_quote_hides_an_entity_from_the_count() {
    let doc = flat_entity(1_600_000, false);
    for (before, after) in [
        ("<!-- <!ENTITY b ' -->", "<!-- ' -->"),
        ("<!ATTLIST svg x CDATA '<!--'>", "<!-- -->"),
    ] {
        let hidden = doc
            .replacen("<!ENTITY", &format!("{before}<!ENTITY"), 1)
            .replacen("]>", &format!("{after}]>"), 1);
        refused("a.svg", hidden.as_bytes());
    }
    refused("nested.svg", as_data_image(&doc).as_bytes());
}

#[test]
fn svg_reference_chains_cant_overflow_the_stack() {
    refused(
        "clip.svg",
        chain("clipPath", "clip-path", 200_000).as_bytes(),
    );
    refused("mask.svg", chain("mask", "mask", 200_000).as_bytes());
    refused("pattern.svg", pattern_chain(200_000).as_bytes());
    let (status, _) = convert("few.svg", chain("mask", "mask", 100).as_bytes(), 3000, 10);
    assert!(status.success());
}

#[test]
fn a_namespace_prefix_doesnt_hide_a_chain() {
    let doc = chain("s:clipPath", "clip-path", 200_000);
    let doc = doc.replacen("<svg ", "<svg xmlns:s='http://www.w3.org/2000/svg' ", 1);
    refused("a.svg", doc.as_bytes());
}

#[test]
fn a_chain_in_a_stylesheet_is_a_chain() {
    let rules: String = (0..300)
        .map(|i| format!("#c{}{{clip-path:url(#c{i})}}", i + 1))
        .collect();
    let chain: String = (0..=300)
        .map(|i| format!("<clipPath id='c{i}'><rect/></clipPath>"))
        .collect();
    let doc = format!(
        "{SVG} width='64' height='64'><style>{rules}</style>{chain}\
        <rect width='9' height='9' clip-path='url(#c300)'/></svg>"
    );
    refused("a.svg", doc.as_bytes());
}

#[test]
fn chains_within_the_limit_still_render() {
    for (name, doc) in [
        ("clip.svg", chain("clipPath", "clip-path", 256)),
        ("mask.svg", chain("mask", "mask", 256)),
        ("pattern.svg", pattern_chain(256)),
        ("filter.svg", filter_chain(128)),
    ] {
        let (status, _) = convert(name, doc.as_bytes(), 3000, 30);
        assert!(status.success(), "{name}: {status}");
    }
}

#[test]
fn chains_in_data_images_add_up_only_when_nested() {
    let image = |svg: &str| {
        let data = BASE64_STANDARD.encode(svg);
        format!("<image width='64' height='64' href='data:image/svg+xml;base64,{data}'/>")
    };
    let root = |body: String| format!("{SVG} width='64' height='64'>{body}</svg>");
    let links = chain("mask", "mask", 200);
    let side_by_side = root(image(&links).repeat(3));
    let (status, _) = convert("side.svg", side_by_side.as_bytes(), 3000, 30);
    assert!(status.success(), "{status}");
    let with_image = links.replace("</svg>", &format!("{}</svg>", image(&links)));
    refused("nested.svg", root(image(&with_image)).as_bytes());
}

#[test]
fn many_clip_paths_side_by_side_still_render() {
    let paths: String = (0..30_000)
        .map(|i| format!("<clipPath id='c{i}'><rect width='9' height='9'/></clipPath><rect width='9' height='9' clip-path='url(#c{i})'/>"))
        .collect();
    let doc = format!("{SVG} width='64' height='64'>{paths}</svg>");
    let (status, _) = convert("a.svg", doc.as_bytes(), 3000, 60);
    assert!(status.success(), "{status}");
}

#[test]
#[cfg(unix)]
#[ignore = "known bug: IC38"]
fn a_legal_16_bit_png_stays_under_2_gib() {
    let (status, _) = convert("a.png", &legal_png16(), 2200, 60);
    assert!(status.success(), "{status}");
}

#[test]
#[cfg(unix)]
#[ignore = "known bug: IC32, IC33, IC34, IC36, IC37"]
fn the_mcp_server_outlives_every_such_file() {
    let dir = folder("mcp");
    let files: [(&str, Vec<u8>); 5] = [
        ("pattern.svg", huge_pattern().into_bytes()),
        ("frame.jpg", second_frame(0x01)),
        ("core.ico", core_header_ico(0xffff, 0xffff)),
        ("entity.svg", flat_entity(1_600_000, false).into_bytes()),
        (
            "chain.svg",
            chain("clipPath", "clip-path", 200_000).into_bytes(),
        ),
    ];
    let mut lines = vec![
        json!({"jsonrpc": "2.0", "id": 0, "method": "initialize", "params": {
            "protocolVersion": "2025-11-25", "capabilities": {},
            "clientInfo": {"name": "test", "version": "1"}}}),
        json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
    ];
    for (id, (name, data)) in files.iter().enumerate() {
        let path = dir.join(name);
        fs::write(&path, data).unwrap();
        lines.push(
            json!({"jsonrpc": "2.0", "id": id + 1, "method": "tools/call", "params": {
            "name": "view_image", "arguments": {"path": path}}}),
        );
    }
    let input: String = lines.iter().map(|line| format!("{line}\n")).collect();
    let mut child = capped(3000)
        .arg("mcp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    let replies = String::from_utf8(output.stdout).unwrap();
    assert_eq!(output.status.code(), Some(0), "{replies}");
    assert_eq!(replies.lines().count(), files.len() + 1);
    for reply in replies.lines().skip(1) {
        let reply: Value = serde_json::from_str(reply).unwrap();
        assert_eq!(reply["result"]["isError"], true, "{reply}");
    }
}
