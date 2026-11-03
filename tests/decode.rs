//! Every accepted and refused variant of the core formats, from files written in the tests or by
//! tests/fixtures/make.sh.

mod common;

use std::alloc::{GlobalAlloc, Layout as Alloc, System};
use std::cell::Cell;
use std::path::Path;

use common::*;
use image_convert::color::{Cicp, Colour};
use image_convert::detect::{Kind, detect};
use image_convert::error::Error;
use image_convert::{
    Layout, Options, Pixels, Samples, bmff, engine, jpeg, jxl, raster, raw, svg, tiff as tif,
};
use jpeg_encoder::{ColorType as Jpeg, SamplingFactor};
use png::{BitDepth, ColorType as Png};

/// Counts this thread's live heap bytes, so a test can bound what one decode allocates.
struct Counting;

thread_local! {
    static LIVE: Cell<usize> = const { Cell::new(0) };
    static PEAK: Cell<usize> = const { Cell::new(0) };
}

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Alloc) -> *mut u8 {
        let _ = LIVE.try_with(|live| {
            live.set(live.get() + layout.size());
            let _ = PEAK.try_with(|peak| peak.set(peak.get().max(live.get())));
        });
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Alloc) {
        let _ = LIVE.try_with(|live| live.set(live.get().saturating_sub(layout.size())));
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static COUNTING: Counting = Counting;

/// Detects the type, then decodes with the reader for it, as the engine does.
fn decode(data: &[u8]) -> Result<Pixels, Error> {
    match detect(data, Path::new("input"))? {
        Kind::Jpeg => jpeg::decode(data),
        Kind::Tiff => tif::decode(data),
        Kind::Jxl => jxl::decode(data).map(|(px, _)| px),
        kind => raster::decode(data, kind),
    }
}

fn u8s(px: &Pixels) -> &[u8] {
    match &px.samples {
        Samples::U8(s) => s,
        Samples::U16(_) => panic!("16-bit samples"),
    }
}

/// The colour of each pixel, alpha left out.
fn rgb(px: &Pixels) -> Vec<[u8; 3]> {
    let n = px.layout.channels();
    u8s(px).chunks(n).map(|p| [p[0], p[1], p[2]]).collect()
}

fn alpha(px: &Pixels) -> Vec<u8> {
    assert!(px.layout.has_alpha());
    u8s(px)
        .chunks(px.layout.channels())
        .map(|p| p[p.len() - 1])
        .collect()
}

/// Palette indices 0..6 into `COLORS`, as BMP rows of 3 pixels, top row first.
const INDICES: [[u8; 3]; 2] = [[0, 1, 2], [3, 4, 5]];

fn expected_colors() -> Vec<[u8; 3]> {
    INDICES
        .iter()
        .flatten()
        .map(|&i| COLORS[i as usize])
        .collect()
}

#[test]
fn every_png_colour_type_and_bit_depth() {
    let (w, h) = (5u32, 3u32);
    let cases: [(Png, &[u8], Layout); 5] = [
        (Png::Grayscale, &[1, 2, 4, 8, 16], Layout::Grey),
        (Png::GrayscaleAlpha, &[8, 16], Layout::GreyAlpha),
        (Png::Rgb, &[8, 16], Layout::Rgb),
        (Png::Rgba, &[8, 16], Layout::Rgba),
        (Png::Indexed, &[1, 2, 4, 8], Layout::Rgb),
    ];
    for (color, depths, layout) in cases {
        for &depth in depths {
            let row_bytes = (w as usize * color.samples() * depth as usize).div_ceil(8);
            let data: Vec<u8> = (0..row_bytes * h as usize)
                .map(|i| (i * 73 + 41) as u8)
                .collect();
            let entries = 1usize << depth.min(8);
            let palette: Vec<u8> = (0..entries * 3).map(|i| (i * 29) as u8).collect();
            let file = png(
                w,
                h,
                color,
                BitDepth::from_u8(depth).unwrap(),
                &data,
                |info| {
                    if color == Png::Indexed {
                        info.palette = Some(palette.clone().into());
                    }
                },
            );
            let px = decode(&file).unwrap();
            let label = format!("{color:?} {depth}");
            assert_eq!(
                (px.width, px.height, px.layout, px.bit_depth),
                (w, h, layout, depth),
                "{label}"
            );

            // Each pixel's samples as stored: `depth` bits each, rows padded to a byte.
            let stored: Vec<u32> = (0..h as usize)
                .flat_map(|y| {
                    let row = &data[y * row_bytes..][..row_bytes];
                    (0..w as usize * color.samples()).map(move |i| match depth {
                        16 => u16::from_be_bytes([row[2 * i], row[2 * i + 1]]).into(),
                        _ => {
                            let bit = i * depth as usize;
                            u32::from(row[bit / 8] >> (8 - depth as usize - bit % 8))
                                & ((1 << depth) - 1)
                        }
                    })
                })
                .collect();
            match (&px.samples, color) {
                (Samples::U16(s), _) => assert_eq!(
                    s.iter().map(|&v| v.into()).collect::<Vec<u32>>(),
                    stored,
                    "{label}"
                ),
                (Samples::U8(s), Png::Indexed) => {
                    let want: Vec<u8> = stored
                        .iter()
                        .flat_map(|&i| palette[i as usize * 3..][..3].to_vec())
                        .collect();
                    assert_eq!(s, &want, "{label}");
                }
                (Samples::U8(s), _) => {
                    let scale = 255 / ((1u32 << depth) - 1);
                    assert_eq!(
                        s.iter().map(|&v| v.into()).collect::<Vec<u32>>(),
                        stored.iter().map(|v| v * scale).collect::<Vec<_>>(),
                        "{label}"
                    );
                }
            }
        }
    }
}

#[test]
fn png_palette_transparency_profile_exif_and_interlace() {
    let palette: Vec<u8> = COLORS.iter().flatten().copied().collect();
    let indices = [0u8, 1, 2, 3, 4, 5];
    let icc = profile(3000);
    let file = png(3, 2, Png::Indexed, BitDepth::Eight, &indices, |info| {
        info.palette = Some(palette.into());
        info.trns = Some(vec![0, 128].into());
        info.icc_profile = Some(icc.clone().into());
        info.exif_metadata = Some(exif_orientation(6).into());
    });
    let px = decode(&file).unwrap();
    assert_eq!(px.layout, Layout::Rgba);
    assert_eq!(rgb(&px), COLORS.to_vec());
    assert_eq!(alpha(&px), [0, 128, 255, 255, 255, 255]);
    assert_eq!(
        (px.colour, px.exif, px.orientation),
        (Some(Colour::Icc(icc)), Some(exif_orientation(6)), 6)
    );

    let interlaced = decode(&fixture("png-adam7.png")).unwrap();
    let plain = decode(&fixture("png-adam7-plain.png")).unwrap();
    assert_eq!((interlaced.width, interlaced.height), (9, 7));
    assert_eq!(interlaced.samples, plain.samples);
}

#[test]
fn apng_gives_its_default_image_and_a_warning() {
    let frames: Vec<Vec<u8>> = (0..3).map(|i| COLORS[i].repeat(4)).collect();
    let px = decode(&apng(2, 2, &frames)).unwrap();
    assert_eq!(rgb(&px), vec![COLORS[0]; 4]);
    assert_eq!(px.frames, 3);
    assert_eq!(px.warnings[0].code, "first_frame_only");
}

#[test]
fn gif_first_frame_on_its_screen_with_transparency() {
    let px = decode(&gif((3, 2), None, &[(0, 0, 3, 2, vec![0, 1, 2, 3, 4, 5])])).unwrap();
    assert_eq!((px.layout, rgb(&px)), (Layout::Rgba, COLORS.to_vec()));
    assert_eq!((px.frames, px.warnings.len()), (1, 0));

    let frames = [(1, 1, 2, 2, vec![0, 5, 1, 2]), (0, 0, 4, 3, vec![2; 12])];
    let px = decode(&gif((4, 3), Some(5), &frames)).unwrap();
    assert_eq!((px.width, px.height, px.frames), (4, 3, 2));
    let a = alpha(&px);
    let opaque: Vec<usize> = (0..12).filter(|&i| a[i] == 255).collect();
    assert_eq!(opaque, [5, 9, 10]);
    let colours = rgb(&px);
    assert_eq!(
        [colours[5], colours[9], colours[10]],
        [COLORS[0], COLORS[1], COLORS[2]]
    );
    assert_eq!(px.warnings[0].code, "first_frame_only");
}

#[test]
fn bmp_every_bit_depth_header_and_row_order() {
    let rows = |bytes: fn(u8) -> Vec<u8>| -> Vec<Vec<u8>> {
        INDICES
            .iter()
            .map(|row| row.iter().flat_map(|&i| bytes(i)).collect())
            .collect()
    };
    let bgr = |i: u8| {
        let [r, g, b] = COLORS[i as usize];
        vec![b, g, r]
    };
    let bgrx = |i: u8| {
        let [r, g, b] = COLORS[i as usize];
        vec![b, g, r, 0]
    };
    let w555 = |i: u8| {
        let [r, g, b] = COLORS[i as usize].map(|c| u16::from(c >> 3));
        ((r << 10) | (g << 5) | b).to_le_bytes().to_vec()
    };
    let w565 = |i: u8| {
        let [r, g, b] = COLORS[i as usize].map(u16::from);
        (((r >> 3) << 11) | ((g >> 2) << 5) | (b >> 3))
            .to_le_bytes()
            .to_vec()
    };
    let none = [0; 4];
    let masks565 = [0xf800, 0x07e0, 0x001f, 0];
    let packed = |bits: usize| -> Vec<Vec<u8>> {
        INDICES
            .iter()
            .map(|row| {
                let mut out = vec![0u8; (3 * bits).div_ceil(8)];
                for (x, &i) in row.iter().enumerate() {
                    out[x * bits / 8] |= i << (8 - bits - x * bits % 8);
                }
                out
            })
            .collect()
    };
    let cases: Vec<(&str, Vec<u8>)> = vec![
        (
            "24-bit",
            bmp(40, (3, 2), 24, 0, none, &[], &bmp_rows(&rows(bgr), false)),
        ),
        (
            "24-bit top-down",
            bmp(40, (3, -2), 24, 0, none, &[], &bmp_rows(&rows(bgr), true)),
        ),
        (
            "24-bit core",
            bmp(12, (3, 2), 24, 0, none, &[], &bmp_rows(&rows(bgr), false)),
        ),
        (
            "32-bit",
            bmp(40, (3, 2), 32, 0, none, &[], &bmp_rows(&rows(bgrx), false)),
        ),
        (
            "16-bit",
            bmp(40, (3, 2), 16, 0, none, &[], &bmp_rows(&rows(w555), false)),
        ),
        (
            "16-bit bitfields",
            bmp(
                40,
                (3, 2),
                16,
                3,
                masks565,
                &[],
                &bmp_rows(&rows(w565), false),
            ),
        ),
        (
            "8-bit",
            bmp(
                40,
                (3, 2),
                8,
                0,
                none,
                &COLORS,
                &bmp_rows(&packed(8), false),
            ),
        ),
        (
            "4-bit",
            bmp(
                40,
                (3, 2),
                4,
                0,
                none,
                &COLORS,
                &bmp_rows(&packed(4), false),
            ),
        ),
        (
            "8-bit core",
            bmp(
                12,
                (3, 2),
                8,
                0,
                none,
                &COLORS,
                &bmp_rows(&packed(8), false),
            ),
        ),
        (
            "RLE8",
            bmp(
                40,
                (3, 2),
                8,
                1,
                none,
                &COLORS,
                &[1, 3, 1, 4, 1, 5, 0, 0, 1, 0, 1, 1, 1, 2, 0, 0, 0, 1],
            ),
        ),
        (
            "RLE4",
            bmp(
                40,
                (3, 2),
                4,
                2,
                none,
                &COLORS,
                &[2, 0x34, 1, 0x50, 0, 0, 2, 0x01, 1, 0x20, 0, 0, 0, 1],
            ),
        ),
    ];
    for (name, file) in cases {
        let px = decode(&file).unwrap_or_else(|e| panic!("{name}: {e:?}"));
        assert_eq!((px.width, px.height), (3, 2), "{name}");
        assert_eq!(rgb(&px), expected_colors(), "{name}");
    }

    let mono = bmp(
        40,
        (3, 2),
        1,
        0,
        none,
        &[[0, 0, 0], [255, 255, 255]],
        &bmp_rows(&[vec![0b1010_0000], vec![0b0100_0000]], false),
    );
    let px = decode(&mono).unwrap();
    assert_eq!(
        rgb(&px).iter().map(|c| c[0]).collect::<Vec<_>>(),
        [255, 0, 255, 0, 255, 0]
    );

    let bgra = |i: u8| {
        let [r, g, b] = COLORS[i as usize];
        vec![b, g, r, 40 * i]
    };
    let argb = [0x00ff_0000, 0x0000_ff00, 0x0000_00ff, 0xff00_0000];
    for header in [56, 108, 124] {
        let px = decode(&bmp(
            header,
            (3, 2),
            32,
            3,
            argb,
            &[],
            &bmp_rows(&rows(bgra), false),
        ))
        .unwrap();
        assert_eq!(
            (rgb(&px), alpha(&px)),
            (expected_colors(), vec![0, 40, 80, 120, 160, 200]),
            "{header}"
        );
    }
}

#[test]
fn ico_takes_the_largest_entry_then_the_deepest() {
    let solid = |side: u32, c: usize| {
        let [r, g, b] = COLORS[c];
        let pixels = [r, g, b, 255].repeat((side * side) as usize);
        png(side, side, Png::Rgba, BitDepth::Eight, &pixels, |_| {})
    };
    let file = ico(&[
        (32, 32, 32, solid(32, 0)),
        (48, 48, 8, solid(48, 2)),
        (48, 48, 32, solid(48, 1)),
        (16, 16, 32, solid(16, 3)),
    ]);
    let px = decode(&file).unwrap();
    assert_eq!((px.width, px.height, rgb(&px)[0]), (48, 48, COLORS[1]));

    // A 2×2 BMP entry: a doubled-height info header, BGRA rows bottom-up, then the AND mask.
    let mut entry = 40u32.to_le_bytes().to_vec();
    entry.extend(2i32.to_le_bytes());
    entry.extend(4i32.to_le_bytes());
    entry.extend([1, 0, 32, 0]);
    entry.extend([0; 24]);
    for row in [[3u8, 4], [0, 1]] {
        for i in row {
            let [r, g, b] = COLORS[i as usize];
            entry.extend([b, g, r, 255]);
        }
    }
    entry.extend([0; 8]);
    let px = decode(&ico(&[(2, 2, 32, entry)])).unwrap();
    assert_eq!(rgb(&px), [COLORS[0], COLORS[1], COLORS[3], COLORS[4]]);
    assert_eq!(alpha(&px), [255; 4]);
}

#[test]
fn webp_lossy_lossless_alpha_metadata_and_animation() {
    let rgba: Vec<u8> = (0..12)
        .flat_map(|i| {
            [
                COLORS[i % 6][0],
                COLORS[i % 6][1],
                COLORS[i % 6][2],
                (i * 20) as u8,
            ]
        })
        .collect();
    let lossless = webp_lossless(4, 3, &rgba);
    let px = decode(&lossless).unwrap();
    assert_eq!((px.layout, u8s(&px)), (Layout::Rgba, &rgba[..]));

    let source = gradient(32, 32, 3);
    let lossy = image_convert::webp::encode(&source, 32, 32, false, 90).unwrap();
    let px = decode(&lossy).unwrap();
    assert_eq!(px.layout, Layout::Rgb);
    assert!(mean_diff(u8s(&px), &source) < 4.0);

    let with_alpha: Vec<u8> = source
        .chunks(3)
        .enumerate()
        .flat_map(|(i, p)| [p[0], p[1], p[2], if i % 32 < 16 { 255 } else { 0 }])
        .collect();
    let px = decode(&image_convert::webp::encode(&with_alpha, 32, 32, true, 90).unwrap()).unwrap();
    assert_eq!(
        alpha(&px),
        with_alpha.chunks(4).map(|p| p[3]).collect::<Vec<_>>()
    );

    // VP8X with ICCP and EXIF around the lossless bitstream.
    let icc = profile(500);
    let mut vp8x = vec![0x20 | 0x10 | 0x08, 0, 0, 0];
    vp8x.extend(u24(3));
    vp8x.extend(u24(2));
    let file = riff_webp(&[
        chunk(b"VP8X", &vp8x),
        chunk(b"ICCP", &icc),
        lossless[12..].to_vec(),
        chunk(b"EXIF", &exif_orientation(6)),
    ]);
    let px = decode(&file).unwrap();
    assert_eq!(
        (px.colour, px.exif, px.orientation),
        (Some(Colour::Icc(icc)), Some(exif_orientation(6)), 6)
    );

    // Two animation frames of 4×3: red, then blue.
    let frame = |c: usize| {
        let mut anmf = [u24(0), u24(0), u24(3), u24(2), u24(100)].concat();
        anmf.push(0x02); // no blending
        anmf.extend(
            &webp_lossless(
                4,
                3,
                &[COLORS[c][0], COLORS[c][1], COLORS[c][2], 255].repeat(12),
            )[12..],
        );
        chunk(b"ANMF", &anmf)
    };
    let mut vp8x = vec![0x02 | 0x10, 0, 0, 0];
    vp8x.extend(u24(3));
    vp8x.extend(u24(2));
    let file = riff_webp(&[
        chunk(b"VP8X", &vp8x),
        chunk(b"ANIM", &[0, 0, 0, 0, 0, 0]),
        frame(0),
        frame(2),
    ]);
    let px = decode(&file).unwrap();
    assert_eq!(
        (rgb(&px)[0], px.frames, px.warnings[0].code),
        (COLORS[0], 2, "first_frame_only")
    );
}

#[test]
fn jpeg_grey_rgb_every_subsampling_and_progressive() {
    let grey = gradient(32, 24, 1);
    let px = decode(&jpeg(32, 24, Jpeg::Luma, &grey, |_| {})).unwrap();
    assert_eq!((px.layout, px.bit_depth), (Layout::Grey, 8));
    assert!(mean_diff(u8s(&px), &grey) < 2.0);

    let source = gradient(32, 24, 3);
    let factors = [
        (SamplingFactor::F_1_1, None),
        (SamplingFactor::F_2_1, None),
        (SamplingFactor::F_1_2, None),
        (SamplingFactor::F_2_2, None),
        (SamplingFactor::F_4_1, None),
        (SamplingFactor::F_4_2, Some("4×2")),
        (SamplingFactor::F_1_4, Some("1×4")),
        (SamplingFactor::F_2_4, Some("2×4")),
    ];
    for (factor, refused) in factors {
        for progressive in [false, true] {
            let file = jpeg(32, 24, Jpeg::Rgb, &source, |e| {
                e.set_sampling_factor(factor);
                e.set_progressive(progressive);
            });
            if let Some(sampling) = refused {
                let message = format!("JPEG with {sampling} sampling factors isn't supported.");
                assert_eq!(decode(&file).unwrap_err().message, message);
                continue;
            }
            let px = decode(&file).unwrap();
            assert_eq!((px.width, px.height, px.layout), (32, 24, Layout::Rgb));
            let diff = mean_diff(u8s(&px), &source);
            assert!(diff < 8.0, "{factor:?} progressive {progressive}: {diff}");
        }
    }

    // SOF1 (extended sequential) is baseline's superset: the same file with its marker changed.
    let mut extended = jpeg(32, 24, Jpeg::Rgb, &source, |_| {});
    let at = extended.windows(2).position(|w| w == [0xff, 0xc0]).unwrap();
    extended[at + 1] = 0xc1;
    assert!(mean_diff(u8s(&decode(&extended).unwrap()), &source) < 4.0);
}

#[test]
fn jpeg_cmyk_and_ycck_from_other_encoders_come_out_as_ink() {
    for name in ["jpeg-cmyk-pil.jpg", "jpeg-cmyk-im.jpg"] {
        let px = decode(&fixture(name)).unwrap();
        assert_eq!((px.layout, px.width), (Layout::Cmyk, 8), "{name}");
        for (got, want) in u8s(&px)[..4].iter().zip([10, 60, 120, 200]) {
            assert!(got.abs_diff(want) <= 3, "{name}: {:?}", &u8s(&px)[..4]);
        }
    }
}

#[test]
fn jpeg_profile_exif_and_first_of_several_images() {
    let icc = profile(100_000);
    let file = jpeg(8, 8, Jpeg::Rgb, &[50; 192], |e| {
        e.add_icc_profile(&icc).unwrap();
        e.add_exif_metadata(&exif_orientation(6)).unwrap();
    });
    let px = decode(&file).unwrap();
    assert_eq!(
        (px.colour, px.exif, px.orientation),
        (Some(Colour::Icc(icc)), Some(exif_orientation(6)), 6)
    );

    // MPO and Ultra HDR: the base image comes first.
    let mut mpo = jpeg(8, 8, Jpeg::Rgb, &COLORS[0].repeat(64), |_| {});
    mpo.extend(jpeg(16, 16, Jpeg::Rgb, &COLORS[2].repeat(256), |_| {}));
    let px = decode(&mpo).unwrap();
    assert_eq!((px.width, px.height), (8, 8));
    assert!(rgb(&px).iter().all(|c| c[0] > 240 && c[2] < 15));
}

#[test]
fn jpeg_refusals_are_decided_from_the_markers() {
    let err = decode(&fixture("jpeg-arithmetic.jpg")).unwrap_err();
    assert_eq!(
        (err.code, err.message.as_str()),
        (
            "unsupported_variant",
            "JPEG with arithmetic coding isn't supported."
        )
    );

    for (components, variant) in [(2u8, "2 colour components"), (5, "5 colour components")] {
        let mut file = vec![
            0xff,
            0xd8,
            0xff,
            0xc0,
            0,
            8 + 3 * components,
            8,
            0,
            8,
            0,
            8,
            components,
        ];
        (1..=components).for_each(|id| file.extend([id, 0x11, 0]));
        file.extend([0xff, 0xd9]);
        assert_eq!(
            decode(&file).unwrap_err().message,
            format!("JPEG with {variant} isn't supported.")
        );
    }
}

#[test]
fn headers_over_the_limits_are_refused_before_decoding() {
    let mut big = png(1, 1, Png::Rgb, BitDepth::Eight, &[1, 2, 3], |_| {});
    big[16..24].copy_from_slice(&[0, 1, 0x86, 0xa0, 0, 1, 0x86, 0xa0]);
    LIVE.with(|live| PEAK.with(|peak| peak.set(live.get())));
    let start = LIVE.with(Cell::get);
    let err = decode(&big).unwrap_err();
    let peak = PEAK.with(Cell::get) - start;
    assert_eq!(
        err.message,
        "The image is 100000×100000 pixels; the limit is 200 megapixels and 65,535 pixels per side."
    );
    assert_eq!(err.hint, Some("Crop it first."));
    assert!(peak < 64 * 1024, "allocated {peak} bytes");

    let mut gif = b"GIF89a".to_vec();
    gif.extend([0x50, 0xc3, 0x50, 0xc3, 0, 0, 0, 0x3b]);
    let bmp = bmp(40, (70_000, 2), 24, 0, [0; 4], &[], &[]);
    let mut icon = png(1, 1, Png::Rgb, BitDepth::Eight, &[1, 2, 3], |_| {});
    icon[16..24].copy_from_slice(&[0, 1, 0x11, 0x70, 0, 0, 0, 1]);
    let mut vp8x = vec![0, 0, 0, 0];
    vp8x.extend(u24(65535));
    vp8x.extend(u24(0));
    let webp = riff_webp(&[chunk(b"VP8X", &vp8x)]);
    let mut jpg = jpeg(8, 8, Jpeg::Luma, &[0; 64], |_| {});
    let at = jpg.windows(2).position(|w| w == [0xff, 0xc0]).unwrap();
    jpg[at + 5..at + 9].copy_from_slice(&[0xff, 0xff, 0xff, 0xff]);
    for file in [gif, bmp, ico(&[(0, 0, 32, icon)]), webp, jpg] {
        assert_eq!(
            decode(&file).unwrap_err().code,
            "image_too_large",
            "{:?}",
            &file[..4]
        );
    }
}

#[test]
fn empty_zero_sized_and_damaged_files() {
    assert_eq!(decode(b"").unwrap_err().code, "empty_file");

    let mut zero = jpeg(8, 8, Jpeg::Luma, &[0; 64], |_| {});
    let at = zero.windows(2).position(|w| w == [0xff, 0xc0]).unwrap();
    zero[at + 5..at + 7].copy_from_slice(&[0, 0]);
    let err = decode(&zero).unwrap_err();
    assert_eq!(
        err.message,
        "Can't decode this JPEG file: its header gives a size of 0 pixels."
    );
    assert_eq!(err.hint, Some("The file may be damaged or cut short."));

    let whole = [
        png(
            16,
            16,
            Png::Rgb,
            BitDepth::Eight,
            &gradient(16, 16, 3),
            |_| {},
        ),
        jpeg(16, 16, Jpeg::Rgb, &gradient(16, 16, 3), |_| {}),
        gif((3, 2), None, &[(0, 0, 3, 2, vec![0, 1, 2, 3, 4, 5])]),
        webp_lossless(4, 3, &[9; 48]),
    ];
    for file in whole {
        let err = decode(&file[..file.len() * 2 / 3]).unwrap_err();
        assert_eq!(err.code, "corrupt_image", "{:?}", &file[..4]);
        assert!(
            err.message.starts_with("Can't decode this "),
            "{}",
            err.message
        );
    }
}

#[test]
fn generated_files_are_detected_by_content() {
    let cases = [
        (
            png(1, 1, Png::Grayscale, BitDepth::Eight, &[0], |_| {}),
            Kind::Png,
        ),
        (jpeg(8, 8, Jpeg::Luma, &[0; 64], |_| {}), Kind::Jpeg),
        (gif((1, 1), None, &[(0, 0, 1, 1, vec![0])]), Kind::Gif),
        (bmp(40, (1, 1), 24, 0, [0; 4], &[], &[0; 4]), Kind::Bmp),
        (
            ico(&[(
                1,
                1,
                32,
                png(1, 1, Png::Grayscale, BitDepth::Eight, &[0], |_| {}),
            )]),
            Kind::Ico,
        ),
        (webp_lossless(1, 1, &[0; 4]), Kind::Webp),
    ];
    for (file, kind) in cases {
        assert_eq!(detect(&file, Path::new("no-extension")), Ok(kind));
    }
}

#[test]
fn tiff_every_layout_compression_and_predictor() {
    use tiff::encoder::{Compression as C, DeflateLevel, colortype as ct};
    use tiff::tags::Predictor as P;
    let (w, h) = (9, 10);
    let grey = gradient(w, h, 1);
    let grey16: Vec<u16> = grey.iter().map(|&v| u16::from(v) * 251 + 3).collect();
    let rgb = gradient(w, h, 3);
    let rgb16: Vec<u16> = rgb.iter().map(|&v| u16::from(v) * 251).collect();
    let rgba = gradient(w, h, 4);
    let schemes = [
        (C::Uncompressed, P::None),
        (C::Lzw, P::None),
        (C::Lzw, P::Horizontal),
        (C::Deflate(DeflateLevel::Balanced), P::Horizontal),
        (C::Packbits, P::None),
    ];
    for (i, (c, p)) in schemes.into_iter().enumerate() {
        let px = decode(&tiff::<ct::Gray8>(w, h, &grey, c, p, &[], None, 1)).unwrap();
        assert_eq!(
            (px.layout, u8s(&px)),
            (Layout::Grey, &grey[..]),
            "scheme {i}"
        );
        let px = decode(&tiff::<ct::Gray16>(w, h, &grey16, c, p, &[], None, 1)).unwrap();
        assert_eq!(
            (px.bit_depth, &px.samples),
            (16, &Samples::U16(grey16.clone()))
        );
        let px = decode(&tiff::<ct::RGB8>(w, h, &rgb, c, p, &[], None, 1)).unwrap();
        assert_eq!((px.layout, u8s(&px)), (Layout::Rgb, &rgb[..]));
        let px = decode(&tiff::<ct::RGB16>(w, h, &rgb16, c, p, &[], None, 1)).unwrap();
        assert_eq!(px.samples, Samples::U16(rgb16.clone()));
        let alpha = [(338, 2)];
        let px = decode(&tiff::<ct::RGBA8>(w, h, &rgba, c, p, &alpha, None, 1)).unwrap();
        assert_eq!((px.layout, u8s(&px)), (Layout::Rgba, &rgba[..]));
        // ExtraSamples absent or 0: the fourth sample is dropped.
        let opaque: Vec<u8> = rgba.chunks(4).flat_map(|p| &p[..3]).copied().collect();
        for tags in [&[][..], &[(338, 0)]] {
            let px = decode(&tiff::<ct::RGBA8>(w, h, &rgba, c, p, tags, None, 1)).unwrap();
            assert_eq!((px.layout, u8s(&px)), (Layout::Rgb, &opaque[..]));
        }
        let px = decode(&tiff::<ct::CMYK8>(w, h, &rgba, c, p, &[], None, 1)).unwrap();
        assert_eq!((px.layout, u8s(&px)), (Layout::Cmyk, &rgba[..]));
    }
    // Old-style Deflate (32946) is the same stream under another number.
    let mut old = tiff::<ct::RGB8>(
        w,
        h,
        &rgb,
        C::Deflate(DeflateLevel::Fast),
        P::None,
        &[],
        None,
        1,
    );
    let at = old.windows(4).position(|x| x == [3, 1, 3, 0]).unwrap();
    old[at + 8..at + 10].copy_from_slice(&32946u16.to_le_bytes());
    assert_eq!(u8s(&decode(&old).unwrap()), &rgb[..]);
}

#[test]
fn tiff_orientation_profile_and_pages() {
    use tiff::encoder::{Compression as C, colortype as ct};
    use tiff::tags::Predictor as P;
    let icc = profile(300);
    let grey = [7u8; 4];
    let file = tiff::<ct::Gray8>(
        2,
        2,
        &grey,
        C::Uncompressed,
        P::None,
        &[(274, 6)],
        Some(&icc),
        1,
    );
    let px = decode(&file).unwrap();
    assert_eq!(
        (px.orientation, px.colour, px.frames),
        (6, Some(Colour::Icc(icc)), 1)
    );
    assert!(px.warnings.is_empty());
    let pages = tiff::<ct::Gray8>(2, 2, &grey, C::Uncompressed, P::None, &[], None, 3);
    let px = decode(&pages).unwrap();
    assert_eq!(px.frames, 3);
    assert_eq!(
        px.warnings[0].message,
        "This TIFF has 3 pages; only the first was converted."
    );
    // Reduced-resolution pages (NewSubfileType bit 0) don't count.
    let thumbs = tiff::<ct::Gray8>(2, 2, &grey, C::Uncompressed, P::None, &[(254, 1)], None, 2);
    let mut thumbs = thumbs;
    let first = thumbs.windows(4).position(|x| x == [254, 0, 3, 0]).unwrap();
    thumbs[first + 8] = 0;
    let px = decode(&thumbs).unwrap();
    assert_eq!((px.frames, px.warnings.len()), (1, 0));
}

#[test]
fn jpeg_xl_lossy_lossless_grey_alpha_animated_and_cmyk() {
    let source = decode(&fixture("jxl-src.png")).unwrap();
    let lossless = decode(&fixture("jxl-lossless.jxl")).unwrap();
    assert_eq!(
        (lossless.layout, u8s(&lossless)),
        (Layout::Rgb, u8s(&source))
    );
    let lossy = decode(&fixture("jxl-lossy.jxl")).unwrap();
    let reference = decode(&fixture("jxl-lossy-ref.png")).unwrap();
    assert!(mean_diff(u8s(&lossy), u8s(&reference)) <= 2.0);

    let grey = decode(&fixture("jxl-grey16.jxl")).unwrap();
    let grey_source = decode(&fixture("jxl-grey16.png")).unwrap();
    assert_eq!((grey.layout, grey.bit_depth), (Layout::Grey, 16));
    assert_eq!(grey.samples, grey_source.samples);
    let alpha = decode(&fixture("jxl-alpha.jxl")).unwrap();
    let alpha_source = decode(&fixture("jxl-alpha.png")).unwrap();
    assert_eq!(
        (alpha.layout, u8s(&alpha)),
        (Layout::Rgba, u8s(&alpha_source))
    );

    let animated = decode(&fixture("jxl-anim.jxl")).unwrap();
    let first = decode(&fixture("jxl-anim-ref.png")).unwrap();
    assert_eq!(
        (animated.frames, animated.warnings[0].code),
        (2, "first_frame_only")
    );
    assert!(mean_diff(&rgb(&animated).concat(), u8s(&first)) <= 2.0);

    // CMYK with alpha comes out as sRGB with alpha, close to libjxl's own rendering.
    let cmyk = decode(&fixture("jxl-cmyk.jxl")).unwrap();
    let cmyk_reference = decode(&fixture("jxl-cmyk-ref.png")).unwrap();
    assert_eq!(
        (cmyk.layout, cmyk.width, cmyk.height),
        (Layout::Rgba, 512, 512)
    );
    let diff = mean_diff(u8s(&cmyk), u8s(&cmyk_reference));
    assert!(diff <= 2.0, "{diff}");

    let cut = &fixture("jxl-lossy.jxl")[..600];
    assert_eq!(decode(cut).unwrap_err().code, "corrupt_image");
}

fn svg(text: &str, options: &Options) -> Result<(Pixels, (u32, u32)), Error> {
    svg::render(text.as_bytes(), options)
}

/// The RGBA pixel at (x, y).
fn at(px: &Pixels, x: u32, y: u32) -> [u8; 4] {
    let i = 4 * (y * px.width + x) as usize;
    u8s(px)[i..i + 4].try_into().unwrap()
}

#[test]
fn svg_renders_shapes_at_its_own_or_the_asked_size() {
    let shapes = "<svg xmlns='http://www.w3.org/2000/svg' width='40' height='20'>
        <rect width='20' height='20' fill='red'/><rect x='20' width='20' height='20' fill='#00f'/>
        </svg>";
    let (px, own) = svg(shapes, &Options::default()).unwrap();
    assert_eq!(
        (px.width, px.height, own, px.layout),
        (40, 20, (40, 20), Layout::Rgba)
    );
    assert_eq!(
        (at(&px, 5, 5), at(&px, 30, 15)),
        ([255, 0, 0, 255], [0, 0, 255, 255])
    );
    let wide = Options {
        width: Some(400),
        ..Options::default()
    };
    let (px, _) = svg(shapes, &wide).unwrap();
    assert_eq!(
        (px.width, px.height, at(&px, 390, 190)),
        (400, 200, [0, 0, 255, 255])
    );

    let view_box = "<svg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 30 12'/>";
    assert_eq!(svg(view_box, &Options::default()).unwrap().1, (30, 12));
    let percent = "<svg xmlns='http://www.w3.org/2000/svg' width='100%' height='100%'/>";
    assert_eq!(svg(percent, &Options::default()).unwrap().1, (100, 100));
}

#[test]
fn svg_sizes_beyond_the_limits_are_reduced_or_refused() {
    let huge = "<svg xmlns='http://www.w3.org/2000/svg' width='1e9' height='5e8'/>";
    let (px, own) = svg(huge, &Options::default()).unwrap();
    assert_eq!(
        (px.width, px.height, own),
        (8192, 4096, (1_000_000_000, 500_000_000))
    );
    assert_eq!(
        px.warnings[0].message,
        "The drawing is 1000000000×500000000 px, so it was rendered at 8192×4096. Pass --width to choose a size."
    );
    let thin = "<svg xmlns='http://www.w3.org/2000/svg' width='10' height='1000'/>";
    let tall = Options {
        width: Some(1000),
        ..Options::default()
    };
    let err = svg(thin, &tall).unwrap_err();
    assert_eq!(err.code, "image_too_large");
    assert_eq!(
        err.hint,
        Some("Use --width or --height to render it smaller.")
    );
}

#[test]
fn svg_refuses_nested_entities_and_utf16_quickly() {
    let laughs = format!(
        "<?xml version='1.0'?><!DOCTYPE svg [<!ENTITY a 'lol'>{}]><svg xmlns='http://www.w3.org/2000/svg'><text>&j;</text></svg>",
        (b'b'..=b'j')
            .map(|c| format!(
                "<!ENTITY {} '{}'>",
                c as char,
                format!("&{};", (c - 1) as char).repeat(10)
            ))
            .collect::<String>()
    );
    let start = std::time::Instant::now();
    let err = svg(&laughs, &Options::default()).unwrap_err();
    assert!(start.elapsed().as_millis() < 100);
    assert_eq!(
        err.message,
        "SVG with entities that reference other entities isn't supported."
    );
    let plain = "<!DOCTYPE svg [<!ENTITY w '40'>]><svg xmlns='http://www.w3.org/2000/svg' width='&w;' height='4'/>";
    assert!(svg(plain, &Options::default()).is_ok());
    let utf16: Vec<u8> = "\u{feff}<svg/>"
        .encode_utf16()
        .flat_map(u16::to_le_bytes)
        .collect();
    let err = svg::render(&utf16, &Options::default()).unwrap_err();
    assert_eq!(err.message, "SVG with UTF-16 text isn't supported.");
}

#[test]
fn svg_draws_data_images_and_skips_every_other_link() {
    let red = png(1, 1, Png::Rgb, BitDepth::Eight, &[255, 0, 0], |_| {});
    let b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &red);
    let doc = format!(
        "<svg xmlns='http://www.w3.org/2000/svg' xmlns:xlink='http://www.w3.org/1999/xlink' width='10' height='10'>
        <image width='10' height='10' href='data:image/png;base64,{b64}'/>
        <image width='5' height='5' href='file:///etc/passwd'/>
        <image width='5' height='5' xlink:href='https://example.com/a.png'/></svg>"
    );
    let (px, _) = svg(&doc, &Options::default()).unwrap();
    assert_eq!(at(&px, 8, 8), [255, 0, 0, 255]);
    assert_eq!(
        px.warnings[0].message,
        "An image linked from outside the file (file:///etc/passwd … and 1 more) was left out."
    );
}

