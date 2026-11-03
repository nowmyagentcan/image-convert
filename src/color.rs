//! Colour to sRGB: ICC profiles and CICP code points through moxcms, CMYK without a profile by the
//! naive formula, and 16-bit samples down to 8.

use moxcms::{
    CicpColorPrimaries, CicpProfile, ColorProfile, DataColorSpace, MatrixCoefficients, ProfileText,
    RenderingIntent, ToneReprCurve, TransferCharacteristics, TransformOptions,
};

use crate::error::Warning;
use crate::{Image, Layout, Pixels, Samples};

/// How the source's colour was handled, as `source.color` and `source.profile` report it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Color {
    /// `srgb` (no profile, or an sRGB one), `converted` or `ignored` (with `icc_ignored`).
    pub kind: &'static str,
    /// The profile's description, or `CMYK (no profile)`.
    pub profile: Option<String>,
}

/// Colour code points (ITU-T H.273), as a HEIF `nclx` box gives them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cicp {
    pub primaries: u16,
    pub transfer: u16,
    pub matrix: u16,
    pub full_range: bool,
}

/// How a source describes its colour.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Colour {
    Icc(Vec<u8>),
    Cicp(Cicp),
    /// The decoder gave sRGB samples already; this is how results report the source's colour.
    Converted(Color),
}

/// Takes the samples out of `px` as an 8-bit sRGB image; an unusable profile adds `icc_ignored`
/// and HDR code points `hdr_clipped`.
pub fn to_srgb(px: &mut Pixels) -> (Image, Color) {
    let mut samples = std::mem::replace(&mut px.samples, Samples::U8(Vec::new()));
    let mut layout = px.layout;
    let mut color = Color {
        kind: "srgb",
        profile: None,
    };
    let converted = match px.colour.take() {
        None => Ok((None, None)),
        Some(Colour::Converted(reported)) => {
            color = reported;
            Ok((None, None))
        }
        Some(Colour::Icc(icc)) => through_profile(&icc, layout, &samples),
        Some(Colour::Cicp(cicp)) => match cicp_profile(cicp) {
            Ok(Some((source, name, hdr))) => {
                if hdr {
                    px.warnings.push(Warning::hdr_clipped());
                }
                transform(&source, layout, &samples).map(|out| (Some(name.into()), Some(out)))
            }
            Ok(None) => Ok((None, None)),
            Err(reason) => Err(reason),
        },
    };
    match converted {
        Ok((name, None)) => color.profile = color.profile.take().or(name),
        Ok((name, Some((out, out_layout)))) => {
            (samples, layout) = (out, out_layout);
            color = Color {
                kind: "converted",
                profile: name,
            };
        }
        Err(reason) => {
            px.warnings.push(Warning::icc_ignored(reason));
            color.kind = "ignored";
        }
    }
    let mut data = match samples {
        Samples::U8(s) => s,
        Samples::U16(s) => s
            .iter()
            .map(|&v| ((u32::from(v) + 128) / 257) as u8)
            .collect(),
    };
    if layout == Layout::Cmyk {
        data = data
            .as_chunks::<4>()
            .0
            .iter()
            .flat_map(|&[c, m, y, k]| [c, m, y].map(|ink| 255 - ink.saturating_add(k)))
            .collect();
        layout = Layout::Rgb;
        color.profile = Some("CMYK (no profile)".into());
        if color.kind == "srgb" {
            color.kind = "converted";
        }
    }
    let image = Image {
        width: px.width,
        height: px.height,
        layout,
        data,
    };
    (image, color)
}

/// Samples in sRGB with their layout; `None` when the profile is sRGB already.
type Converted = Option<(Samples, Layout)>;

/// The profile's name and the samples through it, or why the profile can't be used.
fn through_profile(
    icc: &[u8],
    layout: Layout,
    samples: &Samples,
) -> Result<(Option<String>, Converted), String> {
    let source = ColorProfile::new_from_slice(icc).map_err(|e| lower_first(&e.to_string()))?;
    let name = source.description.as_ref().and_then(text);
    if layout != Layout::Cmyk && is_srgb(&source) {
        return Ok((name, None));
    }
    Ok((name, Some(transform(&source, layout, samples)?)))
}

