//! JPEG through zune-jpeg, after the frame header is read and unsupported codings are refused.

use zune_jpeg::JpegDecoder;
use zune_jpeg::zune_core::bytestream::ZCursor;
use zune_jpeg::zune_core::colorspace::ColorSpace;
use zune_jpeg::zune_core::options::DecoderOptions;

use crate::color::Colour;
use crate::error::Error;
use crate::{Layout, Pixels, Samples, check_size, exif_and_orientation};

/// What the markers before the first scan say about a JPEG.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Frame {
    /// The SOF marker's second byte, 0xC0 (baseline) to 0xCF.
    pub(crate) marker: u8,
    precision: u8,
    pub(crate) width: u32,
    pub(crate) height: u32,
    components: u8,
    /// The largest horizontal and vertical sampling factors of any component.
    sampling: (u8, u8),
    /// The Adobe APP14 colour transform, when that segment is present.
    adobe: Option<u8>,
}

/// Decodes the first image of a JPEG (MPO and Ultra HDR files hold more): grey, RGB or CMYK.
pub fn decode(data: &[u8]) -> Result<Pixels, Error> {
    let frame = frame(data)?;
    let refused = match frame.marker {
        _ if frame.precision != 8 => Some(format!("{}-bit samples", frame.precision)),
        0xc9..=0xcb | 0xcd..=0xcf => Some("arithmetic coding".into()),
        0xc3 | 0xc7 => Some("lossless coding".into()),
        0xc5 | 0xc6 => Some("hierarchical coding".into()),
        _ if frame.components == 2 || frame.components > 4 => {
            Some(format!("{} colour components", frame.components))
        }
        // zune-jpeg 0.5.15 decodes these to wrong colours (fixed in its 0.5.16 pre-releases).
        _ if frame.components > 1
            && !matches!(frame.sampling, (1, 1) | (2, 1) | (1, 2) | (2, 2) | (4, 1)) =>
        {
            let (h, v) = frame.sampling;
            Some(format!("{h}×{v} sampling factors"))
        }
        _ => None,
    };
    if let Some(variant) = refused {
        return Err(Error::unsupported_variant("JPEG", variant));
    }
    if frame.components == 0 {
        return Err(Error::corrupt(
            "JPEG",
            "its frame header lists no colour components",
        ));
    }
    check_size(frame.width.into(), frame.height.into(), "JPEG")?;

    // Four components come out raw, as zune-jpeg reads them (YCCK only after Adobe transform 2).
    let (layout, colorspace) = match (frame.components, frame.adobe) {
        (1, _) => (Layout::Grey, ColorSpace::Luma),
        (3, _) => (Layout::Rgb, ColorSpace::RGB),
        (_, Some(2)) => (Layout::Cmyk, ColorSpace::YCCK),
        _ => (Layout::Cmyk, ColorSpace::CMYK),
    };
    let options = DecoderOptions::default()
        .set_max_width(65535)
        .set_max_height(65535)
        .jpeg_set_out_colorspace(colorspace);
    let mut decoder = JpegDecoder::new_with_options(ZCursor::new(data), options);
    let mut samples = decoder.decode().map_err(|e| Error::corrupt("JPEG", e))?;
    if colorspace == ColorSpace::YCCK {
        ycck_to_inverted_cmyk(&mut samples);
    }
    // Adobe writes CMYK inverted (255 = no ink).
    if layout == Layout::Cmyk && frame.adobe.is_some() {
        samples.iter_mut().for_each(|v| *v = 255 - *v);
    }
    let (exif, orientation) = exif_and_orientation(decoder.exif().cloned());
    Ok(Pixels {
        width: frame.width,
        height: frame.height,
        layout,
        samples: Samples::U8(samples),
        bit_depth: 8,
        colour: decoder.icc_profile().map(Colour::Icc),
        exif,
        orientation,
        frames: 1,
        warnings: Vec::new(),
    })
}

/// Reads the markers up to the first scan: the frame header and any Adobe APP14 transform.
pub(crate) fn frame(data: &[u8]) -> Result<Frame, Error> {
    let cut = || Error::corrupt("JPEG", "its header is cut short");
    let byte = |at: usize| data.get(at).copied().ok_or_else(cut);
    let word = |at: usize| Ok::<_, Error>(u16::from_be_bytes([byte(at)?, byte(at + 1)?]));
    let mut found = None;
    let mut adobe = None;
    let mut at = 2;
    loop {
        if byte(at)? != 0xff {
            return Err(Error::corrupt("JPEG", "a marker is missing"));
        }
        let marker = byte(at + 1)?;
        match marker {
            0xff => {
                at += 1;
                continue;
            }
            0x01 | 0xd0..=0xd8 => {
                at += 2;
                continue;
            }
            0xd9 | 0xda => break,
            _ => {}
        }
        let body = at + 4;
        match marker {
            0xc0..=0xcf if found.is_none() && !matches!(marker, 0xc4 | 0xc8 | 0xcc) => {
                let components = byte(body + 5)?;
                let mut sampling = (0, 0);
                for i in 0..usize::from(components) {
                    let factors = byte(body + 7 + 3 * i)?;
                    sampling = (sampling.0.max(factors >> 4), sampling.1.max(factors & 15));
                }
                found = Some(Frame {
                    marker,
                    precision: byte(body)?,
                    height: word(body + 1)?.into(),
                    width: word(body + 3)?.into(),
                    components,
                    sampling,
                    adobe: None,
                });
            }
            0xf7 => return Err(Error::unsupported_variant("JPEG", "JPEG-LS")),
            0xee if data.get(body..body + 5) == Some(b"Adobe") => adobe = Some(byte(body + 11)?),
            _ => {}
        }
        at += 2 + usize::from(word(at + 2)?);
    }
    let frame = found.ok_or_else(|| Error::corrupt("JPEG", "it has no frame header"))?;
    Ok(Frame { adobe, ..frame })
}