#[test]
fn svg_gradients_filters_and_text_render() {
    let doc = "<svg xmlns='http://www.w3.org/2000/svg' width='100' height='60'>
        <defs><linearGradient id='g'><stop offset='0' stop-color='red'/><stop offset='1' stop-color='blue'/></linearGradient>
        <filter id='f'><feGaussianBlur stdDeviation='3'/></filter></defs>
        <rect width='100' height='10' fill='url(#g)'/>
        <rect x='40' y='15' width='20' height='10' fill='black' filter='url(#f)'/>
        <text x='2' y='55' font-family='sans-serif' font-size='24'>Hi</text>
        <text x='50' y='55' font-family='No Such Font' font-size='24'>Hi</text></svg>";
    let (px, _) = svg(doc, &Options::default()).unwrap();
    let (left, right) = (at(&px, 1, 5), at(&px, 98, 5));
    assert!(left[0] > 240 && left[2] < 15 && right[2] > 240 && right[0] < 15);
    let edge = at(&px, 39, 20)[3];
    assert!(edge > 20 && edge < 235, "blur edge alpha {edge}");
    let inked = |x0: u32| {
        (x0..x0 + 40)
            .flat_map(|x| (32..60).map(move |y| (x, y)))
            .filter(|&(x, y)| at(&px, x, y)[3] > 128)
            .count()
    };
    assert!(
        inked(0) > 30 && inked(50) > 30,
        "{} {}",
        inked(0),
        inked(50)
    );
}

