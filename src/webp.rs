//! Lossy WebP encoding through libwebp's simple API.

use std::ffi::{c_float, c_int, c_void};

unsafe extern "C" {
    fn WebPEncodeRGB(
        rgb: *const u8,
        width: c_int,
        height: c_int,
        stride: c_int,
        quality: c_float,
        output: *mut *mut u8,
    ) -> usize;
    fn WebPEncodeRGBA(
        rgba: *const u8,
        width: c_int,
        height: c_int,
        stride: c_int,
        quality: c_float,
        output: *mut *mut u8,
    ) -> usize;
    fn WebPFree(ptr: *mut c_void);
}

/// WebP's limit on each side.
pub const MAX_SIDE: u32 = 16383;

/// Encodes 8-bit RGB (`alpha` false) or RGBA pixels, row by row with no padding, at quality 1–100.
/// `None` when a side is 0 or over [`MAX_SIDE`], or libwebp fails.
pub fn encode(pixels: &[u8], width: u32, height: u32, alpha: bool, quality: u8) -> Option<Vec<u8>> {
    let channels = if alpha { 4 } else { 3 };
    assert_eq!(pixels.len(), width as usize * height as usize * channels);
    if !(1..=MAX_SIDE).contains(&width) || !(1..=MAX_SIDE).contains(&height) {
        return None;
    }
    let encode_fn = if alpha { WebPEncodeRGBA } else { WebPEncodeRGB };
    let mut out = std::ptr::null_mut();
    let stride = (width as usize * channels) as c_int;
    // SAFETY: `pixels` holds `height` rows of `stride` bytes (checked above), and libwebp only reads it.
    let len = unsafe {
        encode_fn(
            pixels.as_ptr(),
            width as c_int,
            height as c_int,
            stride,
            quality as c_float,
            &mut out,
        )
    };
    if len == 0 {
        return None;
    }
    // SAFETY: on success libwebp returns `len` bytes at `out`, copied here before WebPFree.
    let bytes = unsafe { std::slice::from_raw_parts(out, len) }.to_vec();
    unsafe { WebPFree(out.cast()) };
    Some(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gradient(width: u32, height: u32, alpha: bool) -> Vec<u8> {
        let mut px = Vec::new();
        for y in 0..height {
            for x in 0..width {
                px.extend([(x * 4) as u8, (y * 4) as u8, 128]);
                if alpha {
                    px.push(if x < width / 2 { 255 } else { 0 });
                }
            }
        }
        px
    }

    #[test]
    fn encodes_rgb_and_rgba_that_decode_back() {
        for alpha in [false, true] {
            let webp = encode(&gradient(64, 48, alpha), 64, 48, alpha, 85).unwrap();
            assert_eq!((&webp[..4], &webp[8..12]), (&b"RIFF"[..], &b"WEBP"[..]));
            let img = image::load_from_memory_with_format(&webp, image::ImageFormat::WebP).unwrap();
            assert_eq!(
                (img.width(), img.height(), img.color().has_alpha()),
                (64, 48, alpha)
            );
            if alpha {
                let rgba = img.to_rgba8();
                assert_eq!(
                    (rgba.get_pixel(0, 0)[3], rgba.get_pixel(63, 0)[3]),
                    (255, 0)
                );
            }
        }
    }

    #[test]
    fn refuses_sides_webp_cannot_hold() {
        assert_eq!(encode(&[], 0, 0, false, 85), None);
        assert_eq!(
            encode(
                &vec![0; 3 * (MAX_SIDE as usize + 1)],
                MAX_SIDE + 1,
                1,
                false,
                85
            ),
            None
        );
    }
}
