//! Test inputs written in code: images in every variant the decoders take, built byte by byte
//! where no encoder writes them.
#![allow(dead_code)]

use std::borrow::Cow;

/// Red, green, blue, white, black, yellow: exact in every bit depth and palette.
pub const COLORS: [[u8; 3]; 6] = [
    [255, 0, 0],
    [0, 255, 0],
    [0, 0, 255],
    [255, 255, 255],
    [0, 0, 0],
    [255, 255, 0],
];

/// A smooth gradient with `channels` samples per pixel (down the rows, then across), kind to
/// lossy codecs.
pub fn gradient(width: u32, height: u32, channels: usize) -> Vec<u8> {
    let mut px = Vec::new();
    for y in 0..height {
        for x in 0..width {
            for c in 0..channels {
                let (at, len) = if c % 2 == 0 { (y, height) } else { (x, width) };
                px.push((at * 255 / (len - 1).max(1)) as u8);
            }
        }
    }
    px
}

/// A PNG from the png crate; `setup` adds palettes, profiles or EXIF to its header.
pub fn png(
    width: u32,
    height: u32,
    color: png::ColorType,
    depth: png::BitDepth,
    data: &[u8],
    setup: impl FnOnce(&mut png::Info),
) -> Vec<u8> {
    let mut info = png::Info::with_size(width, height);
    info.color_type = color;
    info.bit_depth = depth;
    setup(&mut info);
    let mut out = Vec::new();
    let mut writer = png::Encoder::with_info(&mut out, info)
        .unwrap()
        .write_header()
        .unwrap();
    writer.write_image_data(data).unwrap();
    writer.finish().unwrap();
    out
}

/// An animated RGB PNG whose default image is its first frame.
pub fn apng(width: u32, height: u32, frames: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut encoder = png::Encoder::new(&mut out, width, height);
    encoder.set_color(png::ColorType::Rgb);
    encoder.set_animated(frames.len() as u32, 0).unwrap();
    let mut writer = encoder.write_header().unwrap();
    for frame in frames {
        writer.write_image_data(frame).unwrap();
    }
    writer.finish().unwrap();
    out
}

/// A GIF with a `screen` and frames of (left, top, width, height, palette indices) using `COLORS`.
pub fn gif(
    screen: (u16, u16),
    transparent: Option<u8>,
    frames: &[(u16, u16, u16, u16, Vec<u8>)],
) -> Vec<u8> {
    let palette: Vec<u8> = COLORS
        .iter()
        .flatten()
        .copied()
        .chain([128, 128, 128, 64, 64, 64])
        .collect();
    let mut out = Vec::new();
    let mut encoder = gif::Encoder::new(&mut out, screen.0, screen.1, &palette).unwrap();
    for (left, top, width, height, indices) in frames {
        let frame = gif::Frame {
            left: *left,
            top: *top,
            width: *width,
            height: *height,
            transparent,
            buffer: Cow::Borrowed(indices),
            ..gif::Frame::default()
        };
        encoder.write_frame(&frame).unwrap();
    }
    drop(encoder);
    out
}

/// Rows padded to 4 bytes, bottom row first unless `top_down`, as BMP stores them.
pub fn bmp_rows(rows: &[Vec<u8>], top_down: bool) -> Vec<u8> {
    let mut out = Vec::new();
    let order: Vec<&Vec<u8>> = if top_down {
        rows.iter().collect()
    } else {
        rows.iter().rev().collect()
    };
    for row in order {
        out.extend(row);
        out.resize(out.len().next_multiple_of(4), 0);
    }
    out
}