/// Whether an RGB profile leaves colours as they are in sRGB, whatever its name says: a grid of
/// colours must come through it within 2 levels.
fn is_srgb(source: &ColorProfile) -> bool {
    let rgb = moxcms::Layout::Rgb;
    let options = TransformOptions {
        rendering_intent: RenderingIntent::RelativeColorimetric,
        ..TransformOptions::default()
    };
    let srgb = ColorProfile::new_srgb();
    let transform = source.create_transform_8bit(rgb, &srgb, rgb, options);
    let (DataColorSpace::Rgb, Ok(t)) = (source.color_space, transform) else {
        return false;
    };
    let level = || (0..=255u8).step_by(15);
    let grid: Vec<u8> = level()
        .flat_map(|r| level().flat_map(move |g| level().flat_map(move |b| [r, g, b])))
        .collect();
    let mut out = vec![0; grid.len()];
    t.transform(&grid, &mut out).is_ok() && grid.iter().zip(&out).all(|(a, b)| a.abs_diff(*b) <= 2)
}

/// How results name a source's colour that the OS decoder has already converted to sRGB.
pub fn describe(colour: Option<&Colour>) -> Color {
    let (srgb, profile) = match colour {
        None => (true, None),
        Some(Colour::Converted(reported)) => return reported.clone(),
        Some(Colour::Icc(icc)) => match ColorProfile::new_from_slice(icc) {
            Ok(source) => (is_srgb(&source), source.description.as_ref().and_then(text)),
            Err(_) => (false, None),
        },
        Some(Colour::Cicp(cicp)) => match cicp_profile(*cicp) {
            Ok(None) => (true, None),
            Ok(Some((_, name, _))) => (false, Some(name.into())),
            Err(_) => (false, None),
        },
    };
    let kind = if srgb { "srgb" } else { "converted" };
    Color { kind, profile }
}

/// The profile CICP code points stand for, its name and whether it is HDR (PQ or HLG); `None`
/// for sRGB. Unspecified primaries or transfer (2) read as sRGB's.
fn cicp_profile(cicp: Cicp) -> Result<Option<(ColorProfile, &'static str, bool)>, String> {
    let (primaries, name) = match cicp.primaries {
        1 | 2 => (CicpColorPrimaries::Bt709, "BT.709"),
        9 => (CicpColorPrimaries::Bt2020, "BT.2020"),
        12 => (CicpColorPrimaries::Smpte432, "Display P3"),
        n => return Err(format!("it uses colour primaries {n}")),
    };
    let hdr: Option<fn(f64) -> f64> = match cicp.transfer {
        16 => Some(pq),
        18 => Some(hlg),
        _ => None,
    };
    let transfer = match cicp.transfer {
        2 | 13 if primaries == CicpColorPrimaries::Bt709 => return Ok(None),
        // H.273 calls 6, 14 and 15 functionally the same as 1.
        1 | 6 | 14 | 15 => TransferCharacteristics::Bt709,
        2 | 13 | 16 | 18 => TransferCharacteristics::Srgb,
        n => return Err(format!("it uses transfer characteristics {n}")),
    };
    let mut profile = ColorProfile::new_from_cicp(CicpProfile {
        color_primaries: primaries,
        transfer_characteristics: transfer,
        matrix_coefficients: MatrixCoefficients::Bt709,
        full_range: cicp.full_range,
    });
    if let Some(eotf) = hdr {
        // moxcms prefers the CICP curve to the profile's own, so it goes.
        profile.cicp = None;
        let table = (0..4096)
            .map(|i| (eotf(f64::from(i) / 4095.0).min(1.0) * 65535.0).round() as u16)
            .collect();
        let curve = ToneReprCurve::Lut(table);
        profile.red_trc = Some(curve.clone());
        profile.green_trc = Some(curve.clone());
        profile.blue_trc = Some(curve);
    }
    Ok(Some((profile, name, hdr.is_some())))
}

/// HDR is shown with its reference white (203 nits, ITU-R BT.2408) as sRGB white; anything
/// brighter clips.
const REFERENCE_WHITE: f64 = 203.0;

