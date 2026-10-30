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

/// The mean absolute difference of two equal-length sample buffers.
pub fn mean_diff(a: &[u8], b: &[u8]) -> f64 {
    assert_eq!(a.len(), b.len());
    let sum: f64 = a
        .iter()
        .zip(b)
        .map(|(x, y)| f64::from(x.abs_diff(*y)))
        .sum();
    sum / a.len() as f64
}

/// A fixture written by tests/fixtures/make.sh.
pub fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(format!(
        "{}/tests/fixtures/{name}",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap()
}

/// A TIFF of 4-row strips from the tiff crate's encoder (little-endian): `pages` copies of the
/// image, each with `tags` (SHORT values) and the ICC profile `icc` added.
#[allow(clippy::too_many_arguments)]
pub fn tiff<C: tiff::encoder::colortype::ColorType>(
    width: u32,
    height: u32,
    data: &[C::Inner],
    compression: tiff::encoder::Compression,
    predictor: tiff::tags::Predictor,
    tags: &[(u16, u16)],
    icc: Option<&[u8]>,
    pages: usize,
) -> Vec<u8>
where
    [C::Inner]: tiff::encoder::TiffValue,
{
    let mut out = std::io::Cursor::new(Vec::new());
    let mut encoder = tiff::encoder::TiffEncoder::new(&mut out)
        .unwrap()
        .with_compression(compression)
        .with_predictor(predictor);
    for _ in 0..pages {
        let mut image = encoder.new_image::<C>(width, height).unwrap();
        image.rows_per_strip(4).unwrap();
        for &(tag, value) in tags {
            let tag = tiff::tags::Tag::from_u16_exhaustive(tag);
            image.encoder().write_tag(tag, value).unwrap();
        }
        if let Some(icc) = icc {
            let tag = tiff::tags::Tag::from_u16_exhaustive(34675);
            image.encoder().write_tag(tag, icc).unwrap();
        }
        image.write_data(data).unwrap();
    }
    out.into_inner()
}

/// An ISO-BMFF box.
pub fn bx(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut out = ((8 + body.len()) as u32).to_be_bytes().to_vec();
    out.extend(kind);
    out.extend(body);
    out
}

/// A full box (version, no flags).
pub fn full(kind: &[u8; 4], version: u8, body: &[u8]) -> Vec<u8> {
    bx(kind, &[&[version, 0, 0, 0][..], body].concat())
}

/// A HEIF file: `ftyp`, `mdat`, then `meta`; item locations are offsets into `mdat`.
#[derive(Default)]
pub struct HeifFile {
    pub brand: Option<[u8; 4]>,
    pub primary: u16,
    /// (item id, item type).
    pub items: Vec<(u16, [u8; 4])>,
    /// (reference type, from, to).
    pub refs: Vec<([u8; 4], u16, Vec<u16>)>,
    /// Property boxes in `ipco`, numbered from 1.
    pub props: Vec<Vec<u8>>,
    /// (item id, property numbers).
    pub assoc: Vec<(u16, Vec<u8>)>,
    /// (item id, construction method, offset, length).
    pub locations: Vec<(u16, u16, u32, u32)>,
    pub mdat: Vec<u8>,
    pub no_meta: bool,
}

impl HeifFile {
    pub fn build(&self) -> Vec<u8> {
        let brand = self.brand.unwrap_or(*b"heic");
        let mut file = bx(b"ftyp", &[&brand[..], &[0; 4], b"mif1", &brand].concat());
        let base = (file.len() + 8) as u32;
        file.extend(bx(b"mdat", &self.mdat));
        if self.no_meta {
            return file;
        }
        let be16 = |v: u16| v.to_be_bytes();
        let mut meta = full(b"hdlr", 0, &[&[0; 4][..], b"pict", &[0; 13]].concat());
        meta.extend(full(b"pitm", 0, &be16(self.primary)));
        let mut iinf = be16(self.items.len() as u16).to_vec();
        for (id, ty) in &self.items {
            iinf.extend(full(
                b"infe",
                2,
                &[&be16(*id)[..], &[0, 0], ty, &[0]].concat(),
            ));
        }
        meta.extend(full(b"iinf", 0, &iinf));
        let mut iref = Vec::new();
        for (ty, from, to) in &self.refs {
            let ids: Vec<u8> = to.iter().flat_map(|&t| be16(t)).collect();
            iref.extend(bx(
                ty,
                &[&be16(*from)[..], &be16(to.len() as u16), &ids].concat(),
            ));
        }
        meta.extend(full(b"iref", 0, &iref));
        let mut iloc = vec![0x44, 0x40];
        iloc.extend(be16(self.locations.len() as u16));
        for &(id, method, offset, length) in &self.locations {
            iloc.extend(
                [
                    &be16(id)[..],
                    &be16(method),
                    &[0, 0],
                    &base.to_be_bytes(),
                    &[0, 1],
                ]
                .concat(),
            );
            iloc.extend([offset.to_be_bytes(), length.to_be_bytes()].concat());
        }
        meta.extend(full(b"iloc", 1, &iloc));
        let mut ipma = (self.assoc.len() as u32).to_be_bytes().to_vec();
        for (id, props) in &self.assoc {
            ipma.extend(be16(*id));
            ipma.push(props.len() as u8);
            ipma.extend(props);
        }
        let ipco = bx(b"ipco", &self.props.concat());
        meta.extend(bx(b"iprp", &[ipco, full(b"ipma", 0, &ipma)].concat()));
        file.extend(full(b"meta", 0, &meta));
        file
    }
}

/// A TIFF entry value: a number stored in the entry, or the offset of a blob.
pub enum V {
    N(u32),
    Blob(usize),
}

/// A TIFF entry: tag, field type, count, value.
pub type Entry = (u16, u16, u32, V);

/// A little-endian TIFF-structured file (magic 42, or 0x55 for RW2): `blobs`, then the SubIFDs,
/// then IFD0, which gets a SubIFDs tag (0x014A) when `subs` isn't empty.
pub fn raw_tiff(magic: u16, blobs: &[&[u8]], ifd0: &[Entry], subs: &[Vec<Entry>]) -> Vec<u8> {
    let mut out = b"II".to_vec();
    out.extend(magic.to_le_bytes());
    out.extend([0; 4]);
    let mut at = Vec::new();
    for blob in blobs {
        at.push(out.len() as u32);
        out.extend_from_slice(blob);
    }
    let ifd = |out: &mut Vec<u8>, entries: &[Entry], extra: Option<(u32, u32)>| {
        let start = out.len() as u32;
        out.extend((entries.len() as u16 + u16::from(extra.is_some())).to_le_bytes());
        for (tag, kind, count, value) in entries {
            let value = match value {
                V::N(n) => *n,
                V::Blob(i) => at[*i],
            };
            out.extend([tag.to_le_bytes(), kind.to_le_bytes()].concat());
            out.extend([count.to_le_bytes(), value.to_le_bytes()].concat());
        }
        if let Some((count, value)) = extra {
            out.extend([&0x014Au16.to_le_bytes()[..], &4u16.to_le_bytes()].concat());
            out.extend([count.to_le_bytes(), value.to_le_bytes()].concat());
        }
        out.extend([0; 4]);
        start
    };
    let offsets: Vec<u32> = subs.iter().map(|s| ifd(&mut out, s, None)).collect();
    let extra = match offsets[..] {
        [] => None,
        [one] => Some((1, one)),
        _ => {
            let list = out.len() as u32;
            offsets.iter().for_each(|o| out.extend(o.to_le_bytes()));
            Some((offsets.len() as u32, list))
        }
    };
    let first = ifd(&mut out, ifd0, extra);
    out[4..8].copy_from_slice(&first.to_le_bytes());
    out
}

/// Decodes a HEIC or AVIF fixture through this machine's OS decoder. When that decoder can't
/// read the file, checks that decoding fails with exactly the `codec_missing` error the probe
/// gives, and returns `None`; that is a failure for a format named in the comma-separated
/// `IMAGE_CONVERT_TEST_REQUIRE` (CI sets it where the decoder must be present).
pub fn os_heif(name: &str) -> Option<image_convert::Pixels> {
    use image_convert::{bmff, detect, os};
    let data = fixture(name);
    let kind = detect::detect(&data, std::path::Path::new(name)).unwrap();
    let heif = bmff::heif(&data, kind).unwrap();
    let required = std::env::var("IMAGE_CONVERT_TEST_REQUIRE")
        .is_ok_and(|v| v.split(',').any(|f| f == heif.kind.name()));
    match os::heif_available(heif.kind) {
        Ok(()) => Some(os::decode_heif(&data, &heif).unwrap()),
        Err(missing) => {
            assert!(!required, "{name}: {}", missing.message);
            assert_eq!(os::decode_heif(&data, &heif), Err(missing));
            None
        }
    }
}

/// A placeable WMF of 96 × 48 px (96 units per inch) with a steel-blue rectangle from (8, 8) to
/// (88, 40) drawn without an outline.
pub fn placeable_wmf() -> Vec<u8> {
    let record = |function: u16, params: &[u16]| -> Vec<u8> {
        let words = 3 + params.len() as u32;
        let mut r = words.to_le_bytes().to_vec();
        r.extend(function.to_le_bytes());
        r.extend(params.iter().flat_map(|p| p.to_le_bytes()));
        r
    };
    let records = [
        record(0x020B, &[0, 0]),                 // SetWindowOrg
        record(0x020C, &[48, 96]),               // SetWindowExt (y, x)
        record(0x02FC, &[0, 0x8246, 0x00B4, 0]), // CreateBrushIndirect: solid, RGB(70, 130, 180)
        record(0x012D, &[0]),                    // SelectObject
        record(0x02FA, &[5, 0, 0, 0, 0]),        // CreatePenIndirect: PS_NULL
        record(0x012D, &[1]),                    // SelectObject
        record(0x041B, &[40, 88, 8, 8]),         // Rectangle (bottom, right, top, left)
        record(0x0000, &[]),                     // EOF
    ]
    .concat();
    let words = (18 + records.len()) as u32 / 2;
    let mut header = vec![1, 0, 9, 0, 0, 3];
    header.extend(words.to_le_bytes());
    header.extend([2, 0]);
    header.extend(9u32.to_le_bytes());
    header.extend([0, 0]);
    let mut placeable = b"\xd7\xcd\xc6\x9a\0\0".to_vec();
    for v in [0u16, 0, 96, 48, 96] {
        placeable.extend(v.to_le_bytes());
    }
    placeable.extend([0; 4]);
    let checksum = placeable
        .as_chunks::<2>()
        .0
        .iter()
        .fold(0, |x, w| x ^ u16::from_le_bytes(*w));
    placeable.extend(checksum.to_le_bytes());
    [placeable, header, records].concat()
}

/// A plain GDI EMF of 96 × 48 px (a 1 × 0.5 inch frame recorded at 96 DPI) with the same
/// rectangle as [`placeable_wmf`] and an orange circle on it.
pub fn plain_emf() -> Vec<u8> {
    let record = |kind: u32, params: &[u32]| -> Vec<u8> {
        let size = 8 + 4 * params.len() as u32;
        [kind, size]
            .iter()
            .chain(params)
            .flat_map(|v| v.to_le_bytes())
            .collect()
    };
    let records = [
        record(39, &[1, 0, 0x00B4_8246, 0]), // CreateBrushIndirect: object 1, solid, RGB(70, 130, 180)
        record(37, &[1]),                    // SelectObject
        record(37, &[0x8000_0008]),          // SelectObject: the stock NULL_PEN
        record(43, &[8, 8, 88, 40]),         // Rectangle (left, top, right, bottom)
        record(39, &[2, 0, 0x0000_A5FF, 0]), // CreateBrushIndirect: object 2, RGB(255, 165, 0)
        record(37, &[2]),                    // SelectObject
        record(42, &[60, 12, 84, 36]),       // Ellipse
        record(14, &[0, 16, 20]),            // EOF
    ]
    .concat();
    let size = 88 + records.len() as u32;
    let header = [
        [8, 8, 87, 39],                  // bounds in device pixels
        [0, 0, 2540, 1270],              // frame in 0.01 mm
        [0x464D_4520, 0x10000, size, 9], // ' EMF', version, bytes, records
        [3, 0, 0, 0],                    // handles, description length and offset, palette
        [960, 480, 254, 127],            // the reference device in pixels and mm (96 DPI)
    ]
    .concat();
    let header = record(1, &header);
    [header, records].concat()
}

/// A CR3 with a JPEG as track 1's first sample, a `PRVW` preview and a `CMT1` block.
pub fn cr3(sample: &[u8], prvw: &[u8], cmt1: &[u8]) -> Vec<u8> {
    let metadata = [
        &[
            0x85, 0xc0, 0xb6, 0x87, 0x82, 0x0f, 0x11, 0xe0, 0x81, 0x11, 0xf4, 0xce, 0x46, 0x2b,
            0x6a, 0x48,
        ][..],
        &bx(b"CNCV", b"CanonCR3"),
        &bx(b"CMT1", cmt1),
    ]
    .concat();
    let preview = [
        &[
            0xea, 0xf4, 0x2b, 0x5e, 0x1c, 0x98, 0x4b, 0x88, 0xb9, 0xfb, 0xb7, 0xdc, 0x40, 0x6e,
            0x4d, 0x16,
        ][..],
        &[0; 8],
        &bx(
            b"PRVW",
            &[
                &[0, 0, 0, 0, 0, 1, 0, 8, 0, 8, 0, 1][..],
                &(prvw.len() as u32).to_be_bytes(),
                prvw,
            ]
            .concat(),
        ),
    ]
    .concat();
    let ftyp = bx(b"ftyp", b"crx \0\0\0\x01crx isom");
    let stbl = |offset: u32| {
        let stsz = full(
            b"stsz",
            0,
            &[
                0u32.to_be_bytes(),
                1u32.to_be_bytes(),
                (sample.len() as u32).to_be_bytes(),
            ]
            .concat(),
        );
        let stco = full(
            b"stco",
            0,
            &[1u32.to_be_bytes(), offset.to_be_bytes()].concat(),
        );
        bx(
            b"trak",
            &bx(b"mdia", &bx(b"minf", &bx(b"stbl", &[stsz, stco].concat()))),
        )
    };
    let moov_len = bx(b"moov", &[bx(b"uuid", &metadata), stbl(0)].concat()).len();
    let offset = ftyp.len() + moov_len + bx(b"uuid", &preview).len() + 8;
    let moov = bx(
        b"moov",
        &[bx(b"uuid", &metadata), stbl(offset as u32)].concat(),
    );
    [ftyp, moov, bx(b"uuid", &preview), bx(b"mdat", sample)].concat()
}
