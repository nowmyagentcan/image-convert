//! The operating system's decoders: HEIC and AVIF through WIC (Windows), ImageIO (macOS) or the
//! system's libheif (Linux), and EMF/WMF through GDI+ on Windows only.

#[cfg(windows)]
mod gdiplus;
#[cfg(target_os = "macos")]
mod imageio;
#[cfg(target_os = "linux")]
mod libheif;
#[cfg(windows)]
mod wic;

use crate::bmff::Heif;
use crate::detect::Kind;
use crate::error::{Error, Warning};
use crate::{Options, Pixels};

#[cfg(target_os = "macos")]
use imageio as heif_os;
#[cfg(target_os = "linux")]
use libheif as heif_os;
#[cfg(windows)]
use wic as heif_os;

/// The `source.decoder` name of this OS's HEIF decoder.
#[cfg(windows)]
pub const HEIF_DECODER: &str = "wic";
#[cfg(target_os = "macos")]
pub const HEIF_DECODER: &str = "imageio";
#[cfg(target_os = "linux")]
pub const HEIF_DECODER: &str = "libheif";

/// Whether this machine can decode `kind` (HEIC or AVIF), or the `codec_missing` error saying
/// what to install.
pub fn heif_available(kind: Kind) -> Result<(), Error> {
    heif_os::check(kind)
}

/// Decodes the primary image of a HEIF or AVIF file that [`crate::bmff::heif`] read as `heif`.
pub fn decode_heif(data: &[u8], heif: &Heif) -> Result<Pixels, Error> {
    let mut px = heif_os::decode(data, heif)?;
    if px.colour.is_none() {
        px.colour = heif.colour.clone();
    }
    px.bit_depth = heif.bits;
    px.exif = heif.exif.clone();
    if heif.sequence {
        px.warnings.push(Warning::first_frame_only());
    }
    Ok(px)
}

/// Renders an EMF or WMF at the size `options` gives it, as [`crate::svg::render`] does an SVG:
/// the pixels and the drawing's own size in px. Only Windows has a renderer.
pub fn render_metafile(
    data: &[u8],
    kind: Kind,
    options: &Options,
) -> Result<(Pixels, (u32, u32)), Error> {
    #[cfg(windows)]
    return gdiplus::render(data, kind, options);
    #[cfg(not(windows))]
    {
        let _ = (data, kind, options);
        Err(Error::unsupported_on_platform())
    }
}

/// One row of `image-convert formats`: a §2.2 format, the decoder that reads it here, and why it
/// can't when it can't.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormatStatus {
    pub format: Kind,
    pub decoder: &'static str,
    /// `codec_missing` or `unsupported_on_platform`, with its message and hint.
    pub missing: Option<Error>,
}

impl FormatStatus {
    /// `available`, `missing` or `unsupported_on_platform`.
    pub fn status(&self) -> &'static str {
        match &self.missing {
            None => "available",
            Some(e) if e.code == "unsupported_on_platform" => e.code,
            Some(_) => "missing",
        }
    }
}

/// Every format of the input matrix in its order, probed on this machine.
pub fn formats() -> Vec<FormatStatus> {
    use Kind::*;
    let row = |format, decoder, missing| FormatStatus {
        format,
        decoder,
        missing,
    };
    let mut rows: Vec<_> = [Png, Jpeg, Gif, Bmp, Ico, Webp, Tiff, Jxl]
        .map(|kind| row(kind, "builtin", None))
        .into();
    rows.extend([Heic, Avif].map(|kind| row(kind, HEIF_DECODER, heif_available(kind).err())));
    rows.push(row(Svg, "builtin", None));
    let metafiles = (!cfg!(windows)).then(Error::unsupported_on_platform);
    rows.extend([Emf, Wmf].map(|kind| row(kind, "gdiplus", metafiles.clone())));
    rows.extend([Cr2, Cr3, Nef, Arw, Dng, Raf, Rw2, Pef].map(|kind| row(kind, "preview", None)));
    rows
}