#[test]
fn svgz_is_inflated_under_a_cap() {
    use flate2::{Compression, write::GzEncoder};
    use std::io::Write;
    let gzip = |data: &[u8]| {
        let mut e = GzEncoder::new(Vec::new(), Compression::best());
        e.write_all(data).unwrap();
        e.finish().unwrap()
    };
    let doc = b"<svg xmlns='http://www.w3.org/2000/svg' width='3' height='2'/>";
    assert_eq!(detect(&gzip(doc), Path::new("a.svgz")), Ok(Kind::Gzip));
    assert_eq!(svg::ungzip(&gzip(doc)).unwrap(), (doc.to_vec(), Kind::Svg));
    let mut bomb = doc.to_vec();
    bomb.resize(65 << 20, b' ');
    let bomb = gzip(&bomb);
    assert!(bomb.len() < 100_000);
    let err = svg::ungzip(&bomb).unwrap_err();
    assert_eq!(
        err.message,
        "The compressed file expands to more than 64 MiB."
    );
    let png = gzip(&png(1, 1, Png::Grayscale, BitDepth::Eight, &[0], |_| {}));
    assert_eq!(svg::ungzip(&png).unwrap_err().code, "unsupported_format");
    assert_eq!(
        svg::ungzip(b"\x1f\x8b\x08junk").unwrap_err().code,
        "corrupt_image"
    );
}

