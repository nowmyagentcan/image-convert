//! The image-convert engine: input detection, decoders that share one decoded-image type, and
//! the pixel pipeline from there to an encoded PNG, JPEG or WebP.

pub mod bmff;
pub mod color;
pub mod detect;
pub mod encode;
pub mod engine;
pub mod error;
pub mod jpeg;
pub mod jxl;
pub mod os;
pub mod paths;
pub mod raster;
pub mod raw;
pub mod resize;
pub mod svg;
mod svg_demand;
pub mod tiff;
pub mod webp;

use detect::Kind;
use error::{Error, ExifSkip, Warning};
use serde::{Deserialize, Deserializer};

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
    pub colour: Option<color::Colour>,
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

impl Format {
    /// The `format` value in results.
    pub fn name(self) -> &'static str {
        match self {
            Format::Png => "png",
            Format::Jpeg => "jpeg",
            Format::Webp => "webp",
        }
    }
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
    /// Copies the source's EXIF into JPEG and PNG output.
    pub keep_exif: bool,
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
            keep_exif: false,
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

/// Option values as the CLI, the page's `X-Options` and the MCP tools give them, before they
/// are checked; numbers are kept as text, so every face refuses a value with the same error.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Choices {
    pub to: Option<String>,
    pub fit: Option<String>,
    #[serde(default, deserialize_with = "text")]
    pub max_side: Option<String>,
    #[serde(default, deserialize_with = "text")]
    pub max_megapixels: Option<String>,
    #[serde(default, deserialize_with = "text")]
    pub max_bytes: Option<String>,
    #[serde(default, deserialize_with = "text")]
    pub width: Option<String>,
    #[serde(default, deserialize_with = "text")]
    pub height: Option<String>,
    #[serde(default, deserialize_with = "text")]
    pub quality: Option<String>,
    pub background: Option<String>,
    #[serde(default)]
    pub keep_exif: bool,
}

/// A JSON number or string as its text.
fn text<'de, D: Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    Ok(
        Option::<serde_json::Value>::deserialize(d)?.map(|v| match v {
            serde_json::Value::String(s) => s,
            v => v.to_string(),
        }),
    )
}

impl Choices {
    /// The conversion options, or the usage error a value gives.
    pub fn options(&self) -> Result<Options, Error> {
        let size = |value: &Option<String>, name| {
            let n = value
                .as_deref()
                .map(|v| v.parse().ok().filter(|n| (1..=65535).contains(n)));
            n.map(|n| n.ok_or_else(|| Error::bad_size(name, 1, 65535)))
                .transpose()
        };
        let to = match self.to.as_deref().map(str::to_ascii_lowercase).as_deref() {
            None | Some("auto") => None,
            Some("png") => Some(Format::Png),
            Some("jpeg" | "jpg") => Some(Format::Jpeg),
            Some("webp") => Some(Format::Webp),
            Some(_) => return Err(Error::bad_format(self.to.as_deref().unwrap_or_default())),
        };
        let fit = match self.fit.as_deref() {
            None | Some("none") => Fit::None,
            Some("vision") => Fit::Vision,
            Some("vision-hd") => Fit::VisionHd,
            Some(other) => return Err(Error::bad_fit(other)),
        };
        let max_megapixels = self.max_megapixels.as_deref().map(|v| {
            v.parse()
                .ok()
                .filter(|x: &f64| (0.01..=200.0).contains(x))
                .ok_or_else(Error::bad_megapixels)
        });
        let quality = self.quality.as_deref().map(|v| {
            v.parse()
                .ok()
                .filter(|q| (1..=100).contains(q))
                .ok_or_else(Error::bad_quality)
        });
        let max_bytes = self
            .max_bytes
            .as_deref()
            .map(|v| byte_size(v).ok_or_else(|| Error::bad_bytes(v)));
        let background = self
            .background
            .as_deref()
            .map(|v| colour(v).ok_or_else(|| Error::bad_color(v)));
        Ok(Options {
            to,
            fit,
            max_side: size(&self.max_side, "max-side")?,
            max_megapixels: max_megapixels.transpose()?,
            max_bytes: max_bytes.transpose()?,
            width: size(&self.width, "width")?,
            height: size(&self.height, "height")?,
            quality: quality.transpose()?,
            background: background.transpose()?.unwrap_or([255; 3]),
            keep_exif: self.keep_exif,
        })
    }
}

/// `500KB`, `2MB`, `3MiB` or plain digits as bytes, between 1 KB and 1 GiB.
fn byte_size(value: &str) -> Option<u64> {
    let digits = value.len() - value.trim_start_matches(|c: char| c.is_ascii_digit()).len();
    let (number, unit) = value.split_at(digits);
    let unit = match unit.to_ascii_lowercase().as_str() {
        "" => 1,
        "kb" => 1000,
        "mb" => 1_000_000,
        "kib" => 1024,
        "mib" => 1 << 20,
        _ => return None,
    };
    let bytes = number.parse::<u64>().ok()?.checked_mul(unit)?;
    (1000..=1 << 30).contains(&bytes).then_some(bytes)
}

