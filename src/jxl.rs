//! JPEG XL through jxl-oxide, rendered upright to sRGB with moxcms and at most 2 GiB allocated.

use jxl_oxide::{AllocTracker, EnumColourEncoding, JxlImage, Moxcms, PixelFormat, RenderingIntent};

use crate::error::{Error, Warning};
use crate::{Layout, Pixels, Samples, check_size};

/// Decodes the first frame of a JPEG XL file to sRGB grey, RGB or either with alpha, with the
/// orientation already applied; also gives the stored size and the orientation.
pub fn decode(data: &[u8]) -> Result<(Pixels, (u32, u32, u8)), Error> {
    let failed = |e: Box<dyn std::error::Error + Send + Sync>| Error::corrupt("JPEG XL", e);
    let mut image = JxlImage::builder()
        .alloc_tracker(AllocTracker::with_limit(2 << 30))
        .read(data)
        .map_err(failed)?;
    let (width, height) = (image.width(), image.height());
    check_size(width.into(), height.into(), "JPEG XL")?;
    let header = image.image_header();
    let stored = (header.size.width, header.size.height);
    let metadata = &header.metadata;
    let orientation = metadata.orientation as u8;
    let bits = metadata.bit_depth.bits_per_sample();
    let encoding = if metadata.grayscale() {
        EnumColourEncoding::gray_srgb(RenderingIntent::Perceptual)
    } else {
        EnumColourEncoding::srgb(RenderingIntent::Perceptual)
    };
    image.set_cms(Moxcms);
    image.request_color_encoding(encoding);
    let frames = image.num_loaded_keyframes() as u32;
    let layout = match image.pixel_format() {
        PixelFormat::Gray => Layout::Grey,
        PixelFormat::Graya => Layout::GreyAlpha,
        PixelFormat::Rgb => Layout::Rgb,
        PixelFormat::Rgba => Layout::Rgba,
        format => {
            return Err(Error::corrupt(
                "JPEG XL",
                format!("it renders as {format:?}"),
            ));
        }
    };
    let render = image.render_frame(0).map_err(failed)?;
    // The decoder's own frame buffers go before ours is allocated.
    drop(image);
    let mut stream = render.stream();
    let (w, h) = (stream.width(), stream.height());
    if stream.channels() as usize != layout.channels() {
        let reason = format!("it renders {} channels", stream.channels());
        return Err(Error::corrupt("JPEG XL", reason));
    }
    let len = w as usize * h as usize * layout.channels();
    let samples = if bits > 8 {
        let mut buf = vec![0u16; len];
        stream.write_to_buffer(&mut buf);
        Samples::U16(buf)
    } else {
        let mut buf = vec![0u8; len];
        stream.write_to_buffer(&mut buf);
        Samples::U8(buf)
    };
    let px = Pixels {
        width: w,
        height: h,
        layout,
        samples,
        bit_depth: bits.min(16) as u8,
        colour: None,
        exif: None,
        orientation: 1,
        frames,
        warnings: (frames > 1)
            .then(Warning::first_frame_only)
            .into_iter()
            .collect(),
    };
    Ok((px, (stored.0, stored.1, orientation)))
}
