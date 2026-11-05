//! PNG, GIF, BMP, ICO and WebP through the image crate, after each header's size is checked.

use std::io::Cursor;

use image::codecs::webp::WebPDecoder;
use image::codecs::{bmp::BmpDecoder, gif::GifDecoder, ico::IcoDecoder, png::PngDecoder};
use image::error::UnsupportedErrorKind;
use image::{ImageDecoder, ImageError, ImageResult, Limits};

use crate::color::Colour;
use crate::detect::Kind;
use crate::error::{Error, Warning};
use crate::{Layout, Pixels, Samples, check_size, exif_and_orientation};

const PNG_SIGNATURE: &[u8] = b"\x89PNG\r\n\x1a\n";

/// Decodes a PNG, GIF, BMP, ICO or WebP file: its default image, first frame or largest icon.
pub fn decode(data: &[u8], kind: Kind) -> Result<Pixels, Error> {
    let label = &kind.label();
    let ico;
    let (data, width, height, frames) = match kind {
        Kind::Ico => {
            let (one, w, h) = ico_entry(data).ok_or_else(|| no_header(label))?;
            ico = one;
            (&ico[..], w, h, 1)
        }
        _ => {
            let header = match kind {
                Kind::Png => png_header(data),
                Kind::Gif => gif_header(data),
                Kind::Bmp => bmp_header(data)?,
                Kind::Webp => webp_header(data),
                _ => unreachable!("{kind:?} is not a raster type"),
            };
            let (w, h, frames) = header.ok_or_else(|| no_header(label))?;
            (data, w, h, frames)
        }
    };
    check_size(width.into(), height.into(), label)?;

    let failed = |err| failure(kind, err);
    let mut decoder = open(data, kind).map_err(failed)?;
    // The decoder can read another size than the header readers above.
    let (width, height) = decoder.dimensions();
    check_size(width.into(), height.into(), label)?;
    decoder.set_limits(limits()).map_err(failed)?;
    let icc = decoder.icc_profile().map_err(failed)?;
    let (exif, orientation) = exif_and_orientation(decoder.exif_metadata().map_err(failed)?);
    let color = decoder.color_type();
    let mut buf = vec![0; decoder.total_bytes() as usize];
    decoder.read_image_boxed(&mut buf).map_err(failed)?;

    let channels = color.channel_count();
    let layout =
        [Layout::Grey, Layout::GreyAlpha, Layout::Rgb, Layout::Rgba][channels as usize - 1];
    let samples = if color.bytes_per_pixel() == 2 * channels {
        Samples::U16(
            buf.as_chunks()
                .0
                .iter()
                .map(|&b| u16::from_ne_bytes(b))
                .collect(),
        )
    } else {
        Samples::U8(buf)
    };
    Ok(Pixels {
        width,
        height,
        layout,
        samples,
        bit_depth: if kind == Kind::Png {
            data.get(24).copied().unwrap_or(8)
        } else {
            8
        },
        colour: icc.map(Colour::Icc),
        exif,
        orientation,
        frames,
        warnings: (frames > 1)
            .then(Warning::first_frame_only)
            .into_iter()
            .collect(),
    })
}

fn open(data: &[u8], kind: Kind) -> ImageResult<Box<dyn ImageDecoder + '_>> {
    let reader = Cursor::new(data);
    Ok(match kind {
        Kind::Png => Box::new(PngDecoder::with_limits(reader, limits())?),
        Kind::Gif => Box::new(GifDecoder::new(reader)?),
        Kind::Bmp => Box::new(BmpDecoder::new(reader)?),
        Kind::Ico => Box::new(IcoDecoder::new(reader)?),
        _ => Box::new(WebPDecoder::new(reader)?),
    })
}

fn limits() -> Limits {
    let mut limits = Limits::default();
    limits.max_image_width = Some(65535);
    limits.max_image_height = Some(65535);
    limits.max_alloc = Some(2 << 30);
    limits
}

fn no_header(label: &str) -> Error {
    Error::corrupt(label, "its header is missing or cut short")
}

/// BMP features image's decoder refuses become `unsupported_variant`; everything else is damage.
fn failure(kind: Kind, err: ImageError) -> Error {
    match err {
        ImageError::Unsupported(e) if kind == Kind::Bmp => match e.kind() {
            UnsupportedErrorKind::GenericFeature(feature) => {
                Error::unsupported_variant("BMP", feature)
            }
            _ => Error::unsupported_variant("BMP", e),
        },
        err => Error::corrupt(&kind.label(), root_cause(&err)),
    }
}

