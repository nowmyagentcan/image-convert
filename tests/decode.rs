//! Every accepted and refused variant of the core formats, from files written in the tests or by
//! tests/fixtures/make.sh.

mod common;

use std::alloc::{GlobalAlloc, Layout as Alloc, System};
use std::cell::Cell;
use std::path::Path;

use common::*;
use image_convert::detect::{Kind, detect};
use image_convert::error::Error;
use image_convert::{Layout, Pixels, Samples, jpeg, raster, tiff as tif};
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
        (px.icc, px.exif, px.orientation),
        (Some(icc), Some(exif_orientation(6)), 6)
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
        (px.icc, px.exif, px.orientation),
        (Some(icc), Some(exif_orientation(6)), 6)
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
        (px.icc, px.exif, px.orientation),
        (Some(icc), Some(exif_orientation(6)), 6)
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
    assert_eq!((px.orientation, px.icc, px.frames), (6, Some(icc), 1));
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
