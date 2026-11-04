//! JPEG through zune-jpeg, after the frame header is read and unsupported codings are refused;
//! the few layouts zune-jpeg 0.5.15 gets wrong go through jpeg-decoder.

use jpeg_decoder::ColorTransform;
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
    /// The first component's sampling factors.
    first_sampling: (u8, u8),
    /// Whether three components have the ids R, G and B.
    rgb_ids: bool,
    /// Whether a JFIF APP0 segment is present.
    jfif: bool,
    /// The Adobe APP14 colour transform, when that segment is present.
    adobe: Option<u8>,
    /// Where the first scan's marker is.
    scan: usize,
    /// How many components the first scan holds.
    scan_components: u8,
}

impl Frame {
    /// Whether zune-jpeg 0.5.15 gets this layout wrong (fixed in its 0.5.16 pre-releases): one
    /// scan per component, 4×2, 2×4 and 1×4 sampling, chroma finer than luma, and subsampled CMYK.
    fn misread_by_zune(&self) -> bool {
        let subsampled = self.components > 1 && self.sampling != (1, 1);
        let odd = !matches!(self.sampling, (2, 1) | (1, 2) | (2, 2) | (4, 1));
        let separate = matches!(self.marker, 0xc0 | 0xc1) && self.scan_components < self.components;
        let fine_chroma = self.sampling != self.first_sampling;
        separate || subsampled && (odd || fine_chroma || self.components == 4)
    }

    /// Whether three components hold RGB, not YCbCr: JFIF decides, then Adobe, then the ids, as
    /// in libjpeg.
    fn rgb(&self) -> bool {
        !self.jfif && self.adobe.map_or(self.rgb_ids, |transform| transform == 0)
    }
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
    // zune-jpeg fills a cut-off image with grey; the scans of a whole one end at an EOI marker.
    if !ends(data, frame.scan) {
        return Err(Error::corrupt("JPEG", "its image data is cut short"));
    }

    let (layout, samples, exif, icc) = if frame.misread_by_zune() {
        other(data, &frame).map_err(|e| match frame.marker {
            // jpeg-decoder 0.3.2 loses its place in some progressive scans of these layouts.
            0xc2 if frame.components == 3 => {
                let variant = "progressive scans and uneven chroma sampling";
                Error::unsupported_variant("JPEG", variant)
            }
            _ => e,
        })?
    } else {
        zune(data, &frame)?
    };
    let (exif, orientation) = exif_and_orientation(exif);
    Ok(Pixels {
        width: frame.width,
        height: frame.height,
        layout,
        samples: Samples::U8(samples),
        bit_depth: 8,
        colour: icc.map(Colour::Icc),
        exif,
        orientation,
        frames: 1,
        warnings: Vec::new(),
    })
}

/// The layout, the samples, the EXIF block and the ICC profile.
type Decoded = (Layout, Vec<u8>, Option<Vec<u8>>, Option<Vec<u8>>);

/// Decodes with jpeg-decoder, which gives CMYK as ink whether or not an Adobe segment says so.
fn other(data: &[u8], frame: &Frame) -> Result<Decoded, Error> {
    let mut decoder = jpeg_decoder::Decoder::new(data);
    if frame.components == 3 {
        decoder.set_color_transform(if frame.rgb() {
            ColorTransform::RGB
        } else {
            ColorTransform::YCbCr
        });
    }
    let mut samples = decoder.decode().map_err(|e| Error::corrupt("JPEG", e))?;
    let layout = if frame.components == 3 {
        Layout::Rgb
    } else {
        if frame.adobe.is_none() {
            samples.iter_mut().for_each(|v| *v = 255 - *v);
        }
        Layout::Cmyk
    };
    let exif = decoder.exif_data().map(<[u8]>::to_vec);
    Ok((layout, samples, exif, decoder.icc_profile()))
}

/// Decodes with zune-jpeg.
fn zune(data: &[u8], frame: &Frame) -> Result<Decoded, Error> {
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
    Ok((
        layout,
        samples,
        decoder.exif().cloned(),
        decoder.icc_profile(),
    ))
}

/// Reads the markers up to the first scan: the frame header and any Adobe APP14 transform.
pub(crate) fn frame(data: &[u8]) -> Result<Frame, Error> {
    let cut = || Error::corrupt("JPEG", "its header is cut short");
    let byte = |at: usize| data.get(at).copied().ok_or_else(cut);
    let word = |at: usize| Ok::<_, Error>(u16::from_be_bytes([byte(at)?, byte(at + 1)?]));
    let mut found = None;
    let mut jfif = false;
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
                let mut ids = Vec::new();
                for i in 0..usize::from(components) {
                    ids.push(byte(body + 6 + 3 * i)?);
                    let factors = byte(body + 7 + 3 * i)?;
                    sampling = (sampling.0.max(factors >> 4), sampling.1.max(factors & 15));
                }
                let first = data.get(body + 7).copied().unwrap_or(0);
                found = Some(Frame {
                    marker,
                    precision: byte(body)?,
                    height: word(body + 1)?.into(),
                    width: word(body + 3)?.into(),
                    components,
                    sampling,
                    first_sampling: (first >> 4, first & 15),
                    rgb_ids: ids == b"RGB",
                    jfif: false,
                    adobe: None,
                    scan: 0,
                    scan_components: 0,
                });
            }
            0xf7 => return Err(Error::unsupported_variant("JPEG", "JPEG-LS")),
            0xe0 if data.get(body..body + 5) == Some(b"JFIF\0") => jfif = true,
            0xee if data.get(body..body + 5) == Some(b"Adobe") => adobe = Some(byte(body + 11)?),
            _ => {}
        }
        at += 2 + usize::from(word(at + 2)?);
    }
    let frame = found.ok_or_else(|| Error::corrupt("JPEG", "it has no frame header"))?;
    Ok(Frame {
        jfif,
        adobe,
        scan: at,
        scan_components: data.get(at + 4).copied().unwrap_or(0),
        ..frame
    })
}

/// Whether the markers from `at` on reach an EOI: segments are skipped by their length, so the
/// bytes FF D9 inside one don't count, and coded data (which never holds them) byte by byte.
fn ends(data: &[u8], mut at: usize) -> bool {
    while let Some(&marker) = data.get(at + 1) {
        match marker {
            0xd9 => return true,
            // A stuffed zero, a restart marker or fill inside coded data.
            0x00 | 0xd0..=0xd7 | 0xff => at += 1,
            _ => match data.get(at + 2..at + 4) {
                Some(&[hi, lo]) => at += 2 + usize::from(u16::from_be_bytes([hi, lo])),
                _ => return false,
            },
        }
        let Some(next) = data
            .get(at..)
            .and_then(|rest| rest.iter().position(|&b| b == 0xff))
        else {
            return false;
        };
        at += next;
    }
    false
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
            first_sampling: (1, 1),
            rgb_ids: false,
            jfif: true,
            adobe: None,
            scan: grey.windows(2).position(|w| w == [0xff, 0xda]).unwrap(),
            scan_components: 1,
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