fn ispe(width: u32, height: u32) -> Vec<u8> {
    full(
        b"ispe",
        0,
        &[width.to_be_bytes(), height.to_be_bytes()].concat(),
    )
}

/// An HEVC primary item with its size, P3 nclx colour, irot 1, imir 0, alpha and EXIF.
fn iphone_like() -> HeifFile {
    let mut exif = vec![0, 0, 0, 0];
    exif.extend(exif_orientation(6));
    let alpha = b"urn:mpeg:mpegB:cicp:systems:auxiliary:alpha\0";
    HeifFile {
        primary: 1,
        items: vec![(1, *b"hvc1"), (2, *b"Exif"), (3, *b"hvc1")],
        refs: vec![(*b"cdsc", 2, vec![1]), (*b"auxl", 3, vec![1])],
        props: vec![
            ispe(4032, 3024),
            bx(
                b"colr",
                &[&b"nclx"[..], &[0, 12, 0, 13, 0, 6, 0x80]].concat(),
            ),
            bx(b"irot", &[1]),
            bx(b"imir", &[0]),
            full(b"auxC", 0, alpha),
        ],
        assoc: vec![(1, vec![0x81, 2, 3, 4]), (3, vec![1, 5])],
        locations: vec![(1, 0, 0, 4), (2, 0, 4, exif.len() as u32), (3, 0, 0, 4)],
        mdat: [&[0; 4][..], &exif].concat(),
        ..HeifFile::default()
    }
}