/// The innermost error's message, which names the actual problem.
fn root_cause(err: &(dyn std::error::Error + 'static)) -> String {
    let mut err = err;
    while let Some(source) = err.source() {
        err = source;
    }
    err.to_string()
}

fn bytes<const N: usize>(d: &[u8], at: usize) -> Option<[u8; N]> {
    d.get(at..at.checked_add(N)?)?.try_into().ok()
}

fn u16le(d: &[u8], at: usize) -> Option<u32> {
    bytes(d, at).map(|b| u16::from_le_bytes(b).into())
}

fn u32le(d: &[u8], at: usize) -> Option<u32> {
    bytes(d, at).map(u32::from_le_bytes)
}

fn u32be(d: &[u8], at: usize) -> Option<u32> {
    bytes(d, at).map(u32::from_be_bytes)
}

/// A signed 32-bit size (BMP heights are negative for top-down rows), as its magnitude.
fn i32le(d: &[u8], at: usize) -> Option<u32> {
    bytes(d, at).map(|b| i32::from_le_bytes(b).unsigned_abs())
}

/// Width, height and frame count (from an APNG's acTL) of a PNG.
fn png_header(d: &[u8]) -> Option<(u32, u32, u32)> {
    if d.get(12..16)? != b"IHDR" {
        return None;
    }
    let mut frames = 1;
    let mut at = 8;
    while let (Some(len), Some(chunk)) = (u32be(d, at), d.get(at + 4..at + 8)) {
        match chunk {
            b"IDAT" => break,
            b"acTL" => frames = u32be(d, at + 8)?.max(1),
            _ => {}
        }
        at += 12 + len as usize;
    }
    Some((u32be(d, 16)?, u32be(d, 20)?, frames))
}

/// The logical screen size and the number of images in a GIF; a cut-short file counts what's there.
fn gif_header(d: &[u8]) -> Option<(u32, u32, u32)> {
    let table = |flags: u8| {
        if flags & 0x80 != 0 {
            3 << ((flags & 7) + 1)
        } else {
            0
        }
    };
    let sub_blocks = |mut at: usize| loop {
        let len = *d.get(at)? as usize;
        at += 1 + len;
        if len == 0 {
            return Some(at);
        }
    };
    let (width, height) = (u16le(d, 6)?, u16le(d, 8)?);
    let mut at = 13 + table(*d.get(10)?);
    let mut frames = 0;
    loop {
        let next = match d.get(at) {
            Some(0x21) => sub_blocks(at + 2),
            Some(0x2c) => {
                frames += 1;
                d.get(at + 9)
                    .and_then(|&flags| sub_blocks(at + 11 + table(flags)))
            }
            _ => None,
        };
        match next {
            Some(next) => at = next,
            None => break,
        }
    }
    Some((width, height, frames.max(1)))
}

/// A BMP's size; refuses compression 4 and 5 (an embedded JPEG or PNG) before any decoding.
fn bmp_header(d: &[u8]) -> Result<Option<(u32, u32, u32)>, Error> {
    let Some(header) = u32le(d, 14) else {
        return Ok(None);
    };
    // Windows info headers; OS/2 headers use 4 for RLE24.
    if matches!(header, 40 | 52 | 56 | 108 | 124) {
        match u32le(d, 30) {
            Some(4) => return Err(Error::unsupported_variant("BMP", "embedded JPEG")),
            Some(5) => return Err(Error::unsupported_variant("BMP", "embedded PNG")),
            _ => {}
        }
    }
    Ok(if header == 12 {
        u16le(d, 18).zip(u16le(d, 20))
    } else {
        i32le(d, 18).zip(i32le(d, 22))
    }
    .map(|(w, h)| (w, h, 1)))
}

/// A WebP's canvas size (VP8X, else the VP8/VP8L bitstream's) and its ANMF frame count.
fn webp_header(d: &[u8]) -> Option<(u32, u32, u32)> {
    let u24 = |at: usize| bytes::<3>(d, at).map(|b| u32::from_le_bytes([b[0], b[1], b[2], 0]));
    let mut size = None;
    let mut frames = 0;
    let mut at = 12;
    while let (Some(id), Some(len)) = (d.get(at..at + 4), u32le(d, at + 4)) {
        let body = at + 8;
        match id {
            b"VP8X" => size = Some((u24(body + 4)? + 1, u24(body + 7)? + 1)),
            b"VP8 " if size.is_none() => {
                size = Some((u16le(d, body + 6)? & 0x3fff, u16le(d, body + 8)? & 0x3fff))
            }
            b"VP8L" if size.is_none() => {
                let bits = u32le(d, body + 1)?;
                size = Some(((bits & 0x3fff) + 1, ((bits >> 14) & 0x3fff) + 1));
            }
            b"ANMF" => frames += 1,
            _ => {}
        }
        at = body + len as usize + (len as usize & 1);
    }
    let (width, height) = size?;
    Some((width, height, frames.max(1)))
}

/// The largest icon (by area, then bit count) as a one-entry ICO, with its real size.
fn ico_entry(d: &[u8]) -> Option<(Vec<u8>, u32, u32)> {
    let side = |b: u8| if b == 0 { 256 } else { u32::from(b) };
    let mut best: Option<((u32, u32), &[u8])> = None;
    for i in 0..u16le(d, 4)? as usize {
        let entry = d.get(6 + 16 * i..22 + 16 * i)?;
        let score = (side(entry[0]) * side(entry[1]), u16le(entry, 6)?);
        if best.is_none_or(|(top, _)| score > top) {
            best = Some((score, entry));
        }
    }
    let (_, entry) = best?;
    let (len, offset) = (u32le(entry, 8)? as usize, u32le(entry, 12)? as usize);
    let image = d.get(offset..offset.checked_add(len)?)?;
    let (width, height) = if image.starts_with(PNG_SIGNATURE) {
        let (w, h, _) = png_header(image)?;
        (w, h)
    } else {
        (i32le(image, 4)?, i32le(image, 8)? / 2)
    };
    let mut one = vec![0, 0, 1, 0, 1, 0];
    one.extend(&entry[..12]);
    one.extend(22u32.to_le_bytes());
    one.extend(image);
    Some((one, width, height))
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{ExtendedColorType, ImageEncoder};

    fn png(width: u32, height: u32, color: ExtendedColorType, pixels: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        image::codecs::png::PngEncoder::new(&mut out)
            .write_image(pixels, width, height, color)
            .unwrap();
        out
    }

    fn with_ihdr_size(mut png: Vec<u8>, width: u32, height: u32) -> Vec<u8> {
        png[16..20].copy_from_slice(&width.to_be_bytes());
        png[20..24].copy_from_slice(&height.to_be_bytes());
        png
    }

    #[test]
    fn png_keeps_16_bit_samples_and_its_bit_depth() {
        let words: Vec<u8> = [0u16, 257, 65535, 4660]
            .iter()
            .flat_map(|w| w.to_ne_bytes())
            .collect();
        let px = decode(&png(2, 2, ExtendedColorType::L16, &words), Kind::Png).unwrap();
        assert_eq!(
            (px.width, px.height, px.layout, px.bit_depth),
            (2, 2, Layout::Grey, 16)
        );
        assert_eq!(px.samples, Samples::U16(vec![0, 257, 65535, 4660]));
        assert_eq!((px.orientation, px.frames, px.warnings.len()), (1, 1, 0));
    }

    #[test]
    fn sizes_come_from_headers_before_decoding() {
        let small = png(1, 1, ExtendedColorType::Rgb8, &[1, 2, 3]);
        let err = decode(&with_ihdr_size(small.clone(), 100_000, 100_000), Kind::Png).unwrap_err();
        assert_eq!(err.code, "image_too_large");
        let err = decode(&with_ihdr_size(small.clone(), 0, 1), Kind::Png).unwrap_err();
        assert_eq!(
            err.message,
            "Can't decode this PNG file: its header gives a size of 0 pixels."
        );
        let err = decode(&small[..20], Kind::Png).unwrap_err();
        assert_eq!(
            err.message,
            "Can't decode this PNG file: its header is missing or cut short."
        );

        let mut gif = b"GIF89a".to_vec();
        gif.extend([0x50, 0xc3, 0x50, 0xc3, 0, 0, 0, 0x3b]);
        assert_eq!(gif_header(&gif), Some((50000, 50000, 1)));
        assert_eq!(decode(&gif, Kind::Gif).unwrap_err().code, "image_too_large");

        let mut webp = b"RIFF\0\0\0\0WEBPVP8X\x0a\0\0\0\x02\0\0\0".to_vec();
        webp.extend([0xff, 0xff, 0x00, 0x0f, 0x27, 0x00]);
        assert_eq!(webp_header(&webp), Some((65536, 10000, 1)));
        assert_eq!(
            decode(&webp, Kind::Webp).unwrap_err().code,
            "image_too_large"
        );
    }

    #[test]
    fn bmp_with_embedded_jpeg_or_png_is_refused_by_name() {
        for (compression, variant) in [(4u32, "embedded JPEG"), (5, "embedded PNG")] {
            let mut bmp = b"BM".to_vec();
            bmp.extend([0; 12]);
            bmp.extend(40u32.to_le_bytes());
            bmp.extend(1i32.to_le_bytes());
            bmp.extend(1i32.to_le_bytes());
            bmp.extend([1, 0, 24, 0]);
            bmp.extend(compression.to_le_bytes());
            bmp.resize(60, 0);
            let err = decode(&bmp, Kind::Bmp).unwrap_err();
            assert_eq!(err.message, format!("BMP with {variant} isn't supported."));
        }
    }

    #[test]
    fn damage_is_reported_with_the_decoders_reason() {
        let mut bad = png(4, 4, ExtendedColorType::Rgb8, &[9; 48]);
        let n = bad.len();
        bad[n - 20] ^= 0xff;
        let err = decode(&bad, Kind::Png).unwrap_err();
        assert_eq!(err.code, "corrupt_image");
        assert!(
            err.message.starts_with("Can't decode this PNG file: "),
            "{}",
            err.message
        );
    }
}
