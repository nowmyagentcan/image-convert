//! The image-convert engine: input detection, decoders that share one decoded-image type, and
//! the pixel pipeline from there to an encoded PNG, JPEG or WebP.

pub mod color;
pub mod detect;
pub mod encode;
pub mod error;
pub mod jpeg;
pub mod raster;
pub mod resize;
pub mod webp;

use detect::Kind;
use error::{Error, Warning};

/// How the samples of one pixel are laid out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layout {
    Grey,
    GreyAlpha,
    Rgb,
    Rgba,
    /// Ink amounts, 0 = no ink.
    Cmyk,
}

impl Layout {
    pub fn channels(self) -> usize {
        match self {
            Layout::Grey => 1,
            Layout::GreyAlpha => 2,
            Layout::Rgb => 3,
            Layout::Rgba | Layout::Cmyk => 4,
        }
    }

    pub fn has_alpha(self) -> bool {
        matches!(self, Layout::GreyAlpha | Layout::Rgba)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Samples {
    U8(Vec<u8>),
    U16(Vec<u16>),
}

/// A decoded image, still in its source colour and orientation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pixels {
    /// The stored size, before orientation.
    pub width: u32,
    pub height: u32,
    pub layout: Layout,
    /// Row by row from the top, channels interleaved, no padding.
    pub samples: Samples,
    /// The source's bits per sample.
    pub bit_depth: u8,
    pub icc: Option<Vec<u8>>,
    /// The EXIF block, from its TIFF header on.
    pub exif: Option<Vec<u8>>,
    /// The EXIF orientation, 1–8.
    pub orientation: u8,
    /// Frames or pages in the file, 1 when unknown.
    pub frames: u32,
    pub warnings: Vec<Warning>,
}

/// An 8-bit sRGB image: grey, grey + alpha, RGB or RGBA, laid out as [`Pixels`] samples are.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Image {
    pub width: u32,
    pub height: u32,
    pub layout: Layout,
    pub data: Vec<u8>,
}

/// A size preset (spec §2.6).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Fit {
    #[default]
    None,
    Vision,
    VisionHd,
}

/// An output format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Png,
    Jpeg,
    Webp,
}

/// How to convert; limits left at `None` take the preset's value, or none.
#[derive(Debug, Clone, PartialEq)]
pub struct Options {
    /// `None` is `--to auto`.
    pub to: Option<Format>,
    /// `None` is 85, which byte fitting may lower.
    pub quality: Option<u8>,
    /// Fills transparency in JPEG output.
    pub background: [u8; 3],
    pub fit: Fit,
    pub max_side: Option<u32>,
    pub max_megapixels: Option<f64>,
    pub max_bytes: Option<u64>,
    /// The box to fit inside: raster shrinks into it, vector is drawn to fill it.
    pub width: Option<u32>,
    pub height: Option<u32>,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            to: None,
            quality: None,
            background: [255; 3],
            fit: Fit::None,
            max_side: None,
            max_megapixels: None,
            max_bytes: None,
            width: None,
            height: None,
        }
    }
}

impl Options {
    /// The longest side, megapixels and bytes allowed: explicit values over the preset's.
    pub fn limits(&self) -> (Option<u32>, Option<f64>, Option<u64>) {
        let (side, megapixels, bytes) = match self.fit {
            Fit::None => (None, None, None),
            Fit::Vision => (Some(1568), Some(1.15), Some(3_750_000)),
            Fit::VisionHd => (Some(2048), Some(2.4), Some(7_500_000)),
        };
        (
            self.max_side.or(side),
            self.max_megapixels.or(megapixels),
            self.max_bytes.or(bytes),
        )
    }
}

/// An encoded image and the facts results report about it.
#[derive(Debug, Clone, PartialEq)]
pub struct Output {
    pub bytes: Vec<u8>,
    pub format: Format,
    /// The size written, upright.
    pub width: u32,
    pub height: u32,
    /// JPEG and WebP only.
    pub quality: Option<u8>,
    /// Whether the size differs from the upright source's.
    pub resized: bool,
    pub color: color::Color,
    /// The decoder's warnings, then the pipeline's.
    pub warnings: Vec<Warning>,
}

/// Takes a decoded image through colour, size, orientation and alpha to its output format
/// (spec §2.4).
pub fn render(mut px: Pixels, kind: Kind, options: &Options) -> Result<Output, Error> {
    let (image, color) = color::to_srgb(&mut px);
    let image = encode::drop_opaque_alpha(image);
    let format = options
        .to
        .unwrap_or_else(|| encode::auto(kind, image.layout.has_alpha()));
    let mut image = resize::size_and_orient(image, px.orientation, options);
    if format == Format::Jpeg && image.layout.has_alpha() {
        image = encode::flatten(image, options.background);
        let background = match options.background {
            [255, 255, 255] => "white".to_string(),
            [0, 0, 0] => "black".to_string(),
            [r, g, b] => format!("#{r:02x}{g:02x}{b:02x}"),
        };
        px.warnings
            .push(Warning::transparency_flattened(&background));
    }
    let out = encode::fit(&image, format, options, &mut px.warnings)?;
    let upright = if px.orientation >= 5 {
        (px.height, px.width)
    } else {
        (px.width, px.height)
    };
    Ok(Output {
        resized: (out.width, out.height) != upright,
        bytes: out.bytes,
        format: out.format,
        width: out.width,
        height: out.height,
        quality: (out.format != Format::Png).then_some(out.quality),
        color,
        warnings: px.warnings,
    })
}

/// Refuses a header size of 0, or over 65,535 px per side or 200 megapixels, before decoding.
pub fn check_size(width: u64, height: u64, format: &str) -> Result<(), Error> {
    if width == 0 || height == 0 {
        Err(Error::corrupt(
            format,
            "its header gives a size of 0 pixels",
        ))
    } else if width > 65535 || height > 65535 || width * height > 200_000_000 {
        Err(Error::image_too_large(width, height, false))
    } else {
        Ok(())
    }
}

/// The EXIF block without a leading `Exif\0\0`, and its orientation (1 when absent or invalid).
fn exif_and_orientation(exif: Option<Vec<u8>>) -> (Option<Vec<u8>>, u8) {
    let exif = exif.map(|mut e| {
        if e.starts_with(b"Exif\0\0") {
            e.drain(..6);
        }
        e
    });
    let orientation = exif
        .as_deref()
        .and_then(image::metadata::Orientation::from_exif_chunk)
        .map_or(1, |o| o.to_exif());
    (exif, orientation)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_are_checked_against_the_limits() {
        assert_eq!(check_size(65535, 3051, "PNG"), Ok(()));
        assert_eq!(check_size(1, 0, "PNG").unwrap_err().code, "corrupt_image");
        assert_eq!(
            check_size(65536, 1, "PNG").unwrap_err().code,
            "image_too_large"
        );
        assert_eq!(
            check_size(20000, 10001, "PNG").unwrap_err().code,
            "image_too_large"
        );
    }

    #[test]
    fn exif_orientation_with_or_without_its_app1_prefix() {
        let mut exif = b"MM\0*\0\0\0\x08\0\x01\x01\x12\0\x03\0\0\0\x01\0\x06\0\0\0\0\0\0".to_vec();
        assert_eq!(exif_and_orientation(Some(exif.clone())).1, 6);
        exif.splice(0..0, *b"Exif\0\0");
        let (block, orientation) = exif_and_orientation(Some(exif));
        assert_eq!(
            (block.unwrap()[..4].to_vec(), orientation),
            (b"MM\0*".to_vec(), 6)
        );
        assert_eq!(exif_and_orientation(None), (None, 1));
    }
}
