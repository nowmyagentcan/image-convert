//! The pixel pipeline on decoded files: colour, size, orientation, outputs, byte limits and EXIF.

mod common;

use std::path::Path;

use common::*;
use image_convert::color::to_srgb;
use image_convert::detect::{Kind, detect};
use image_convert::{Image, Layout, Pixels, jpeg, raster};
use jpeg_encoder::ColorType as Jpeg;
use moxcms::ColorProfile;

fn decode(data: &[u8]) -> Pixels {
    match detect(data, Path::new("input")).unwrap() {
        Kind::Jpeg => jpeg::decode(data),
        kind => raster::decode(data, kind),
    }
    .unwrap()
}

fn srgb(data: &[u8]) -> Image {
    to_srgb(&mut decode(data)).0
}

/// CIELAB (D65) of an sRGB colour.
fn lab([r, g, b]: [u8; 3]) -> [f64; 3] {
    let lin = |v: u8| {
        let c = f64::from(v) / 255.0;
        if c <= 0.04045 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    };
    let (r, g, b) = (lin(r), lin(g), lin(b));
    let x = (0.4124564 * r + 0.3575761 * g + 0.1804375 * b) / 0.95047;
    let y = 0.2126729 * r + 0.7151522 * g + 0.0721750 * b;
    let z = (0.0193339 * r + 0.1191920 * g + 0.9503041 * b) / 1.08883;
    let f = |t: f64| {
        if t > 216.0 / 24389.0 {
            t.cbrt()
        } else {
            (24389.0 / 27.0 * t + 16.0) / 116.0
        }
    };
    let (fx, fy, fz) = (f(x), f(y), f(z));
    [116.0 * fy - 16.0, 500.0 * (fx - fy), 200.0 * (fy - fz)]
}

/// CIEDE2000 colour difference.
fn delta_e([l1, a1, b1]: [f64; 3], [l2, a2, b2]: [f64; 3]) -> f64 {
    let p7 = |c: f64| (c.powi(7) / (c.powi(7) + 25f64.powi(7))).sqrt();
    let g = 0.5 * (1.0 - p7((a1.hypot(b1) + a2.hypot(b2)) / 2.0));
    let (a1, a2) = (a1 * (1.0 + g), a2 * (1.0 + g));
    let (c1, c2) = (a1.hypot(b1), a2.hypot(b2));
    let hue = |b: f64, a: f64| b.atan2(a).to_degrees().rem_euclid(360.0);
    let (h1, h2) = (hue(b1, a1), hue(b2, a2));
    let chroma = c1 * c2 != 0.0;
    let dh = match h2 - h1 {
        _ if !chroma => 0.0,
        d if d > 180.0 => d - 360.0,
        d if d < -180.0 => d + 360.0,
        d => d,
    };
    let dh = 2.0 * (c1 * c2).sqrt() * (dh.to_radians() / 2.0).sin();
    let (l, c) = ((l1 + l2) / 2.0, (c1 + c2) / 2.0);
    let h = match h1 + h2 {
        s if !chroma => s,
        s if (h1 - h2).abs() <= 180.0 => s / 2.0,
        s if s < 360.0 => (s + 360.0) / 2.0,
        s => (s - 360.0) / 2.0,
    };
    let cos = |deg: f64| deg.to_radians().cos();
    let t = 1.0 - 0.17 * cos(h - 30.0) + 0.24 * cos(2.0 * h) + 0.32 * cos(3.0 * h + 6.0)
        - 0.20 * cos(4.0 * h - 63.0);
    let rt = -(60.0 * (-((h - 275.0) / 25.0).powi(2)).exp())
        .to_radians()
        .sin()
        * 2.0
        * p7(c);
    let sl = 1.0 + 0.015 * (l - 50.0).powi(2) / (20.0 + (l - 50.0).powi(2)).sqrt();
    let (dl, dc, dh) = (
        (l2 - l1) / sl,
        (c2 - c1) / (1.0 + 0.045 * c),
        dh / (1.0 + 0.015 * c * t),
    );
    (dl * dl + dc * dc + dh * dh + rt * dc * dh).sqrt()
}

/// The largest CIEDE2000 difference between two RGB images of the same size.
fn worst_delta_e(a: &[u8], b: &[u8]) -> f64 {
    assert_eq!(a.len(), b.len());
    a.as_chunks::<3>()
        .0
        .iter()
        .zip(b.as_chunks::<3>().0)
        .map(|(x, y)| delta_e(lab(*x), lab(*y)))
        .fold(0.0, f64::max)
}

