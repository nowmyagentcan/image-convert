//! Output sizes from presets and limits, Lanczos3 resizing on premultiplied alpha, and turning
//! images upright.

use crate::error::{Error, Warning};
use crate::{Image, Options};
use fast_image_resize::images::{Image as Buffer, ImageRef};
use fast_image_resize::{PixelType, Resizer};

/// Sizes a stored image for its upright shape, resizes it, then applies the EXIF `orientation`,
/// so a large photo is never turned at full size.
pub fn size_and_orient(image: Image, orientation: u8, options: &Options) -> Image {
    let swap = orientation >= 5;
    let (w, h) = (image.width, image.height);
    let (tw, th) = if swap {
        let (tw, th) = target(h, w, options, true);
        (th, tw)
    } else {
        target(w, h, options, true)
    };
    let image = if (tw, th) == (w, h) {
        image
    } else {
        resize(&image, tw, th)
    };
    orient(image, orientation)
}

/// The size for an image of `width` × `height` px under `options`: shrunk to fit every limit,
/// never enlarged when `raster`. Vector input with no limit keeps its own size.
pub fn target(width: u32, height: u32, options: &Options, raster: bool) -> (u32, u32) {
    let (w, h) = (f64::from(width), f64::from(height));
    let (max_side, max_megapixels, _) = options.limits();
    let scale = [
        raster.then_some(1.0),
        max_side.map(|side| f64::from(side) / w.max(h)),
        max_megapixels.map(|mp| (mp * 1e6 / (w * h)).sqrt()),
        options.width.map(|bw| f64::from(bw) / w),
        options.height.map(|bh| f64::from(bh) / h),
    ]
    .into_iter()
    .flatten()
    .reduce(f64::min);
    match scale {
        Some(scale) => (scaled(width, scale), scaled(height, scale)),
        None => (width, height),
    }
}

/// `side × scale` rounded down, at least 1; the nudge keeps exact products like 4032 × 1568/4032
/// from rounding down to the integer below.
/// The size to draw a vector image of `own` px at: its own size, cut to 8192 px a side unless
/// `--width`/`--height` asks, fitted to `options`; with `render_reduced` when the cut decided
/// it. Over 16384 px a side or 200 megapixels is `image_too_large`.
pub fn vector_target(
    own: [u32; 2],
    options: &Options,
) -> Result<(u32, u32, Option<Warning>), Error> {
    let explicit = options.width.is_some() || options.height.is_some();
    let base = match own[0].max(own[1]) {
        longer if !explicit && longer > 8192 => {
            let scale = 8192.0 / f64::from(longer);
            own.map(|s| scaled(s, scale))
        }
        _ => own,
    };
    let (width, height) = target(base[0], base[1], options, false);
    if width.max(height) > 16384 || u64::from(width) * u64::from(height) > 200_000_000 {
        return Err(Error::image_too_large(width.into(), height.into(), true));
    }
    let reduced = (base != own && [width, height] == base)
        .then(|| Warning::render_reduced((own[0].into(), own[1].into()), (width, height)));
    Ok((width, height, reduced))
}

pub fn scaled(side: u32, scale: f64) -> u32 {
    ((f64::from(side) * scale + 1e-6).floor() as u32).max(1)
}

/// The image at `width` × `height` px through Lanczos3, alpha premultiplied while filtering.
pub fn resize(image: &Image, width: u32, height: u32) -> Image {
    let pixel = [
        PixelType::U8,
        PixelType::U8x2,
        PixelType::U8x3,
        PixelType::U8x4,
    ][image.layout.channels() - 1];
    let source = ImageRef::new(image.width, image.height, &image.data, pixel)
        .expect("the buffer holds width × height pixels");
    let mut out = Buffer::new(width, height, pixel);
    Resizer::new()
        .resize(&source, &mut out, None)
        .expect("source and destination share a pixel type");
    Image {
        width,
        height,
        layout: image.layout,
        data: out.into_vec(),
    }
}