#[test]
fn bmff_reads_the_primary_item_and_its_properties() {
    let file = iphone_like().build();
    let info = bmff::heif(&file, Kind::Heic).unwrap();
    let expected = bmff::Heif {
        kind: Kind::Heic,
        width: 4032,
        height: 3024,
        colour: Some(Colour::Cicp(Cicp {
            primaries: 12,
            transfer: 13,
            matrix: 6,
            full_range: true,
        })),
        rotation: 1,
        mirror: Some(0),
        alpha: true,
        bits: 8,
        exif: Some(exif_orientation(6)),
        sequence: false,
    };
    assert_eq!(info, expected);
    assert_eq!(info.orientation(), 7);

    // A grid of AV1 tiles is AVIF whatever the brand says; its own data may sit in idat.
    let mut grid = iphone_like();
    grid.primary = 9;
    grid.items
        .extend([(9, *b"grid"), (10, *b"av01"), (11, *b"av01")]);
    grid.refs.push((*b"dimg", 9, vec![10, 11]));
    grid.props[0] = ispe(1024, 512);
    grid.props[1] = bx(b"colr", &[&b"prof"[..], &profile(40)].concat());
    grid.assoc.push((9, vec![1, 2]));
    grid.locations
        .extend([(9, 1, 0, 8), (10, 0, 0, 4), (11, 0, 0, 4)]);
    let info = bmff::heif(&grid.build(), Kind::Heic).unwrap();
    assert_eq!(
        (info.kind, info.width, info.rotation),
        (Kind::Avif, 1024, 0)
    );
    assert_eq!(info.colour, Some(Colour::Icc(profile(40))));
    assert_eq!((info.alpha, info.exif), (false, None));

    // A tone-mapped primary goes by its base image.
    let mut tmap = iphone_like();
    tmap.primary = 9;
    tmap.items.push((9, *b"tmap"));
    tmap.refs.push((*b"dimg", 9, vec![1]));
    tmap.assoc.push((9, vec![1]));
    assert_eq!(
        bmff::heif(&tmap.build(), Kind::Avif).unwrap().kind,
        Kind::Heic
    );
}