#[test]
fn delta_e_matches_the_published_test_pairs() {
    // Sharma, Wu and Dalal (2005), pairs 1 and 17.
    let d = delta_e([50.0, 2.6772, -79.7751], [50.0, 0.0, -82.7485]);
    assert!((d - 2.0425).abs() < 1e-4, "{d}");
    let d = delta_e([50.0, 2.5, 0.0], [73.0, 25.0, -18.0]);
    assert!((d - 27.1492).abs() < 1e-4, "{d}");
}

#[test]
fn display_p3_jpeg_matches_the_matrix_conversion() {
    // Display P3 → sRGB, both D65 with the sRGB curve.
    const P3_TO_SRGB: [[f64; 3]; 3] = [
        [1.2249401, -0.2249404, 0.0],
        [-0.0420569, 1.0420571, 0.0],
        [-0.0196376, -0.0786361, 1.0982735],
    ];
    let decode_curve = |v: u8| {
        let c = f64::from(v) / 255.0;
        if c <= 0.04045 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    };
    let encode_curve = |c: f64| {
        let c = c.clamp(0.0, 1.0);
        let v = if c <= 0.0031308 {
            12.92 * c
        } else {
            1.055 * c.powf(1.0 / 2.4) - 0.055
        };
        (v * 255.0).round() as u8
    };
    let colours: Vec<[u8; 3]> = [40, 120, 210]
        .into_iter()
        .flat_map(|r| [60, 180].map(|g| (r, g)))
        .flat_map(|(r, g)| [50, 140, 220].map(|b| [r, g, b]))
        .collect();
    let (width, height) = (48u16, 24u16);
    let mut p3 = vec![0; usize::from(width) * usize::from(height) * 3];
    let mut expected = p3.clone();
    for (i, px) in p3.as_chunks_mut::<3>().0.iter_mut().enumerate() {
        let (x, y) = (i % usize::from(width), i / usize::from(width));
        let colour = colours[y / 8 * 6 + x / 8];
        *px = colour;
        let linear = colour.map(decode_curve);
        for (c, row) in P3_TO_SRGB.iter().enumerate() {
            expected[3 * i + c] = encode_curve((0..3).map(|k| row[k] * linear[k]).sum());
        }
    }
    let icc = ColorProfile::new_display_p3().encode().unwrap();
    let file = jpeg(width, height, Jpeg::Rgb, &p3, |e| {
        e.set_quality(100);
        e.add_icc_profile(&icc).unwrap();
    });
    let mut px = decode(&file);
    let (image, color) = to_srgb(&mut px);
    assert_eq!(
        (color.kind, color.profile.as_deref()),
        ("converted", Some("Display P3"))
    );
    let worst = worst_delta_e(&image.data, &expected);
    assert!(worst <= 2.0, "ΔE2000 {worst}");
}

#[test]
fn adobe_rgb_and_swop_cmyk_match_lcms2() {
    for (name, profile) in [
        ("adobergb", "Compatible with Adobe RGB (1998)"),
        ("swop", "SWOP TR005 Coated"),
    ] {
        let source = fixture(&format!(
            "color-{name}.{}",
            if name == "swop" { "jpg" } else { "png" }
        ));
        let mut px = decode(&source);
        let (image, color) = to_srgb(&mut px);
        assert_eq!(
            (color.kind, color.profile.as_deref()),
            ("converted", Some(profile))
        );
        let reference = srgb(&fixture(&format!("color-{name}-srgb.png")));
        assert_eq!((image.layout, image.width), (Layout::Rgb, 48));
        let worst = worst_delta_e(&image.data, &reference.data);
        assert!(worst <= 2.0, "{name}: ΔE2000 {worst}");
    }
}

#[test]
fn a_broken_profile_is_ignored_with_a_warning() {
    let file = png(
        2,
        1,
        png::ColorType::Rgb,
        png::BitDepth::Eight,
        &[10, 20, 30, 40, 50, 60],
        |info| info.icc_profile = Some(profile(300).into()),
    );
    let mut px = decode(&file);
    let (image, color) = to_srgb(&mut px);
    assert_eq!(
        (color.kind, image.data),
        ("ignored", vec![10, 20, 30, 40, 50, 60])
    );
    assert_eq!(px.warnings[0].code, "icc_ignored");
}