/// A PQ (SMPTE ST 2084) signal as linear light relative to reference white.
fn pq(signal: f64) -> f64 {
    let p = signal.powf(1.0 / 78.84375);
    let nits = 10000.0
        * ((p - 0.8359375).max(0.0) / (18.8515625 - 18.6875 * p)).powf(1.0 / 0.1593017578125);
    nits / REFERENCE_WHITE
}

/// An HLG (ARIB STD-B67) signal as linear light relative to reference white, on a 1000-nit
/// display (system gamma 1.2).
fn hlg(signal: f64) -> f64 {
    let scene = if signal <= 0.5 {
        signal * signal / 3.0
    } else {
        (((signal - 0.55991073) / 0.17883277).exp() + 0.28466892) / 12.0
    };
    1000.0 * scene.powf(1.2) / REFERENCE_WHITE
}

/// The samples through `source` to sRGB (grey to grey), with their layout.
fn transform(
    source: &ColorProfile,
    layout: Layout,
    samples: &Samples,
) -> Result<(Samples, Layout), String> {
    use moxcms::Layout as L;
    let space = source.color_space;
    let srgb = ColorProfile::new_srgb();
    let mut grey = ColorProfile::new_gray_with_gamma(1.0);
    grey.gray_trc = srgb.red_trc.clone();
    let (from, to, out, target) = match (layout, space) {
        (Layout::Grey, DataColorSpace::Gray) => (L::Gray, L::Gray, Layout::Grey, &grey),
        (Layout::GreyAlpha, DataColorSpace::Gray) => {
            (L::GrayAlpha, L::GrayAlpha, Layout::GreyAlpha, &grey)
        }
        (Layout::Rgb, DataColorSpace::Rgb) => (L::Rgb, L::Rgb, Layout::Rgb, &srgb),
        (Layout::Rgba, DataColorSpace::Rgb) => (L::Rgba, L::Rgba, Layout::Rgba, &srgb),
        (Layout::Cmyk, DataColorSpace::Cmyk) => (L::Rgba, L::Rgb, Layout::Rgb, &srgb),
        _ => {
            let name = |space| match space {
                DataColorSpace::Gray => "grey".into(),
                space => format!("{space:?}").to_uppercase(),
            };
            let pixels = match layout {
                Layout::Grey | Layout::GreyAlpha => DataColorSpace::Gray,
                Layout::Rgb | Layout::Rgba => DataColorSpace::Rgb,
                Layout::Cmyk => DataColorSpace::Cmyk,
            };
            return Err(format!(
                "it describes {} colour, not {}",
                name(space),
                name(pixels)
            ));
        }
    };
    // Perceptual first; profiles without a perceptual table fall back to relative colorimetric.
    let mut failure = None;
    for rendering_intent in [
        RenderingIntent::Perceptual,
        RenderingIntent::RelativeColorimetric,
    ] {
        let options = TransformOptions {
            rendering_intent,
            ..TransformOptions::default()
        };
        let len = |n: usize| n / from.channels() * to.channels();
        let converted = match samples {
            Samples::U8(s) => source
                .create_transform_8bit(from, target, to, options)
                .and_then(|t| {
                    let mut o = vec![0; len(s.len())];
                    t.transform(s, &mut o).map(|()| Samples::U8(o))
                }),
            Samples::U16(s) => source
                .create_transform_16bit(from, target, to, options)
                .and_then(|t| {
                    let mut o = vec![0; len(s.len())];
                    t.transform(s, &mut o).map(|()| Samples::U16(o))
                }),
        };
        match converted {
            Ok(converted) => return Ok((converted, out)),
            Err(e) => failure = Some(e),
        }
    }
    Err(lower_first(&failure.unwrap().to_string()))
}

/// A profile text tag as one string, English first.
fn text(text: &ProfileText) -> Option<String> {
    match text {
        ProfileText::PlainString(s) => Some(s.clone()),
        ProfileText::Description(d) => Some(d.ascii_string.clone()),
        ProfileText::Localizable(all) => all
            .iter()
            .find(|s| s.language == "en")
            .or(all.first())
            .map(|s| s.value.clone()),
    }
    .map(|s| s.trim_end_matches('\0').trim().to_string())
    .filter(|s| !s.is_empty())
}

