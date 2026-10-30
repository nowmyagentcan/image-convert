//! Colour to sRGB: ICC profiles through moxcms, CMYK without one by the naive formula, and 16-bit
//! samples down to 8.

use moxcms::{ColorProfile, DataColorSpace, ProfileText, RenderingIntent, TransformOptions};

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

/// Takes the samples out of `px` as an 8-bit sRGB image; an unusable profile adds `icc_ignored`.
pub fn to_srgb(px: &mut Pixels) -> (Image, Color) {
    let mut samples = std::mem::replace(&mut px.samples, Samples::U8(Vec::new()));
    let mut layout = px.layout;
    let mut color = Color {
        kind: "srgb",
        profile: None,
    };
    if let Some(icc) = px.icc.take() {
        match through_profile(&icc, layout, &samples) {
            Ok((name, None)) => color.profile = name,
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
    use moxcms::Layout as L;
    let source = ColorProfile::new_from_slice(icc).map_err(|e| lower_first(&e.to_string()))?;
    let name = source.description.as_ref().and_then(text);
    let space = source.color_space;
    if space == DataColorSpace::Rgb
        && layout != Layout::Cmyk
        && name.as_deref().is_some_and(|n| n.contains("sRGB"))
    {
        return Ok((name, None));
    }
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
            Ok(converted) => return Ok((name, Some((converted, out)))),
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

    fn pixels(layout: Layout, samples: Samples, icc: Option<Vec<u8>>) -> Pixels {
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
            icc,
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
            Some(linear),
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
        let mut px = pixels(Layout::Rgb, Samples::U8(vec![1, 2, 3]), Some(srgb.clone()));
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
            let mut px = pixels(layout, Samples::U8(vec![0; n]), Some(icc));
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
}
