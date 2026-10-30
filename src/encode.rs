//! PNG, JPEG and WebP output with no metadata, and the alpha handling around them.

use std::borrow::Cow;

use image::codecs::png::{CompressionType, FilterType, PngEncoder};
use image::{ExtendedColorType, ImageEncoder};
use jpeg_encoder::{ChromaSubsamplingMethod, ColorType, Encoder};

use crate::detect::Kind;
use crate::{Format, Image, Layout, webp};

/// `--to auto`: PNG for sources drawn or stored losslessly and for anything transparent, else
/// JPEG.
pub fn auto(kind: Kind, alpha: bool) -> Format {
    use Kind::*;
    if alpha || matches!(kind, Png | Gif | Bmp | Ico | Tiff | Svg | Emf | Wmf) {
        Format::Png
    } else {
        Format::Jpeg
    }
}

/// Encodes 8-bit sRGB pixels at `quality` (JPEG and WebP); JPEG takes only grey or RGB.
pub fn encode(image: &Image, format: Format, quality: u8) -> Vec<u8> {
    let (width, height) = (image.width, image.height);
    let mut out = Vec::new();
    match format {
        Format::Png => {
            let color = [
                ExtendedColorType::L8,
                ExtendedColorType::La8,
                ExtendedColorType::Rgb8,
                ExtendedColorType::Rgba8,
            ][image.layout.channels() - 1];
            PngEncoder::new_with_quality(&mut out, CompressionType::Default, FilterType::Adaptive)
                .write_image(&image.data, width, height, color)
                .expect("PNG encodes any 8-bit image into memory");
        }
        Format::Jpeg => {
            let color = match image.layout {
                Layout::Grey => ColorType::Luma,
                Layout::Rgb => ColorType::Rgb,
                layout => unreachable!("{layout:?} is flattened before JPEG"),
            };
            // Chroma is 4:2:0 below quality 90 and 4:4:4 from 90, jpeg-encoder's default.
            let mut encoder = Encoder::new(&mut out, quality);
            encoder.set_optimized_huffman_tables(true);
            encoder.set_chroma_subsampling_method(ChromaSubsamplingMethod::Average);
            encoder
                .encode(&image.data, width as u16, height as u16, color)
                .expect("JPEG encodes sides up to 65,535 px");
        }
        Format::Webp => {
            let alpha = image.layout.has_alpha();
            let pixels = match image.layout {
                Layout::Grey => image.data.iter().flat_map(|&v| [v; 3]).collect(),
                Layout::GreyAlpha => image
                    .data
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .flat_map(|&[v, a]| [v, v, v, a])
                    .collect(),
                _ => Cow::Borrowed(&image.data[..]),
            };
            out = webp::encode(&pixels, width, height, alpha, quality)
                .expect("WebP encodes sides up to 16,383 px");
        }
    }
    out
}

/// Drops an alpha channel that is 255 everywhere, so only real transparency counts later.
pub fn drop_opaque_alpha(image: Image) -> Image {
    let n = image.layout.channels();
    let layout = match image.layout {
        Layout::GreyAlpha => Layout::Grey,
        Layout::Rgba => Layout::Rgb,
        _ => return image,
    };
    if image.data.chunks_exact(n).any(|px| px[n - 1] < 255) {
        return image;
    }
    let data = image
        .data
        .chunks_exact(n)
        .flat_map(|px| &px[..n - 1])
        .copied()
        .collect();
    Image {
        layout,
        data,
        ..image
    }
}

/// Composites a transparent image over `background` for JPEG; grey stays grey on a grey
/// background.
pub fn flatten(image: Image, background: [u8; 3]) -> Image {
    let n = image.layout.channels();
    let grey = image.layout == Layout::GreyAlpha && background.iter().all(|&c| c == background[0]);
    let mut data = Vec::with_capacity(image.data.len() / n * if grey { 1 } else { 3 });
    for px in image.data.chunks_exact(n) {
        let a = u32::from(px[n - 1]);
        let over =
            |c: u8, bg: u8| ((u32::from(c) * a + u32::from(bg) * (255 - a) + 127) / 255) as u8;
        if grey {
            data.push(over(px[0], background[0]));
        } else {
            for (i, &bg) in background.iter().enumerate() {
                data.push(over(px[if n == 2 { 0 } else { i }], bg));
            }
        }
    }
    Image {
        layout: if grey { Layout::Grey } else { Layout::Rgb },
        data,
        ..image
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image(layout: Layout, data: &[u8]) -> Image {
        Image {
            width: (data.len() / layout.channels()) as u32,
            height: 1,
            layout,
            data: data.to_vec(),
        }
    }

    #[test]
    fn auto_keeps_drawings_and_transparency_lossless() {
        assert_eq!(auto(Kind::Svg, false), Format::Png);
        assert_eq!(auto(Kind::Heic, true), Format::Png);
        assert_eq!(auto(Kind::Heic, false), Format::Jpeg);
        assert_eq!(auto(Kind::Cr3, false), Format::Jpeg);
    }

    #[test]
    fn opaque_alpha_is_dropped_and_real_alpha_kept() {
        let opaque = drop_opaque_alpha(image(Layout::GreyAlpha, &[9, 255, 8, 255]));
        assert_eq!((opaque.layout, opaque.data), (Layout::Grey, vec![9, 8]));
        let clear = image(Layout::Rgba, &[1, 2, 3, 254]);
        assert_eq!(drop_opaque_alpha(clear.clone()), clear);
    }

    #[test]
    fn flattening_blends_over_the_background() {
        let flat = flatten(
            image(Layout::GreyAlpha, &[0, 0, 0, 255, 200, 128]),
            [255; 3],
        );
        assert_eq!((flat.layout, flat.data), (Layout::Grey, vec![255, 0, 227]));
        let flat = flatten(image(Layout::GreyAlpha, &[0, 0]), [255, 0, 0]);
        assert_eq!((flat.layout, flat.data), (Layout::Rgb, vec![255, 0, 0]));
        let flat = flatten(
            image(Layout::Rgba, &[10, 20, 30, 0, 10, 20, 30, 255]),
            [0; 3],
        );
        assert_eq!(flat.data, [0, 0, 0, 10, 20, 30]);
    }
}
