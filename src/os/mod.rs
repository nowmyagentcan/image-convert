//! The operating system's decoders: HEIC and AVIF through WIC (Windows), ImageIO (macOS) or the
//! system's libheif (Linux), and EMF/WMF through GDI+ on Windows only.

#[cfg(target_os = "macos")]
mod imageio;
#[cfg(windows)]
mod wic;

use crate::Pixels;
use crate::bmff::Heif;
use crate::detect::Kind;
use crate::error::{Error, Warning};

#[cfg(target_os = "macos")]
use imageio as heif_os;
#[cfg(windows)]
use wic as heif_os;

/// The `source.decoder` name of this OS's HEIF decoder.
#[cfg(windows)]
pub const HEIF_DECODER: &str = "wic";
#[cfg(target_os = "macos")]
pub const HEIF_DECODER: &str = "imageio";
#[cfg(target_os = "linux")]
pub const HEIF_DECODER: &str = "libheif";

/// Elsewhere, for now, nothing decodes HEIF.
#[cfg(target_os = "linux")]
mod heif_os {
    use super::*;
    use crate::error::Codec;

    pub fn check(kind: Kind) -> Result<(), Error> {
        Err(Error::codec_missing(
            Codec::LinuxNoLibheif,
            kind == Kind::Heic,
        ))
    }

    pub fn decode(_: &[u8], heif: &Heif) -> Result<Pixels, Error> {
        check(heif.kind).map(|()| unreachable!())
    }
}

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
