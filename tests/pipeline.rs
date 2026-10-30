//! The pixel pipeline on decoded files: colour, size, orientation, outputs, byte limits and EXIF.

mod common;

use std::path::Path;

use common::*;
use image_convert::color::to_srgb;
use image_convert::detect::{Kind, detect};
use image_convert::resize::size_and_orient;
use image_convert::{Format, Image, Layout, Options, Output, Pixels, jpeg, raster, render};
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

fn mean_diff(a: &[u8], b: &[u8]) -> f64 {
    assert_eq!(a.len(), b.len());
    let sum: f64 = a
        .iter()
        .zip(b)
        .map(|(x, y)| f64::from(x.abs_diff(*y)))
        .sum();
    sum / a.len() as f64
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

/// The stored pixels that EXIF `orientation` turns into `upright`, by the image crate's inverse.
fn stored(upright: &image::RgbImage, orientation: u8) -> image::RgbImage {
    let inverse = match orientation {
        6 => 8,
        8 => 6,
        o => o,
    };
    let mut img = image::DynamicImage::ImageRgb8(upright.clone());
    img.apply_orientation(image::metadata::Orientation::from_exif(inverse).unwrap());
    img.into_rgb8()
}

#[test]
fn every_orientation_comes_out_upright_and_pixel_equal() {
    let upright =
        image::RgbImage::from_fn(5, 3, |x, y| image::Rgb([x as u8 * 50, y as u8 * 100, 7]));
    for orientation in 1..=8 {
        let raw = stored(&upright, orientation);
        let file = png(
            raw.width(),
            raw.height(),
            png::ColorType::Rgb,
            png::BitDepth::Eight,
            &raw,
            |info| {
                info.exif_metadata = Some(exif_orientation(orientation.into()).into());
            },
        );
        let mut px = decode(&file);
        assert_eq!(px.orientation, orientation);
        let image = size_and_orient(to_srgb(&mut px).0, px.orientation, &Options::default());
        assert_eq!(
            (image.width, image.height),
            (5, 3),
            "orientation {orientation}"
        );
        assert_eq!(
            image.data,
            upright.as_raw().as_slice(),
            "orientation {orientation}"
        );

        let file = jpeg(
            raw.width() as u16,
            raw.height() as u16,
            Jpeg::Rgb,
            &raw,
            |e| {
                e.set_quality(100);
                e.add_exif_metadata(&exif_orientation(orientation.into()))
                    .unwrap();
            },
        );
        let mut px = decode(&file);
        let image = size_and_orient(to_srgb(&mut px).0, px.orientation, &Options::default());
        assert_eq!(
            (image.width, image.height),
            (5, 3),
            "jpeg orientation {orientation}"
        );
        let diff = mean_diff(&image.data, upright.as_raw());
        assert!(diff <= 2.0, "jpeg orientation {orientation}: {diff}");
    }
}

#[test]
fn sizes_are_for_the_upright_image_and_resizing_comes_first() {
    let upright =
        image::RgbImage::from_fn(30, 40, |x, y| image::Rgb([x as u8 * 8, y as u8 * 6, 90]));
    let options = Options {
        max_side: Some(20),
        ..Options::default()
    };
    let direct = size_and_orient(
        Image {
            width: 30,
            height: 40,
            layout: Layout::Rgb,
            data: upright.to_vec(),
        },
        1,
        &options,
    );
    for orientation in [6, 7] {
        let raw = stored(&upright, orientation);
        let image = Image {
            width: raw.width(),
            height: raw.height(),
            layout: Layout::Rgb,
            data: raw.into_raw(),
        };
        let image = size_and_orient(image, orientation, &options);
        assert_eq!((image.width, image.height), (15, 20));
        assert!(mean_diff(&image.data, &direct.data) <= 1.0);
    }
}

fn convert(data: &[u8], options: &Options) -> Output {
    let kind = detect(data, Path::new("input")).unwrap();
    render(decode(data), kind, options).unwrap()
}

fn to(format: Format) -> Options {
    Options {
        to: Some(format),
        ..Options::default()
    }
}

/// The marker of every JPEG segment before the scan, and the SOF's sampling factors.
fn jpeg_markers(data: &[u8]) -> (Vec<u8>, Vec<u8>) {
    let (mut markers, mut sampling, mut at) = (Vec::new(), Vec::new(), 2);
    while data[at + 1] != 0xda {
        let marker = data[at + 1];
        if marker == 0xc0 {
            let components = usize::from(data[at + 9]);
            sampling = (0..components).map(|i| data[at + 11 + 3 * i]).collect();
        }
        markers.push(marker);
        at += 2 + usize::from(u16::from_be_bytes([data[at + 2], data[at + 3]]));
    }
    (markers, sampling)
}

/// The chunk types of a PNG, or the chunk ids of a WebP.
fn chunks(data: &[u8]) -> Vec<String> {
    let png = data.starts_with(b"\x89PNG");
    let (mut at, mut out) = (if png { 8 } else { 12 }, Vec::new());
    while at + 8 <= data.len() {
        let (len, id) = if png {
            (
                u32::from_be_bytes(data[at..at + 4].try_into().unwrap()),
                &data[at + 4..at + 8],
            )
        } else {
            (
                u32::from_le_bytes(data[at + 4..at + 8].try_into().unwrap()),
                &data[at..at + 4],
            )
        };
        out.push(String::from_utf8_lossy(id).into_owned());
        at += 8 + len as usize + if png { 4 } else { len as usize & 1 };
    }
    out
}

#[test]
fn outputs_carry_no_metadata() {
    let rgb = gradient(32, 24, 3);
    let rgba = gradient(32, 24, 4);
    let icc = ColorProfile::new_srgb().encode().unwrap();
    let sources = [
        jpeg(32, 24, Jpeg::Rgb, &rgb, |e| {
            e.add_icc_profile(&icc).unwrap();
            e.add_exif_metadata(&exif_orientation(1)).unwrap();
            e.add_app_segment(13, b"Photoshop 3.0\0".to_vec()).unwrap();
        }),
        png(
            32,
            24,
            png::ColorType::Rgba,
            png::BitDepth::Eight,
            &rgba,
            |info| {
                info.icc_profile = Some(icc.clone().into());
                info.exif_metadata = Some(exif_orientation(1).into());
                info.uncompressed_latin1_text
                    .push(png::text_metadata::TEXtChunk::new("Comment", "hello"));
            },
        ),
    ];
    for source in &sources {
        let out = convert(source, &to(Format::Png));
        assert_eq!(chunks(&out.bytes), ["IHDR", "IDAT", "IEND"]);
        let out = convert(source, &to(Format::Jpeg));
        let markers = jpeg_markers(&out.bytes).0;
        assert_eq!(markers[0], 0xe0, "JFIF APP0 first");
        assert!(
            markers[1..]
                .iter()
                .all(|m| !(0xe0..=0xef).contains(m) && *m != 0xfe),
            "{markers:x?}"
        );
        let out = convert(source, &to(Format::Webp));
        let ids = chunks(&out.bytes);
        assert!(
            ids == ["VP8 "] || ids == ["VP8X", "ALPH", "VP8 "],
            "{ids:?}"
        );
    }
}

#[test]
fn auto_picks_png_for_drawings_and_transparency_and_jpeg_for_photos() {
    let photo = jpeg(16, 16, Jpeg::Rgb, &gradient(16, 16, 3), |_| {});
    let screenshot = png(
        16,
        16,
        png::ColorType::Rgb,
        png::BitDepth::Eight,
        &gradient(16, 16, 3),
        |_| {},
    );
    let mut rgba = [90, 120, 150, 255].repeat(256);
    let opaque = webp_lossless(16, 16, &rgba);
    rgba[3] = 0;
    let transparent = webp_lossless(16, 16, &rgba);
    for (file, format) in [
        (&photo, Format::Jpeg),
        (&screenshot, Format::Png),
        (&opaque, Format::Jpeg),
        (&transparent, Format::Png),
    ] {
        let out = convert(file, &Options::default());
        assert_eq!(out.format, format);
        assert_eq!(out.quality, (format == Format::Jpeg).then_some(85));
    }
}

#[test]
fn grey_stays_grey_and_jpeg_chroma_follows_the_quality() {
    let grey = png(
        8,
        8,
        png::ColorType::Grayscale,
        png::BitDepth::Eight,
        &[90; 64],
        |_| {},
    );
    let out = convert(&grey, &Options::default());
    assert_eq!(out.bytes[25], 0, "PNG colour type");
    let out = convert(&grey, &to(Format::Jpeg));
    assert_eq!(jpeg_markers(&out.bytes).1, [0x11]);

    let rgb = png(
        16,
        16,
        png::ColorType::Rgb,
        png::BitDepth::Eight,
        &gradient(16, 16, 3),
        |_| {},
    );
    for (quality, luma) in [(89, 0x22), (90, 0x11)] {
        let options = Options {
            quality: Some(quality),
            ..to(Format::Jpeg)
        };
        let out = convert(&rgb, &options);
        assert_eq!(jpeg_markers(&out.bytes).1, [luma, 0x11, 0x11]);
        assert_eq!(out.quality, Some(quality));
    }
}

#[test]
fn jpeg_flattens_transparency_over_the_background() {
    // The top 8×8 block is fully transparent; quality 95 keeps chroma at full resolution.
    let mut rgba = [200, 40, 40, 255].repeat(256);
    for row in 0..8 {
        rgba[64 * row..64 * row + 32].fill(0);
    }
    let file = png(
        16,
        16,
        png::ColorType::Rgba,
        png::BitDepth::Eight,
        &rgba,
        |_| {},
    );
    for (background, name, fill) in [
        ([255; 3], "white", [255; 3]),
        ([0, 0, 255], "#0000ff", [0, 0, 255]),
    ] {
        let options = Options {
            background,
            quality: Some(95),
            ..to(Format::Jpeg)
        };
        let out = convert(&file, &options);
        assert_eq!(out.warnings[0].code, "transparency_flattened");
        assert_eq!(
            out.warnings[0].message,
            format!("JPEG has no transparency, so transparent areas were filled with {name}.")
        );
        let back = srgb(&out.bytes);
        assert_eq!(back.layout, Layout::Rgb);
        assert!(
            back.data[..3]
                .iter()
                .zip(fill)
                .all(|(a, b)| a.abs_diff(b) <= 3),
            "{:?}",
            &back.data[..3]
        );
    }
    let opaque = png(
        8,
        8,
        png::ColorType::Rgba,
        png::BitDepth::Eight,
        &[9, 9, 9, 255].repeat(64),
        |_| {},
    );
    assert!(convert(&opaque, &to(Format::Jpeg)).warnings.is_empty());
}

#[test]
fn webp_keeps_alpha_for_an_independent_decoder() {
    let mut rgba = gradient(20, 10, 4);
    for px in rgba.as_chunks_mut::<4>().0.iter_mut().take(100) {
        px[3] = 0;
    }
    let file = png(
        20,
        10,
        png::ColorType::Rgba,
        png::BitDepth::Eight,
        &rgba,
        |_| {},
    );
    let out = convert(&file, &to(Format::Webp));
    let back = image::load_from_memory_with_format(&out.bytes, image::ImageFormat::WebP)
        .unwrap()
        .to_rgba8();
    assert_eq!((back.width(), back.height()), (20, 10));
    assert_eq!(
        (back.get_pixel(0, 0)[3], back.get_pixel(19, 9)[3]),
        (0, 255)
    );

    // WebP holds at most 16,383 px a side.
    let wide = png(
        20000,
        2,
        png::ColorType::Rgb,
        png::BitDepth::Eight,
        &[128; 120_000],
        |_| {},
    );
    let out = convert(&wide, &to(Format::Webp));
    assert_eq!((out.width, out.height, out.resized), (16383, 1, true));
}

#[test]
fn render_sizes_the_upright_image_and_reports_it() {
    let file = jpeg(400, 300, Jpeg::Rgb, &gradient(400, 300, 3), |e| {
        e.add_exif_metadata(&exif_orientation(6)).unwrap();
    });
    let options = Options {
        max_side: Some(100),
        ..Options::default()
    };
    let out = convert(&file, &options);
    assert_eq!((out.width, out.height, out.resized), (75, 100, true));
    assert_eq!(out.color.kind, "srgb");
    let back = decode(&out.bytes);
    assert_eq!((back.width, back.height, back.orientation), (75, 100, 1));
    let out = convert(&file, &Options::default());
    assert_eq!((out.width, out.height, out.resized), (300, 400, false));
}