#[test]
fn bmff_refuses_the_variants_the_os_decoders_never_see() {
    let message = |file: &HeifFile| bmff::heif(&file.build(), Kind::Heic).unwrap_err().message;
    let no_meta = HeifFile {
        no_meta: true,
        ..iphone_like()
    };
    assert_eq!(
        message(&no_meta),
        "HEIF with no primary image isn't supported."
    );
    for (ty, variant) in [
        (b"jpeg", "JPEG compression"),
        (b"j2k1", "JPEG 2000 compression"),
        (b"unci", "no compression (unci)"),
        (b"vvc1", "VVC compression"),
        (b"evc1", "EVC compression"),
        (b"mski", "'mski' compression"),
        (b"iovl", "overlay images"),
        (b"iden", "identity-derived images"),
    ] {
        let mut file = iphone_like();
        file.items[0].1 = *ty;
        assert_eq!(
            message(&file),
            format!("HEIF with {variant} isn't supported.")
        );
    }
    let mut idat = iphone_like();
    idat.locations[0].1 = 1;
    assert_eq!(
        message(&idat),
        "HEIF with item data in idat or another item isn't supported."
    );
    let mut outside = iphone_like();
    outside.locations[0].2 = 1 << 30;
    assert_eq!(
        bmff::heif(&outside.build(), Kind::Heic).unwrap_err().code,
        "corrupt_image"
    );
    let mut huge = iphone_like();
    huge.props[0] = ispe(70_000, 10);
    assert_eq!(
        bmff::heif(&huge.build(), Kind::Heic).unwrap_err().code,
        "image_too_large"
    );
    let mut sizeless = iphone_like();
    sizeless.assoc[0].1.retain(|&p| p & 0x7f != 1);
    assert_eq!(
        bmff::heif(&sizeless.build(), Kind::Heic).unwrap_err().code,
        "corrupt_image"
    );
}

#[test]
fn bmff_limits_end_in_corrupt_image() {
    // A file cut inside its last box, and one with too many boxes.
    let mut cut = iphone_like().build();
    cut.truncate(cut.len() - 3);
    let err = bmff::heif(&cut, Kind::Heic).unwrap_err();
    assert_eq!(
        err.message,
        "Can't decode this HEIF file: a box runs past its end."
    );
    let many: Vec<u8> = (0..10_001).flat_map(|_| bx(b"free", &[])).collect();
    let err = bmff::heif(&[bx(b"ftyp", b"heic\0\0\0\0"), many].concat(), Kind::Heic).unwrap_err();
    assert_eq!(
        err.message,
        "Can't decode this HEIF file: it has more than 10,000 boxes."
    );
}

#[test]
fn bmff_finds_cr3_previews_and_cmt1() {
    let (sample, prvw) = (
        b"\xff\xd8\xff\xe0track".as_slice(),
        b"\xff\xd8\xff\xdbprvw".as_slice(),
    );
    let file = cr3(sample, prvw, &exif_orientation(8));
    let found = bmff::cr3(&file).unwrap();
    assert_eq!(found.previews, vec![sample, prvw]);
    assert_eq!(found.cmt1, Some(&exif_orientation(8)[..]));
}

/// A `width` × `height` JPEG preview, with an EXIF orientation when `orientation` is given.
fn preview(width: u16, height: u16, orientation: Option<u16>) -> Vec<u8> {
    let pixels = gradient(width.into(), height.into(), 3);
    jpeg(width, height, Jpeg::Rgb, &pixels, |e| {
        if let Some(o) = orientation {
            e.add_app_segment(1, [&b"Exif\0\0"[..], &exif_orientation(o)].concat())
                .unwrap();
        }
    })
}

fn jpeg_entries(blob: usize, len: usize) -> Vec<Entry> {
    vec![
        (0x201, 4, 1, V::Blob(blob)),
        (0x202, 4, 1, V::N(len as u32)),
    ]
}

#[test]
fn raw_takes_the_largest_jpeg_preview_with_the_right_orientation() {
    let (thumb, big) = (preview(160, 120, None), preview(1200, 800, None));
    // The raw data: a lossless JPEG (SOF3) larger than every preview.
    let mut lossless = preview(1600, 1000, None);
    let sof = lossless.windows(2).position(|w| w == [0xff, 0xc0]).unwrap();
    lossless[sof + 1] = 0xc3;
    let blobs = [&thumb[..], &big, &lossless];
    let mut ifd0 = jpeg_entries(0, thumb.len());
    ifd0.push((0x112, 3, 1, V::N(6)));
    let strip = vec![
        (259, 3, 1, V::N(7)),
        (0x111, 4, 1, V::Blob(2)),
        (0x117, 4, 1, V::N(lossless.len() as u32)),
    ];
    let nef = raw_tiff(42, &blobs, &ifd0, &[jpeg_entries(1, big.len()), strip]);
    let px = raw::decode(&nef, Kind::Nef).unwrap();
    assert_eq!((px.width, px.height, px.orientation), (1200, 800, 6));
    let codes: Vec<_> = px.warnings.iter().map(|w| w.message.as_str()).collect();
    assert_eq!(
        codes,
        ["Converted the camera's embedded preview (1200×800), not a full RAW development."]
    );

    // The preview's own EXIF orientation wins over IFD0's.
    let own = preview(1200, 800, Some(3));
    let pef = raw_tiff(
        42,
        &[&own],
        &[jpeg_entries(0, own.len()), vec![(0x112, 3, 1, V::N(6))]]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>(),
        &[],
    );
    assert_eq!(raw::decode(&pef, Kind::Pef).unwrap().orientation, 3);

    // CR2: an old-style JPEG strip in IFD0, small enough to warn.
    let cr2 = raw_tiff(
        42,
        &[&thumb],
        &[
            (259, 3, 1, V::N(6)),
            (0x111, 4, 1, V::Blob(0)),
            (0x117, 4, 1, V::N(thumb.len() as u32)),
        ],
        &[],
    );
    let px = raw::decode(&cr2, Kind::Cr2).unwrap();
    assert_eq!((px.width, px.orientation), (160, 1));
    assert_eq!(
        px.warnings[1].message,
        "The embedded preview is only 160×120 pixels."
    );
}

#[test]
fn raw_rw2_raf_and_cr3_previews() {
    let big = preview(1200, 800, None);
    let rw2 = raw_tiff(
        0x55,
        &[&big],
        &[(0x2E, 7, big.len() as u32, V::Blob(0))],
        &[],
    );
    assert_eq!(&rw2[..4], b"IIU\0");
    assert_eq!(raw::decode(&rw2, Kind::Rw2).unwrap().width, 1200);

    let mut raf = b"FUJIFILMCCD-RAW 0201FF383501".to_vec();
    raf.resize(100, 0);
    raf[84..88].copy_from_slice(&100u32.to_be_bytes());
    raf[88..92].copy_from_slice(&(big.len() as u32).to_be_bytes());
    raf.extend(&big);
    assert_eq!(raw::decode(&raf, Kind::Raf).unwrap().height, 800);

    // Track 1 holds no JPEG here, so the PRVW preview is used, upright by CMT1.
    let cr3 = cr3(
        b"not a jpeg, a raw track",
        &preview(1620, 1080, None),
        &exif_orientation(8),
    );
    let px = raw::decode(&cr3, Kind::Cr3).unwrap();
    assert_eq!((px.width, px.orientation), (1620, 8));
}

#[test]
fn raw_without_a_usable_preview() {
    // A DNG whose only image is an uncompressed thumbnail, and one whose JPEG is under 1 KB.
    let thumb = vec![
        (259, 3, 1, V::N(1)),
        (0x111, 4, 1, V::Blob(0)),
        (0x117, 4, 1, V::N(768)),
    ];
    let dng = raw_tiff(42, &[&[0; 768]], &thumb, &[]);
    let err = raw::decode(&dng, Kind::Dng).unwrap_err();
    assert_eq!(
        (err.code, err.message.as_str()),
        (
            "raw_no_preview",
            "This Adobe DNG RAW file has no embedded preview image."
        )
    );
    let tiny = preview(8, 8, None);
    assert!(tiny.len() < 1024);
    let arw = raw_tiff(42, &[&tiny], &jpeg_entries(0, tiny.len()), &[]);
    assert_eq!(
        raw::decode(&arw, Kind::Arw).unwrap_err().code,
        "raw_no_preview"
    );
    assert_eq!(
        raw::decode(b"II*\0\xff\xff\0\0", Kind::Nef)
            .unwrap_err()
            .code,
        "corrupt_image"
    );
}

