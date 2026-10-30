//! HEIC and AVIF through ImageIO: the primary image made upright, then drawn by Core Graphics
//! into an sRGB bitmap (spec §3.3).

use objc2_core_foundation::{
    CFBoolean, CFData, CFDictionary, CFNumber, CFString, CFType, CGPoint, CGRect, CGSize,
};
use objc2_core_graphics::{
    CGBitmapContextCreate, CGColorSpace, CGContext, CGImage, CGImageAlphaInfo,
    CGImageByteOrderInfo, kCGColorSpaceSRGB,
};
use objc2_image_io::{
    CGImageSource, kCGImageSourceCreateThumbnailFromImageAlways,
    kCGImageSourceCreateThumbnailWithTransform, kCGImageSourceShouldCacheImmediately,
    kCGImageSourceThumbnailMaxPixelSize,
};

use crate::bmff::Heif;
use crate::color::{Colour, describe};
use crate::detect::Kind;
use crate::error::{Codec, Error};
use crate::{Layout, Pixels, Samples};

/// Whether ImageIO lists `kind` among the types it reads (AVIF from macOS 13).
pub fn check(kind: Kind) -> Result<(), Error> {
    let uti = if kind == Kind::Heic {
        "public.heic"
    } else {
        "public.avif"
    };
    let types = unsafe { CGImageSource::type_identifiers() };
    let types = unsafe { types.cast_unchecked::<CFString>() };
    if types.iter().any(|t| t.to_string() == uti) {
        Ok(())
    } else {
        Err(Error::codec_missing(Codec::MacOs, kind == Kind::Heic))
    }
}

/// Decodes the primary image as upright, unpremultiplied sRGB RGBA.
pub fn decode(data: &[u8], heif: &Heif) -> Result<Pixels, Error> {
    check(heif.kind)?;
    let failed = || Error::corrupt(&heif.kind.label(), "macOS couldn't read it");
    let source =
        unsafe { CGImageSource::with_data(&CFData::from_bytes(data), None) }.ok_or_else(failed)?;
    let yes: &CFType = CFBoolean::new(true);
    let side = CFNumber::new_i64(heif.width.max(heif.height).into());
    let keys = unsafe {
        [
            kCGImageSourceCreateThumbnailFromImageAlways,
            kCGImageSourceCreateThumbnailWithTransform,
            kCGImageSourceShouldCacheImmediately,
            kCGImageSourceThumbnailMaxPixelSize,
        ]
    };
    let options = CFDictionary::<CFString, CFType>::from_slices(&keys, &[yes, yes, yes, &side]);
    let image = unsafe {
        source.thumbnail_at_index(source.primary_image_index(), Some(options.as_opaque()))
    }
    .ok_or_else(failed)?;
    let (width, height) = (CGImage::width(Some(&image)), CGImage::height(Some(&image)));
    let mut rgba = vec![0u8; width * height * 4];
    let space = CGColorSpace::with_name(Some(unsafe { kCGColorSpaceSRGB })).ok_or_else(failed)?;
    let context = unsafe {
        CGBitmapContextCreate(
            rgba.as_mut_ptr().cast(),
            width,
            height,
            8,
            width * 4,
            Some(&space),
            CGImageAlphaInfo::PremultipliedLast.0 | CGImageByteOrderInfo::Order32Big.0,
        )
    }
    .ok_or_else(failed)?;
    let rect = CGRect::new(
        CGPoint::new(0.0, 0.0),
        CGSize::new(width as f64, height as f64),
    );
    CGContext::draw_image(Some(&context), rect, Some(&image));
    drop(context);
    for px in rgba.as_chunks_mut::<4>().0 {
        let a = u32::from(px[3]);
        if a != 0 && a != 255 {
            for c in &mut px[..3] {
                *c = ((u32::from(*c) * 255 + a / 2) / a).min(255) as u8;
            }
        }
    }
    Ok(Pixels {
        width: width as u32,
        height: height as u32,
        layout: Layout::Rgba,
        samples: Samples::U8(rgba),
        bit_depth: 8,
        // Core Graphics did the colour conversion; results still name the source's colour.
        colour: Some(Colour::Converted(describe(heif.colour.as_ref()))),
        exif: None,
        orientation: 1,
        frames: 1,
        warnings: Vec::new(),
    })
}