/// YCCK as zune-jpeg gives it (Y, Cb, Cr, inverted K) to CMYK in Adobe's inverted form.
fn ycck_to_inverted_cmyk(samples: &mut [u8]) {
    for px in samples.as_chunks_mut::<4>().0 {
        let [r, g, b] = ycc_to_rgb(px[0], px[1], px[2]);
        px[..3].copy_from_slice(&[255 - r, 255 - g, 255 - b]);
    }
}

/// Full-range (JFIF) YCbCr to RGB.
pub fn ycc_to_rgb(y: u8, cb: u8, cr: u8) -> [u8; 3] {
    let (y, cb, cr) = (f32::from(y), f32::from(cb) - 128.0, f32::from(cr) - 128.0);
    [
        y + 1.402 * cr,
        y - 0.344_136 * cb - 0.714_136 * cr,
        y + 1.772 * cb,
    ]
    .map(|c| c.round().clamp(0.0, 255.0) as u8)
}

#[cfg(test)]
mod tests {
    use super::*;
    use jpeg_encoder::{ColorType, Encoder};

    fn jpeg(width: u16, height: u16, color: ColorType, pixels: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        Encoder::new(&mut out, 100)
            .encode(pixels, width, height, color)
            .unwrap();
        out
    }

    /// The offset of the first SOF0 marker.
    fn sof(data: &[u8]) -> usize {
        data.windows(2).position(|w| w == [0xff, 0xc0]).unwrap()
    }

    #[test]
    fn reads_the_frame_header_and_adobe_transform() {
        let grey = jpeg(3, 2, ColorType::Luma, &[0; 6]);
        let frame = frame(&grey).unwrap();
        let expected = Frame {
            marker: 0xc0,
            precision: 8,
            width: 3,
            height: 2,
            components: 1,
            sampling: (1, 1),
            adobe: None,
        };
        assert_eq!(frame, expected);
        let cmyk = jpeg(1, 1, ColorType::CmykAsYcck, &[0; 4]);
        assert_eq!(super::frame(&cmyk).unwrap().adobe, Some(2));
        assert_eq!(super::frame(&cmyk[..10]).unwrap_err().code, "corrupt_image");
    }

    #[test]
    fn unsupported_codings_are_refused_from_the_marker() {
        let rgb = jpeg(8, 8, ColorType::Rgb, &[200; 192]);
        let at = sof(&rgb);
        let cases = [
            (0xc0, 12, "12-bit samples"),
            (0xc9, 8, "arithmetic coding"),
            (0xcd, 8, "arithmetic coding"),
            (0xc3, 8, "lossless coding"),
            (0xc7, 8, "lossless coding"),
            (0xc5, 8, "hierarchical coding"),
            (0xf7, 8, "JPEG-LS"),
        ];
        for (marker, precision, variant) in cases {
            let mut data = rgb.clone();
            data[at + 1] = marker;
            data[at + 4] = precision;
            let err = decode(&data).unwrap_err();
            assert_eq!(err.message, format!("JPEG with {variant} isn't supported."));
        }
        let mut odd = rgb.clone();
        odd[at + 11] = 0x42;
        let err = decode(&odd).unwrap_err();
        assert_eq!(
            err.message,
            "JPEG with 4×2 sampling factors isn't supported."
        );
        let mut extended = rgb.clone();
        extended[at + 1] = 0xc1;
        assert_eq!(decode(&extended).unwrap().layout, Layout::Rgb);
    }

    #[test]
    fn cmyk_and_ycck_come_out_as_ink() {
        let ink = [10, 60, 120, 200];
        for color in [ColorType::Cmyk, ColorType::CmykAsYcck] {
            let px = decode(&jpeg(8, 8, color, &ink.repeat(64))).unwrap();
            let Samples::U8(samples) = px.samples else {
                unreachable!()
            };
            assert_eq!(px.layout, Layout::Cmyk);
            for (got, want) in samples[..4].iter().zip(ink) {
                assert!(got.abs_diff(want) <= 3, "{color:?}: {:?}", &samples[..4]);
            }
        }
    }
}