/// The CC0 samples tests/fixtures/fetch-raw.sh downloads (too large to commit), one per kind.
#[test]
#[ignore = "needs tests/fixtures/fetch-raw.sh"]
fn real_raw_samples() {
    let dir = std::env::var("IMAGE_CONVERT_RAW_SAMPLES")
        .unwrap_or_else(|_| format!("{}/target/raw-samples", env!("CARGO_MANIFEST_DIR")));
    let mut kinds = Vec::new();
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        let data = std::fs::read(&path).unwrap();
        let kind = detect(&data, &path).unwrap();
        let px = raw::decode(&data, kind).unwrap_or_else(|e| panic!("{path:?}: {}", e.message));
        println!(
            "{path:?}: {}×{} orientation {}",
            px.width, px.height, px.orientation
        );
        assert!(
            px.warnings.iter().any(|w| w.code == "raw_preview"),
            "{path:?}"
        );
        let fit = Options {
            fit: image_convert::Fit::Vision,
            ..Options::default()
        };
        let out = image_convert::render(px, kind, &fit).unwrap();
        assert!(out.width.max(out.height) <= 1568);
        kinds.push(kind);
    }
    kinds.sort_by_key(|k| k.name());
    use Kind::*;
    assert_eq!(kinds, [Arw, Cr2, Cr3, Dng, Nef, Pef, Raf, Rw2]);
}

/// Samples as 8 bits, as the pipeline reduces them.
fn eight(px: &Pixels) -> Vec<u8> {
    match &px.samples {
        Samples::U8(s) => s.clone(),
        Samples::U16(s) => s
            .iter()
            .map(|&v| ((u32::from(v) + 128) / 257) as u8)
            .collect(),
    }
}

#[test]
fn tiff_from_libtiff_and_imagemagick_match_their_references() {
    let cases = [
        ("tiff-be-lzw.tif", "tiff-src.png", Layout::Rgb, 0.0),
        (
            "tiff-rgb-jpeg.tif",
            "tiff-rgb-jpeg-ref.png",
            Layout::Rgb,
            2.0,
        ),
        (
            "tiff-ycbcr-jpeg.tif",
            "tiff-ycbcr-jpeg-ref.png",
            Layout::Rgb,
            2.0,
        ),
        ("tiff-g4.tif", "tiff-g4-ref.png", Layout::Grey, 0.0),
        (
            "tiff-bilevel.tif",
            "tiff-bilevel-ref.png",
            Layout::Grey,
            0.0,
        ),
        ("tiff-white.tif", "tiff-white-ref.png", Layout::Grey, 0.0),
        (
            "tiff-greya8.tif",
            "tiff-greya8-ref.png",
            Layout::GreyAlpha,
            0.0,
        ),
        (
            "tiff-greya16.tif",
            "tiff-greya8-ref.png",
            Layout::GreyAlpha,
            0.0,
        ),
    ];
    for (file, reference, layout, max) in cases {
        let px = decode(&fixture(file)).unwrap();
        let reference = decode(&fixture(reference)).unwrap();
        assert_eq!((px.layout, px.width, px.height), (layout, 40, 30), "{file}");
        assert_eq!(reference.layout, layout, "{file}'s reference");
        let diff = mean_diff(&eight(&px), &eight(&reference));
        assert!(diff <= max, "{file}: mean difference {diff}");
    }
}

/// The decoded image made upright, as 8-bit sRGB.
fn upright(mut px: Pixels) -> image_convert::Image {
    let orientation = px.orientation;
    image_convert::resize::orient(image_convert::color::to_srgb(&mut px).0, orientation)
}

#[test]
fn heif_irot_is_applied_once_and_exif_orientation_ignored() {
    let reference = raster::decode(&fixture("heif-irot-ref.png"), Kind::Png).unwrap();
    let Samples::U8(expected) = reference.samples else {
        unreachable!()
    };
    for name in ["heif-irot.heic", "heif-irot.avif"] {
        let Some(px) = os_heif(name) else { continue };
        let exif = px.exif.as_deref().unwrap();
        let orientation = image::metadata::Orientation::from_exif_chunk(exif).unwrap();
        assert_eq!((orientation.to_exif(), px.bit_depth), (6, 8), "{name}");
        let image = upright(px);
        assert_eq!((image.width, image.height), (64, 128), "{name}");
        let rgb: Vec<u8> = match image.layout {
            Layout::Rgba => image
                .data
                .chunks(4)
                .flat_map(|p| &p[..3])
                .copied()
                .collect(),
            _ => image.data,
        };
        let diff = mean_diff(&rgb, &expected);
        assert!(diff <= 1.0, "{name}: mean difference {diff}");
    }
}

#[test]
fn heif_results_report_alpha_from_the_file() {
    for (name, alpha) in [("heif-alpha.avif", true), ("heif-p3.avif", false)] {
        if os_heif(name).is_some() {
            let (file, _) = engine::convert(name.into(), &fixture(name), &Options::default());
            assert_eq!(file.source.unwrap().alpha, alpha, "{name}");
        }
    }
}

#[test]
fn heif_alpha_and_ten_bit_samples_come_through() {
    if let Some(px) = os_heif("heif-alpha.avif") {
        assert!(px.warnings.is_empty(), "{:?}", px.warnings);
        let expected = raster::decode(&fixture("heif-alpha.png"), Kind::Png).unwrap();
        let Samples::U8(expected) = expected.samples else {
            unreachable!()
        };
        let image = upright(px);
        assert_eq!(
            (image.width, image.height, image.layout),
            (40, 30, Layout::Rgba)
        );
        // Compared as shown: ImageIO draws premultiplied, so faint pixels lose colour precision.
        let shown = |rgba: &[u8]| -> Vec<u8> {
            let pixels = rgba.as_chunks::<4>().0.iter();
            pixels
                .flat_map(|&[r, g, b, a]| {
                    let premultiply = |c: u8| (u16::from(c) * u16::from(a) / 255) as u8;
                    [premultiply(r), premultiply(g), premultiply(b), a]
                })
                .collect()
        };
        let diff = mean_diff(&shown(&image.data), &shown(&expected));
        assert!(diff <= 1.0, "mean difference {diff}");
    }
    if let Some(px) = os_heif("heif-10bit.avif") {
        assert_eq!(px.bit_depth, 10);
        let expected = raster::decode(&fixture("heif-10bit.png"), Kind::Png).unwrap();
        let Samples::U16(expected) = expected.samples else {
            unreachable!()
        };
        let expected: Vec<u8> = expected
            .iter()
            .map(|&v| ((u32::from(v) + 128) / 257) as u8)
            .collect();
        let image = upright(px);
        let rgb: Vec<u8> = image
            .data
            .chunks(4)
            .flat_map(|p| &p[..3])
            .copied()
            .collect();
        let diff = mean_diff(&rgb, &expected);
        assert!(diff <= 1.0, "mean difference {diff}");
    }
}

#[test]
fn metafiles_render_on_windows_only() {
    use flate2::{Compression, write::GzEncoder};
    use image_convert::os::render_metafile;
    use std::io::Write;
    let (emf, wmf) = (plain_emf(), placeable_wmf());
    let mut gz = GzEncoder::new(Vec::new(), Compression::default());
    gz.write_all(&emf).unwrap();
    let (emz, kind) = svg::ungzip(&gz.finish().unwrap()).unwrap();
    assert_eq!(kind, Kind::Emf);
    // (name, data, exact geometry, an orange circle drawn anti-aliased on the rectangle).
    let cases = [
        ("emf", emf, true, true),
        ("emz", emz, true, true),
        ("wmf", wmf.clone(), true, false),
        ("emf-plus.emf", fixture("emf-plus.emf"), false, true),
        ("emf-dual.emf", fixture("emf-dual.emf"), false, true),
    ];
    for (name, data, exact, circle) in cases {
        let kind = detect(&data, Path::new(name)).unwrap();
        let rendered = render_metafile(&data, kind, &Options::default());
        if cfg!(not(windows)) {
            assert_eq!(rendered.unwrap_err().code, "unsupported_on_platform");
            continue;
        }
        let (px, own) = rendered.unwrap();
        let size = (px.width, px.height, own, px.layout);
        assert_eq!(size, (96, 48, (96, 48), Layout::Rgba), "{name}");
        let Samples::U8(rgba) = &px.samples else {
            unreachable!()
        };
        let at = |x: usize, y: usize| <[u8; 4]>::try_from(&rgba[(y * 96 + x) * 4..][..4]).unwrap();
        assert_eq!(at(2, 2)[3], 0, "{name}: the background stays transparent");
        assert_eq!(at(20, 24), [70, 130, 180, 255], "{name}");
        if exact {
            assert_eq!([at(9, 9)[3], at(86, 38)[3]], [255; 2], "{name}");
            assert_eq!([at(6, 6)[3], at(89, 41)[3]], [0; 2], "{name}");
        }
        if circle {
            let (blue, orange) = ([70, 130, 180, 255], [255, 165, 0, 255]);
            let pixels = rgba.as_chunks::<4>().0;
            assert!(pixels.contains(&orange), "{name}: no orange circle");
            let blend = pixels
                .iter()
                .any(|p| p[3] == 255 && ![blue, orange].contains(p));
            assert!(blend, "{name}: the circle's edge isn't anti-aliased");
        }
    }

    // A WMF without its placeable header, and drawing to a size.
    let plain = detect(&wmf[22..], Path::new("a.wmf")).unwrap();
    let refused = render_metafile(&wmf[22..], plain, &Options::default()).unwrap_err();
    let wide = Options {
        width: Some(192),
        ..Options::default()
    };
    let sized = render_metafile(&wmf, Kind::Wmf, &wide);
    if cfg!(windows) {
        let message = "WMF with no placeable header isn't supported.";
        assert_eq!(
            (refused.code, refused.message.as_str()),
            ("unsupported_variant", message)
        );
        let (px, own) = sized.unwrap();
        assert_eq!((px.width, px.height, own), (192, 96, (96, 48)));
    } else {
        assert_eq!(refused.code, "unsupported_on_platform");
        assert_eq!(sized.unwrap_err().code, "unsupported_on_platform");
    }
}