/// A BMP with a `header`-byte info header (12 core, 40, 56, 108 V4, 124 V5). Masks (R, G, B, A)
/// go inside headers from 56 bytes, or after a 40-byte one when `compression` is 3.
pub fn bmp(
    header: u32,
    size: (i32, i32),
    bpp: u16,
    compression: u32,
    masks: [u32; 4],
    palette: &[[u8; 3]],
    pixels: &[u8],
) -> Vec<u8> {
    let mut info = header.to_le_bytes().to_vec();
    if header == 12 {
        info.extend((size.0 as u16).to_le_bytes());
        info.extend((size.1 as u16).to_le_bytes());
        info.extend(1u16.to_le_bytes());
        info.extend(bpp.to_le_bytes());
    } else {
        info.extend(size.0.to_le_bytes());
        info.extend(size.1.to_le_bytes());
        info.extend(1u16.to_le_bytes());
        info.extend(bpp.to_le_bytes());
        info.extend(compression.to_le_bytes());
        info.extend((pixels.len() as u32).to_le_bytes());
        info.extend([0; 8]);
        info.extend((palette.len() as u32).to_le_bytes());
        info.extend([0; 4]);
        if header >= 56 || compression == 3 {
            masks
                .iter()
                .take(if header == 40 { 3 } else { 4 })
                .for_each(|m| info.extend(m.to_le_bytes()));
        }
        info.resize(info.len().max(header as usize), 0);
    }
    for [r, g, b] in palette {
        info.extend([*b, *g, *r]);
        if header != 12 {
            info.push(0);
        }
    }
    // A core header has no colour count: its palette always has 2^bpp entries.
    if header == 12 && bpp <= 8 {
        info.resize(12 + 3 * (1 << bpp), 0);
    }
    let offset = 14 + info.len() as u32;
    let mut out = b"BM".to_vec();
    out.extend((offset + pixels.len() as u32).to_le_bytes());
    out.extend([0; 4]);
    out.extend(offset.to_le_bytes());
    out.extend(info);
    out.extend(pixels);
    out
}

/// An ICO from (width, height, declared bit count, image data) entries.
pub fn ico(entries: &[(u8, u8, u16, Vec<u8>)]) -> Vec<u8> {
    let mut out = vec![0, 0, 1, 0];
    out.extend((entries.len() as u16).to_le_bytes());
    let mut offset = 6 + 16 * entries.len() as u32;
    for (width, height, bits, data) in entries {
        out.extend([*width, *height, 0, 0, 1, 0]);
        out.extend(bits.to_le_bytes());
        out.extend((data.len() as u32).to_le_bytes());
        out.extend(offset.to_le_bytes());
        offset += data.len() as u32;
    }
    entries.iter().for_each(|e| out.extend(&e.3));
    out
}

/// A RIFF chunk, padded to an even length.
pub fn chunk(id: &[u8; 4], data: &[u8]) -> Vec<u8> {
    let mut out = id.to_vec();
    out.extend((data.len() as u32).to_le_bytes());
    out.extend(data);
    if data.len() % 2 == 1 {
        out.push(0);
    }
    out
}

pub fn riff_webp(chunks: &[Vec<u8>]) -> Vec<u8> {
    let body: Vec<u8> = chunks.concat();
    let mut out = b"RIFF".to_vec();
    out.extend((4 + body.len() as u32).to_le_bytes());
    out.extend(b"WEBP");
    out.extend(body);
    out
}

pub fn u24(v: u32) -> [u8; 3] {
    let b = v.to_le_bytes();
    [b[0], b[1], b[2]]
}

/// A lossless RGBA WebP from image's encoder.
pub fn webp_lossless(width: u32, height: u32, rgba: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    image::codecs::webp::WebPEncoder::new_lossless(&mut out)
        .encode(rgba, width, height, image::ExtendedColorType::Rgba8)
        .unwrap();
    out
}

/// A JPEG from jpeg-encoder; `setup` picks sampling, progressive mode or APP segments.
pub fn jpeg(
    width: u16,
    height: u16,
    color: jpeg_encoder::ColorType,
    pixels: &[u8],
    setup: impl FnOnce(&mut jpeg_encoder::Encoder<&mut Vec<u8>>),
) -> Vec<u8> {
    let mut out = Vec::new();
    let mut encoder = jpeg_encoder::Encoder::new(&mut out, 95);
    setup(&mut encoder);
    encoder.encode(pixels, width, height, color).unwrap();
    out
}

/// A little-endian EXIF block (TIFF header on) holding only an Orientation tag.
pub fn exif_orientation(orientation: u16) -> Vec<u8> {
    let mut exif = b"II*\0\x08\0\0\0\x01\0\x12\x01\x03\0\x01\0\0\0".to_vec();
    exif.extend(orientation.to_le_bytes());
    exif.extend([0; 6]);
    exif
}

/// A stand-in ICC profile: only its bytes are compared.
pub fn profile(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i * 7 % 251) as u8).collect()
}

/// A fixture written by tests/fixtures/make.sh.
pub fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(format!(
        "{}/tests/fixtures/{name}",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap()
}