/// Applies EXIF orientation 2–8 to the pixels; 1 and unknown values leave them as they are.
pub fn orient(image: Image, orientation: u8) -> Image {
    if !(2..=8).contains(&orientation) {
        return image;
    }
    let (w, h, n) = (
        image.width as usize,
        image.height as usize,
        image.layout.channels(),
    );
    let (ow, oh) = if orientation >= 5 { (h, w) } else { (w, h) };
    let mut data = Vec::with_capacity(image.data.len());
    for y in 0..oh {
        for x in 0..ow {
            let (sx, sy) = match orientation {
                2 => (w - 1 - x, y),
                3 => (w - 1 - x, h - 1 - y),
                4 => (x, h - 1 - y),
                5 => (y, x),
                6 => (y, h - 1 - x),
                7 => (w - 1 - y, h - 1 - x),
                _ => (w - 1 - y, x),
            };
            let at = (sy * w + sx) * n;
            data.extend_from_slice(&image.data[at..at + n]);
        }
    }
    Image {
        width: ow as u32,
        height: oh as u32,
        data,
        ..image
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Fit, Layout};

    fn fit(fit: Fit) -> Options {
        Options {
            fit,
            ..Options::default()
        }
    }

    #[test]
    fn presets_and_limits_shrink_to_the_smallest_result() {
        let vision = fit(Fit::Vision);
        assert_eq!(target(4032, 3024, &vision, true), (1238, 928));
        assert_eq!(target(1000, 800, &vision, true), (1000, 800));
        assert_eq!(target(4032, 3024, &Options::default(), true), (4032, 3024));
        let side = Options {
            max_side: Some(1000),
            ..vision
        };
        assert_eq!(target(4032, 3024, &side, true), (1000, 750));
        let boxed = Options {
            width: Some(400),
            height: Some(100),
            ..Options::default()
        };
        assert_eq!(target(4032, 3024, &boxed, true), (133, 100));
        assert_eq!(target(10, 5, &boxed, true), (10, 5));
        assert_eq!(target(10, 5, &boxed, false), (200, 100));
        assert_eq!(target(10, 5, &Options::default(), false), (10, 5));
        let tiny = Options {
            max_megapixels: Some(0.01),
            ..Options::default()
        };
        assert_eq!(target(65535, 2, &tiny, true), (18101, 1));
    }

    #[test]
    fn presets_fit_vision_token_and_patch_budgets() {
        for (w, h) in [
            (4000, 4000),
            (4032, 3024),
            (3840, 2160),
            (6000, 2000),
            (2000, 6000),
        ] {
            let (vw, vh) = target(w, h, &fit(Fit::Vision), true);
            assert!(
                vw.max(vh) <= 1568 && vw * vh <= 1_150_000,
                "{w}×{h}: {vw}×{vh}"
            );
            assert!(
                vw.div_ceil(28) * vh.div_ceil(28) <= 1568,
                "{w}×{h}: {vw}×{vh}"
            );
            let (hw, hh) = target(w, h, &fit(Fit::VisionHd), true);
            assert!(
                hw.max(hh) <= 2048 && hw * hh <= 2_400_000,
                "{w}×{h}: {hw}×{hh}"
            );
            assert!(
                hw.div_ceil(32) * hh.div_ceil(32) <= 2500,
                "{w}×{h}: {hw}×{hh}"
            );
        }
    }

    #[test]
    fn resizing_keeps_flat_colour_and_transparent_edges_clean() {
        let mut data = Vec::new();
        for x in 0..64 {
            data.extend(if x < 32 {
                [200, 30, 90, 255]
            } else {
                [0, 255, 0, 0]
            });
        }
        let image = Image {
            width: 64,
            height: 1,
            layout: Layout::Rgba,
            data: data.repeat(8),
        };
        let small = resize(&image, 16, 2);
        assert_eq!((small.width, small.height, small.data.len()), (16, 2, 128));
        // Premultiplied: the transparent green never bleeds into the opaque colour.
        for px in small.data.as_chunks::<4>().0 {
            if px[3] >= 64 {
                assert!(px[1].abs_diff(30) <= 2, "{px:?}");
            }
        }
    }
}
