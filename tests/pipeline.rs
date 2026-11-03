//! The pixel pipeline on decoded files: colour, size, orientation, outputs, byte limits and EXIF.

mod common;

use std::path::Path;

use common::*;
use image_convert::color::{Cicp, Colour, to_srgb};
use image_convert::detect::{Kind, detect};
use image_convert::resize::size_and_orient;
use image_convert::{
    Format, Image, Layout, Options, Output, Pixels, Samples, jpeg, raster, render, tiff as tif,
};
use jpeg_encoder::ColorType as Jpeg;
use moxcms::ColorProfile;

fn decode(data: &[u8]) -> Pixels {
    match detect(data, Path::new("input")).unwrap() {
        Kind::Jpeg => jpeg::decode(data),
        Kind::Tiff => tif::decode(data),
        kind => raster::decode(data, kind),
    }
    .unwrap()
}

fn srgb(data: &[u8]) -> Image {
    to_srgb(&mut decode(data)).0
}

/// An sRGB sample in linear light.
fn linear(v: u8) -> f64 {
    let c = f64::from(v) / 255.0;
    if c <= 0.04045 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

/// CIELAB (D65) of an sRGB colour.
fn lab([r, g, b]: [u8; 3]) -> [f64; 3] {
    let (r, g, b) = (linear(r), linear(g), linear(b));
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

/// 48 × 24 px of Display P3 colour patches (8 px squares) and their sRGB by the published matrix.
fn p3_patches() -> (Vec<u8>, Vec<u8>) {
    // Display P3 → sRGB, both D65 with the sRGB curve.
    const P3_TO_SRGB: [[f64; 3]; 3] = [
        [1.2249401, -0.2249404, 0.0],
        [-0.0420569, 1.0420571, 0.0],
        [-0.0196376, -0.0786361, 1.0982735],
    ];
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
    let mut p3 = vec![0; 48 * 24 * 3];
    let mut expected = p3.clone();
    for (i, px) in p3.as_chunks_mut::<3>().0.iter_mut().enumerate() {
        let colour = colours[i / 48 / 8 * 6 + i % 48 / 8];
        *px = colour;
        let light = colour.map(linear);
        for (c, row) in P3_TO_SRGB.iter().enumerate() {
            expected[3 * i + c] = encode_curve((0..3).map(|k| row[k] * light[k]).sum());
        }
    }
    (p3, expected)
}

#[test]
fn display_p3_jpeg_and_code_points_match_the_matrix_conversion() {
    let (p3, expected) = p3_patches();
    let icc = ColorProfile::new_display_p3().encode().unwrap();
    let file = jpeg(48, 24, Jpeg::Rgb, &p3, |e| {
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

    // The same patches described by nclx code points (P3 primaries, sRGB transfer).
    px.samples = Samples::U8(p3);
    px.colour = Some(Colour::Cicp(Cicp {
        primaries: 12,
        transfer: 13,
        matrix: 0,
        full_range: true,
    }));
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
fn heif_colour_matches_its_reference() {
    let rgb =
        |data: Vec<u8>| -> Vec<u8> { data.chunks(4).flat_map(|p| &p[..3]).copied().collect() };
    let cases = [
        ("heif-p3.avif", "Display P3", p3_patches().1),
        (
            "heif-adobergb.heic",
            "Compatible with Adobe RGB (1998)",
            srgb(&fixture("color-adobergb-srgb.png")).data,
        ),
    ];
    for (name, profile, expected) in cases {
        let Some(mut px) = os_heif(name) else {
            continue;
        };
        let (image, color) = to_srgb(&mut px);
        assert_eq!(
            (color.kind, color.profile.as_deref()),
            ("converted", Some(profile)),
            "{name}"
        );
        let worst = worst_delta_e(&rgb(image.data), &expected);
        assert!(worst <= 2.0, "{name}: ΔE2000 {worst}");
    }
}

#[test]
fn adobe_rgb_tiff_matches_lcms2() {
    use tiff::encoder::{Compression, colortype::RGB8};
    let mut png = decode(&fixture("color-adobergb.png"));
    let Samples::U8(rgb) = &png.samples else {
        unreachable!()
    };
    let icc = match png.colour.take() {
        Some(Colour::Icc(icc)) => Some(icc),
        _ => None,
    };
    let file = tiff::<RGB8>(
        48,
        32,
        rgb,
        Compression::Lzw,
        tiff::tags::Predictor::Horizontal,
        &[],
        icc.as_deref(),
        1,
    );
    let (image, color) = to_srgb(&mut decode(&file));
    assert_eq!(color.kind, "converted");
    let reference = srgb(&fixture("color-adobergb-srgb.png"));
    let worst = worst_delta_e(&image.data, &reference.data);
    assert!(worst <= 2.0, "ΔE2000 {worst}");
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

/// Bytes no codec can shrink much, from a fixed seed.
fn noise(len: usize) -> Vec<u8> {
    let mut state = 0x2545_f491_4f6c_dd1du64;
    (0..len)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state >> 24) as u8
        })
        .collect()
}

fn codes(out: &Output) -> Vec<&str> {
    out.warnings.iter().map(|w| w.code).collect()
}

#[test]
fn byte_limits_change_format_then_quality_then_size() {
    let screenshot = png(
        1000,
        700,
        png::ColorType::Rgb,
        png::BitDepth::Eight,
        &noise(2_100_000),
        |_| {},
    );
    assert!(screenshot.len() > 2_000_000);
    let limited = |max| {
        let options = Options {
            max_bytes: Some(max),
            ..Options::default()
        };
        convert(&screenshot, &options)
    };
    let out = limited(300_000);
    assert!(out.bytes.len() <= 300_000, "{}", out.bytes.len());
    assert_eq!(codes(&out), ["format_changed", "quality_reduced"]);
    assert_eq!(
        out.warnings[0].message,
        "Saved as JPEG instead of PNG to fit 300000 bytes."
    );
    let out = limited(120_000);
    assert!(out.bytes.len() <= 120_000, "{}", out.bytes.len());
    assert_eq!(
        codes(&out),
        ["format_changed", "quality_reduced", "size_reduced"]
    );
    assert_eq!(
        out.warnings[1].message,
        "Quality lowered to 50 to fit 120000 bytes."
    );
    assert_eq!(
        out.warnings[2].message,
        format!(
            "Shrunk to {}×{} to fit 120000 bytes.",
            out.width, out.height
        )
    );
    assert_eq!(
        (out.format, out.quality, out.resized),
        (Format::Jpeg, Some(50), true)
    );
    assert_eq!(srgb(&out.bytes).width, out.width);

    // Transparency goes to WebP; a given quality is kept; a preset brings its own limit.
    let mut rgba = noise(400 * 300 * 4);
    rgba.iter_mut().skip(3).step_by(4).for_each(|a| *a |= 1);
    let file = png(
        400,
        300,
        png::ColorType::Rgba,
        png::BitDepth::Eight,
        &rgba,
        |_| {},
    );
    let options = Options {
        max_bytes: Some(150_000),
        quality: Some(80),
        ..Options::default()
    };
    let out = convert(&file, &options);
    assert_eq!(out.format, Format::Webp);
    assert_eq!(codes(&out), ["format_changed", "size_reduced"]);
    assert_eq!(out.quality, Some(80));
    let vision = Options {
        fit: image_convert::Fit::Vision,
        ..Options::default()
    };
    assert!(convert(&screenshot, &vision).warnings.is_empty());
}

#[test]
fn an_impossible_limit_gives_cannot_fit() {
    let kind = |data: &[u8]| detect(data, Path::new("input")).unwrap();
    let file = png(
        256,
        256,
        png::ColorType::Rgb,
        png::BitDepth::Eight,
        &noise(196_608),
        |_| {},
    );
    let options = Options {
        max_bytes: Some(1000),
        ..Options::default()
    };
    let err = render(decode(&file), kind(&file), &options).unwrap_err();
    assert_eq!(err.code, "cannot_fit");
    assert!(
        err.message.starts_with(
            "Can't make this image smaller than 1000 bytes; the smallest try was 81×81, "
        ),
        "{}",
        err.message
    );

    // A long strip stops at 12 encodes before its longer side reaches 64 px.
    let strip = png(
        3000,
        400,
        png::ColorType::Rgb,
        png::BitDepth::Eight,
        &noise(3_600_000),
        |_| {},
    );
    let options = Options {
        max_bytes: Some(1000),
        ..to(Format::Png)
    };
    let err = render(decode(&strip), kind(&strip), &options).unwrap_err();
    assert!(
        err.message.contains("the smallest try was 126×16, "),
        "{}",
        err.message
    );
}

/// A little-endian EXIF block with Orientation and a capture date in IFD0.
fn exif_with_date(orientation: u16) -> Vec<u8> {
    let mut exif = b"II*\0\x08\0\0\0\x02\0".to_vec();
    exif.extend([0x12, 0x01, 3, 0, 1, 0, 0, 0]);
    exif.extend(orientation.to_le_bytes());
    exif.extend([0, 0]);
    exif.extend([0x32, 0x01, 2, 0, 20, 0, 0, 0, 38, 0, 0, 0]);
    exif.extend([0; 4]);
    exif.extend(b"2024:05:01 10:00:00\0");
    exif
}

#[test]
fn keep_exif_copies_the_block_with_orientation_one() {
    let keep = |format| Options {
        keep_exif: true,
        ..to(format)
    };
    let source = jpeg(30, 20, Jpeg::Rgb, &gradient(30, 20, 3), |e| {
        e.add_exif_metadata(&exif_with_date(6)).unwrap();
    });
    for format in [Format::Jpeg, Format::Png] {
        let out = convert(&source, &keep(format));
        assert!(out.exif && out.warnings.is_empty(), "{:?}", out.warnings);
        let back = decode(&out.bytes);
        assert_eq!((back.width, back.height, back.orientation), (20, 30, 1));
        assert_eq!(back.exif, Some(exif_with_date(1)));
        if format == Format::Png {
            assert_eq!(chunks(&out.bytes), ["IHDR", "eXIf", "IDAT", "IEND"]);
        }
    }
    assert!(!convert(&source, &to(Format::Jpeg)).exif);

    let mut big = exif_with_date(1);
    big.resize(65_528, 0);
    let large = png(
        4,
        4,
        png::ColorType::Rgb,
        png::BitDepth::Eight,
        &[7; 48],
        |info| {
            info.exif_metadata = Some(big.into());
        },
    );
    assert!(convert(&large, &keep(Format::Png)).exif);
    let plain = jpeg(8, 8, Jpeg::Rgb, &[9; 192], |_| {});
    let gif = gif((2, 2), None, &[(0, 0, 2, 2, vec![0, 1, 2, 3])]);
    for (file, format, reason) in [
        (&source, Format::Webp, "WebP output doesn't carry it"),
        (&large, Format::Jpeg, "it is larger than 64 KB"),
        (&plain, Format::Jpeg, "the source has none"),
        (&gif, Format::Png, "GIF sources aren't supported"),
    ] {
        let out = convert(file, &keep(format));
        assert!(!out.exif);
        assert_eq!(codes(&out), ["exif_not_kept"]);
        assert_eq!(
            out.warnings[0].message,
            format!("EXIF wasn't kept: {reason}.")
        );
    }
}

#[test]
#[ignore = "known bug: IC03"]
fn only_a_real_srgb_profile_skips_the_conversion() {
    use moxcms::{ProfileText, ToneReprCurve};
    let with = |profile: ColorProfile| {
        let icc = profile.encode().unwrap();
        let file = png(
            1,
            1,
            png::ColorType::Rgb,
            png::BitDepth::Eight,
            &[128, 64, 32],
            |info| info.icc_profile = Some(icc.into()),
        );
        let (image, color) = to_srgb(&mut decode(&file));
        (color.kind, image.data)
    };
    assert_eq!(with(ColorProfile::new_srgb()), ("srgb", vec![128, 64, 32]));
    // Linear light under a name that says sRGB, as Krita's and GIMP's linear profiles are named.
    let mut linear = ColorProfile::new_srgb();
    let curve = Some(ToneReprCurve::Parametric(vec![1.0]));
    (linear.red_trc, linear.green_trc, linear.blue_trc) = (curve.clone(), curve.clone(), curve);
    linear.cicp = None;
    linear.description = Some(ProfileText::PlainString("Linear sRGB".into()));
    let (kind, data) = with(linear);
    assert_eq!(kind, "converted");
    // The sRGB encodings of 128, 64 and 32 / 255 in linear light.
    let expected = [188, 137, 99];
    assert!(
        data.iter().zip(expected).all(|(a, b)| a.abs_diff(b) <= 2),
        "{data:?}"
    );
}

#[test]
#[ignore = "known bug: IC11"]
fn lossy_heif_colour_is_converted_once() {
    let Some(mut px) = os_heif("heif-p3-yuv.avif") else {
        return;
    };
    let (image, _) = to_srgb(&mut px);
    let rgb: Vec<u8> = image
        .data
        .chunks(4)
        .flat_map(|p| &p[..3])
        .copied()
        .collect();
    let worst = worst_delta_e(&rgb, &p3_patches().1);
    assert!(worst <= 2.0, "ΔE2000 {worst}");
}