fn lower_first(s: &str) -> String {
    let mut chars = s.chars();
    chars
        .next()
        .map(|c| c.to_lowercase().chain(chars).collect())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pixels(layout: Layout, samples: Samples, colour: Option<Colour>) -> Pixels {
        let n = match &samples {
            Samples::U8(s) => s.len(),
            Samples::U16(s) => s.len(),
        };
        Pixels {
            width: (n / layout.channels()) as u32,
            height: 1,
            layout,
            samples,
            bit_depth: 8,
            colour,
            exif: None,
            orientation: 1,
            frames: 1,
            warnings: Vec::new(),
        }
    }

    #[test]
    fn sixteen_bit_samples_round_to_eight() {
        let mut px = pixels(
            Layout::Grey,
            Samples::U16(vec![0, 385, 386, 0x8080, 51400, 65535]),
            None,
        );
        let (image, color) = to_srgb(&mut px);
        assert_eq!(image.data, [0, 1, 2, 128, 200, 255]);
        assert_eq!((color.kind, color.profile), ("srgb", None));
    }

    #[test]
    fn cmyk_without_a_profile_uses_the_naive_formula() {
        let mut px = pixels(
            Layout::Cmyk,
            Samples::U8(vec![10, 60, 120, 200, 0, 0, 0, 0]),
            None,
        );
        let (image, color) = to_srgb(&mut px);
        assert_eq!(
            (image.layout, image.data),
            (Layout::Rgb, vec![45, 0, 0, 255, 255, 255])
        );
        assert_eq!(
            (color.kind, color.profile.as_deref()),
            ("converted", Some("CMYK (no profile)"))
        );
    }

    #[test]
    fn a_grey_profile_keeps_grey_pixels() {
        let linear = ColorProfile::new_gray_with_gamma(1.0).encode().unwrap();
        let mut px = pixels(
            Layout::GreyAlpha,
            Samples::U8(vec![128, 7, 255, 255]),
            Some(Colour::Icc(linear)),
        );
        let (image, color) = to_srgb(&mut px);
        assert_eq!(image.layout, Layout::GreyAlpha);
        assert!(image.data[0].abs_diff(188) <= 1, "{:?}", image.data);
        assert_eq!(&image.data[1..], [7, 255, 255]);
        assert_eq!(color.kind, "converted");
    }

    #[test]
    fn srgb_profiles_are_kept_and_broken_ones_ignored() {
        let srgb = ColorProfile::new_srgb().encode().unwrap();
        let mut px = pixels(
            Layout::Rgb,
            Samples::U8(vec![1, 2, 3]),
            Some(Colour::Icc(srgb.clone())),
        );
        let (image, color) = to_srgb(&mut px);
        assert_eq!(image.data, [1, 2, 3]);
        assert_eq!(
            (color.kind, color.profile.as_deref()),
            ("srgb", Some("sRGB IEC61966-2.1"))
        );

        for (layout, icc, reason) in [
            (Layout::Rgb, vec![0; 200], "invalid ICC profile"),
            (Layout::Cmyk, srgb, "it describes RGB colour, not CMYK"),
        ] {
            let n = layout.channels();
            let mut px = pixels(layout, Samples::U8(vec![0; n]), Some(Colour::Icc(icc)));
            let (image, color) = to_srgb(&mut px);
            assert_eq!(image.data, [255 * u8::from(layout == Layout::Cmyk); 3]);
            assert_eq!(color.kind, "ignored");
            assert_eq!(px.warnings.len(), 1);
            assert_eq!(
                px.warnings[0].message,
                format!(
                    "The embedded colour profile couldn't be used ({reason}), so colours were treated as sRGB."
                )
            );
        }
    }

    fn cicp(primaries: u16, transfer: u16) -> Option<Colour> {
        Some(Colour::Cicp(Cicp {
            primaries,
            transfer,
            matrix: 6,
            full_range: true,
        }))
    }

    #[test]
    fn srgb_and_unspecified_code_points_change_nothing() {
        for (primaries, transfer) in [(1, 13), (2, 2), (1, 2), (2, 13)] {
            let mut px = pixels(
                Layout::Rgba,
                Samples::U8(vec![10, 128, 250, 9]),
                cicp(primaries, transfer),
            );
            let (image, color) = to_srgb(&mut px);
            assert_eq!(image.data, [10, 128, 250, 9]);
            assert_eq!(
                (color.kind, color.profile, px.warnings),
                ("srgb", None, vec![])
            );
        }
    }

    #[test]
    fn wide_gamut_code_points_are_converted_and_named() {
        // Pure P3 and BT.2020 red lie outside sRGB, so they clip to sRGB red.
        for (primaries, transfer, name) in
            [(12, 13, "Display P3"), (9, 1, "BT.2020"), (1, 6, "BT.709")]
        {
            let mut px = pixels(
                Layout::Rgb,
                Samples::U8(vec![255, 0, 0, 255, 255, 255]),
                cicp(primaries, transfer),
            );
            let (image, color) = to_srgb(&mut px);
            assert_eq!(
                (color.kind, color.profile.as_deref()),
                ("converted", Some(name))
            );
            assert!(
                image.data[..3]
                    .iter()
                    .zip([255, 0, 0])
                    .all(|(a, b)| a.abs_diff(b) <= 1),
                "{name}: {:?}",
                image.data
            );
            assert!(
                image.data[3..].iter().all(|&v| v >= 254),
                "{name}: {:?}",
                image.data
            );
            assert!(px.warnings.is_empty());
        }
    }

    #[test]
    fn hdr_reference_white_becomes_srgb_white_and_brighter_clips() {
        // 203 nits: PQ signal 0.5806, HLG signal 0.75.
        for (transfer, white) in [(16, 148), (18, 191)] {
            let mut px = pixels(
                Layout::Rgb,
                Samples::U8(vec![white, white, white, 255, 255, 255, 40, 40, 40]),
                cicp(9, transfer),
            );
            let (image, color) = to_srgb(&mut px);
            assert_eq!(color.kind, "converted");
            assert!(
                image.data[..6].iter().all(|&v| v >= 253),
                "{transfer}: {:?}",
                image.data
            );
            assert!(image.data[6] < 128, "{transfer}: {:?}", image.data);
            assert_eq!(px.warnings, [Warning::hdr_clipped()]);
        }
    }

    #[test]
    fn other_code_points_are_ignored_with_the_reason() {
        for (primaries, transfer, reason) in [
            (5, 13, "colour primaries 5"),
            (12, 8, "transfer characteristics 8"),
        ] {
            let mut px = pixels(
                Layout::Rgb,
                Samples::U8(vec![1, 2, 3]),
                cicp(primaries, transfer),
            );
            let (image, color) = to_srgb(&mut px);
            assert_eq!((image.data, color.kind), (vec![1, 2, 3], "ignored"));
            assert_eq!(
                px.warnings,
                [Warning::icc_ignored(format!("it uses {reason}"))]
            );
        }
    }

    #[test]
    fn colour_a_decoder_converted_is_reported_as_given() {
        let reported = Color {
            kind: "converted",
            profile: Some("Display P3".into()),
        };
        let mut px = pixels(
            Layout::Rgb,
            Samples::U8(vec![1, 2, 3]),
            Some(Colour::Converted(reported.clone())),
        );
        assert_eq!(
            to_srgb(&mut px),
            (
                Image {
                    width: 1,
                    height: 1,
                    layout: Layout::Rgb,
                    data: vec![1, 2, 3]
                },
                reported
            )
        );
    }

    #[test]
    fn colour_converted_elsewhere_is_named_as_to_srgb_would() {
        let p3 = ColorProfile::new_display_p3().encode().unwrap();
        let srgb = ColorProfile::new_srgb().encode().unwrap();
        for (colour, kind, profile) in [
            (None, "srgb", None),
            (Some(Colour::Icc(p3)), "converted", Some("Display P3")),
            (Some(Colour::Icc(srgb)), "srgb", Some("sRGB IEC61966-2.1")),
            (cicp(12, 13), "converted", Some("Display P3")),
            (cicp(1, 13), "srgb", None),
            (cicp(5, 13), "converted", None),
        ] {
            let color = describe(colour.as_ref());
            assert_eq!((color.kind, color.profile.as_deref()), (kind, profile));
        }
    }
}