/// `white`, `black` or `#rrggbb`.
fn colour(value: &str) -> Option<[u8; 3]> {
    match value.to_ascii_lowercase().as_str() {
        "white" => Some([255; 3]),
        "black" => Some([0; 3]),
        hex => {
            let hex = hex
                .strip_prefix('#')
                .filter(|h| h.len() == 6 && h.is_ascii())?;
            let byte = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).ok();
            Some([byte(0)?, byte(2)?, byte(4)?])
        }
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
    /// Whether the source's EXIF was written.
    pub exif: bool,
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
    let exif = options.keep_exif.then(|| match px.exif.as_deref() {
        _ if !matches!(
            kind,
            Kind::Jpeg | Kind::Png | Kind::Webp | Kind::Heic | Kind::Avif
        ) =>
        {
            Err(ExifSkip::Source(kind.label()))
        }
        None => Err(ExifSkip::NoneInSource),
        Some(block) => Ok(upright_exif(block)),
    });
    let block = exif.as_ref().and_then(|e| e.as_deref().ok());
    let out = encode::fit(&image, format, block, options, &mut px.warnings)?;
    let kept = block.is_some_and(|b| encode::carries_exif(out.format, b.len()));
    let skipped = match exif {
        Some(Err(why)) => Some(why),
        Some(Ok(_)) if !kept && out.format == Format::Webp => Some(ExifSkip::WebpOutput),
        Some(Ok(_)) if !kept => Some(ExifSkip::TooLarge),
        _ => None,
    };
    px.warnings.extend(skipped.map(Warning::exif_not_kept));
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
        exif: kept,
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
    let orientation = exif.as_deref().and_then(exif_orientation).unwrap_or(1);
    (exif, orientation)
}

/// The Orientation tag of an EXIF or TIFF block (from its TIFF header on), when it has one.
fn exif_orientation(block: &[u8]) -> Option<u8> {
    image::metadata::Orientation::from_exif_chunk(block).map(|o| o.to_exif())
}

/// A copy of an EXIF block with its Orientation (IFD0 tag 274) set to 1, as the pixels are
/// upright now.
fn upright_exif(exif: &[u8]) -> Vec<u8> {
    let mut out = exif.to_vec();
    let entry = tiff::Tiff::new(exif)
        .and_then(|(t, first)| t.ifd(first))
        .and_then(|(ifd, _)| ifd.get(0x112).copied());
    if let Some(e) = entry.filter(|e| e.kind == 3) {
        let one = if exif.starts_with(b"MM") {
            [0, 1]
        } else {
            [1, 0]
        };
        out[e.at..e.at + 2].copy_from_slice(&one);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn byte_sizes_take_decimal_and_binary_units_within_range() {
        assert_eq!(byte_size("500KB"), Some(500_000));
        assert_eq!(byte_size("2mb"), Some(2_000_000));
        assert_eq!(byte_size("3MiB"), Some(3 << 20));
        assert_eq!(byte_size("1KiB"), Some(1024));
        assert_eq!(byte_size("1000"), Some(1000));
        assert_eq!(byte_size("1024MiB"), Some(1 << 30));
        for bad in [
            "999",
            "1025MiB",
            "KB",
            "1.5MB",
            "5 KB",
            "-5KB",
            "5GB",
            "",
            "99999999999999999999",
        ] {
            assert_eq!(byte_size(bad), None, "{bad}");
        }
    }

    #[test]
    fn colours_are_names_or_hex() {
        assert_eq!(colour("White"), Some([255; 3]));
        assert_eq!(colour("black"), Some([0; 3]));
        assert_eq!(colour("#1A2b3C"), Some([0x1a, 0x2b, 0x3c]));
        for bad in ["#12345", "#1234567", "red", "123456", "#12345g", "#ééé"] {
            assert_eq!(colour(bad), None, "{bad}");
        }
    }

    #[test]
    fn json_choices_take_numbers_as_their_text() {
        let choices: Choices =
            serde_json::from_str(r#"{"max_side":1568,"quality":"90","max_bytes":300000}"#).unwrap();
        let options = choices.options().unwrap();
        assert_eq!(
            (options.max_side, options.quality, options.max_bytes),
            (Some(1568), Some(90), Some(300_000))
        );
        let half: Choices = serde_json::from_str(r#"{"width":1.5}"#).unwrap();
        assert_eq!(half.options().unwrap_err().code, "bad_size");
        assert!(serde_json::from_str::<Choices>(r#"{"colour":"red"}"#).is_err());
    }

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

    #[test]
    fn kept_exif_is_made_upright_in_either_byte_order() {
        let big = b"MM\0*\0\0\0\x08\0\x01\x01\x12\0\x03\0\0\0\x01\0\x06\0\0\0\0\0\0";
        let fixed = upright_exif(big);
        assert_eq!((fixed.len(), &fixed[18..20]), (big.len(), &[0, 1][..]));
        let little = b"II*\0\x08\0\0\0\x01\0\x12\x01\x03\0\x01\0\0\0\x08\0\0\0\0\0";
        assert_eq!(&upright_exif(little)[18..20], [1, 0]);
        assert_eq!(upright_exif(b"II*\0\xff\xff\0\0"), b"II*\0\xff\xff\0\0");
    }
}