#[test]
fn formats_lists_the_matrix_in_order_as_this_machine_decodes_it() {
    use image_convert::os::{HEIF_DECODER, formats, heif_available};
    let rows = formats();
    let names: Vec<_> = rows.iter().map(|r| r.format.name()).collect();
    let matrix =
        "png jpeg gif bmp ico webp tiff jxl heic avif svg emf wmf cr2 cr3 nef arw dng raf rw2 pef";
    assert_eq!(names.join(" "), matrix);
    let windows_only = (!cfg!(windows)).then(Error::unsupported_on_platform);
    for row in &rows {
        // What this runner has, in the CI log (with --nocapture).
        let why = row.missing.as_ref().map_or("", |e| e.message.as_str());
        eprintln!("{:<5} {:<23} {why}", row.format.name(), row.status());
        let (decoder, missing) = match row.format {
            Kind::Heic | Kind::Avif => (HEIF_DECODER, heif_available(row.format).err()),
            Kind::Emf | Kind::Wmf => ("gdiplus", windows_only.clone()),
            Kind::Cr2
            | Kind::Cr3
            | Kind::Nef
            | Kind::Arw
            | Kind::Dng
            | Kind::Raf
            | Kind::Rw2
            | Kind::Pef => ("preview", None),
            _ => ("builtin", None),
        };
        assert_eq!(
            (row.decoder, &row.missing),
            (decoder, &missing),
            "{}",
            row.format.name()
        );
        let status = match &missing {
            None => "available",
            Some(e) if e.code == "codec_missing" => "missing",
            Some(_) => "unsupported_on_platform",
        };
        assert_eq!(row.status(), status);
    }
}

/// CI's heif-linux job names the libheif packages it installed in `IMAGE_CONVERT_TEST_HEIF`.
#[test]
fn heif_state_matches_the_installed_packages() {
    let Ok(state) = std::env::var("IMAGE_CONVERT_TEST_HEIF") else {
        return;
    };
    for (kind, name, codec) in [(Kind::Heic, "HEIC", "HEVC"), (Kind::Avif, "AVIF", "AV1")] {
        let expected = match state.as_str() {
            "none" => Some(format!(
                "{name} needs the system's libheif library, which isn't installed."
            )),
            "old" => Some("The system's libheif 1.12 is too old; 1.13 or newer is needed.".into()),
            // Ubuntu 24.04's libheif1 decodes AV1 through libaom by itself; HEVC is a plugin.
            "no-plugin" if kind == Kind::Avif => None,
            "no-plugin" => Some(format!("The system's libheif has no {codec} decoder.")),
            "all" => None,
            other => panic!("unknown IMAGE_CONVERT_TEST_HEIF: {other}"),
        };
        let got = image_convert::os::heif_available(kind).err();
        assert_eq!(got.map(|e| e.message), expected, "{name}");
    }
}

#[test]
fn a_jpeg_cut_inside_its_scans_is_corrupt() {
    for progressive in [false, true] {
        let whole = jpeg(64, 64, Jpeg::Rgb, &gradient(64, 64, 3), |e| {
            e.set_progressive(progressive)
        });
        let err = decode(&whole[..whole.len() * 3 / 4]).unwrap_err();
        assert_eq!(err.code, "corrupt_image", "progressive {progressive}");
    }
}

#[test]
fn gif_screen_outside_the_first_frame_is_transparent_as_browsers_draw_it() {
    // No transparent index: the uncovered screen still isn't filled with the background colour.
    let px = decode(&gif((4, 3), None, &[(1, 1, 2, 2, vec![0, 1, 2, 3])])).unwrap();
    let a = alpha(&px);
    let opaque: Vec<usize> = (0..12).filter(|&i| a[i] == 255).collect();
    assert_eq!(opaque, [5, 6, 9, 10]);
}

#[test]
fn jpeg_xl_orientation_is_reported_with_the_stored_size() {
    for orientation in 1..=8u8 {
        let mut data = fixture("jxl-anim.jxl");
        // This file's 3 orientation bits (value - 1) are bits 3 to 5 of byte 3.
        data[3] = data[3] & !0x38 | (orientation - 1) << 3;
        let (file, _) = engine::convert("x.jxl".into(), &data, &Options::default());
        let source = file.source.unwrap();
        assert_eq!(
            (source.width, source.height, source.orientation),
            (64, 48, orientation)
        );
        let upright = if orientation > 4 { (48, 64) } else { (64, 48) };
        assert_eq!(
            (file.width, file.height),
            (Some(upright.0), Some(upright.1))
        );
    }
}

#[test]
#[ignore = "known bug: IC15"]
fn jpeg_with_one_scan_per_component_decodes() {
    // jpeg-encoder writes each component's scan separately once its Huffman tables are optimised.
    let source = gradient(53, 37, 3);
    for factor in [
        SamplingFactor::F_2_1,
        SamplingFactor::F_1_2,
        SamplingFactor::F_2_2,
        SamplingFactor::F_4_1,
    ] {
        let file = jpeg(53, 37, Jpeg::Rgb, &source, |e| {
            e.set_sampling_factor(factor);
            e.set_optimized_huffman_tables(true);
        });
        assert_eq!(file.windows(2).filter(|w| w == &[0xff, 0xda]).count(), 3);
        let diff = mean_diff(u8s(&decode(&file).unwrap()), &source);
        assert!(diff < 8.0, "{factor:?}: {diff}");
    }
}

#[test]
#[ignore = "known bug: IC02"]
fn a_jpeg_cut_inside_its_scans_is_corrupt_even_with_an_end_marker() {
    let [baseline, progressive] = [false, true].map(|progressive| {
        jpeg(64, 64, Jpeg::Rgb, &gradient(64, 64, 3), |e| {
            e.set_progressive(progressive)
        })
    });
    for whole in [&baseline, &progressive] {
        let mut cut = whole[..whole.len() * 3 / 4].to_vec();
        cut.extend([0xff, 0xd9]);
        assert_eq!(decode(&cut).unwrap_err().code, "corrupt_image");
    }
    // Cut after the first of a progressive JPEG's scans; the end-marker bytes sit in an APP15.
    let whole = progressive;
    let mut scans = (0..whole.len() - 1).filter(|&i| whole[i..i + 2] == [0xff, 0xda]);
    let mut cut = whole[..scans.nth(1).unwrap()].to_vec();
    cut.extend([0xff, 0xef, 0, 4, 0xff, 0xd9]);
    assert_eq!(decode(&cut).unwrap_err().code, "corrupt_image");
}

#[test]
#[ignore = "known bug: IC16"]
fn svg_data_images_are_held_to_the_limits() {
    let image = |kind: &str, data: &[u8]| {
        format!(
            "<svg xmlns='http://www.w3.org/2000/svg' width='8' height='8'><defs>
            <image id='i' width='8' height='8' href='data:image/{kind};base64,{}'/></defs>",
            base64::Engine::encode(&base64::engine::general_purpose::STANDARD, data)
        )
    };
    // The bytes the render allocates beyond what is live when it starts.
    let peak = |doc: &str| {
        LIVE.with(|live| PEAK.with(|peak| peak.set(live.get())));
        let start = LIVE.with(Cell::get);
        let result = svg(doc, &Options::default());
        (result.err().map(|e| e.code), PEAK.with(Cell::get) - start)
    };
    // 65,536 px wide, and 14,200² = 201.6 MP: tiny as 1-bit PNGs.
    for (width, height) in [(65_536u32, 1u32), (14_200, 14_200)] {
        let rows = vec![0; width.div_ceil(8) as usize * height as usize];
        let data = png(width, height, Png::Grayscale, BitDepth::One, &rows, |_| {});
        let doc = image("png", &data) + "<use href='#i'/></svg>";
        let (code, bytes) = peak(&doc);
        assert_eq!(code, Some("image_too_large"), "{width}×{height}");
        assert!(
            bytes < 64 << 20,
            "{width}×{height}: allocated {bytes} bytes"
        );
    }
    // SVG images nested five deep, each used ten times: 10⁵ copies from a 6 KB file.
    let mut doc = "<svg xmlns='http://www.w3.org/2000/svg' width='8' height='8'/>".to_string();
    for _ in 0..5 {
        doc = image("svg+xml", doc.as_bytes()) + &"<use href='#i'/>".repeat(10) + "</svg>";
    }
    let (_, bytes) = peak(&doc);
    assert!(bytes < 64 << 20, "nested: allocated {bytes} bytes");
}
