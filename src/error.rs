//! Error and warning codes with the exact messages and hints that people and agents see.

use std::any::Any;
use std::fmt::Display;

use serde::Serialize;

const SUPPORTED: &str = "Supported: PNG, JPEG, GIF, BMP, ICO, WebP, TIFF, JPEG XL, HEIC/HEIF, AVIF, SVG, EMF/WMF (Windows), and RAW from Canon (CR2, CR3), Nikon (NEF), Sony (ARW), Adobe DNG, Fujifilm (RAF), Panasonic (RW2) and Pentax (PEF).";
const RAW_EDITOR: &str =
    "Export a JPEG from the camera's software, or use a RAW editor (darktable, RawTherapee).";
const APT_HEVC: &str = "Ubuntu/Debian: sudo apt install libheif1 libheif-plugin-libde265 · Fedora: sudo dnf install libheif-freeworld (RPM Fusion) · Arch: sudo pacman -S libheif";
const APT_AV1: &str = "Ubuntu/Debian: sudo apt install libheif1 libheif-plugin-dav1d · Fedora: sudo dnf install libheif · Arch: sudo pacman -S libheif";

/// A file, usage or runtime error: a stable `code`, a sentence for people and an optional fix.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Error {
    pub code: &'static str,
    pub message: String,
    pub hint: Option<&'static str>,
    /// Shown by `--debug` only: where a caught panic happened.
    #[serde(skip)]
    pub cause: Option<String>,
}

/// Something worth knowing about a file that still converted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Warning {
    pub code: &'static str,
    pub message: String,
}

/// Why an OS decoder can't read HEIC or AVIF on this machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    WindowsHeifExtension,
    WindowsHevc,
    WindowsAv1,
    MacOs,
    LinuxNoLibheif,
    LinuxOldLibheif(u8, u8),
    LinuxNoPlugin,
}

/// Why `--keep-exif` couldn't copy the EXIF block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExifSkip {
    NoneInSource,
    /// A source format with no EXIF path, by its label.
    Source(String),
    WebpOutput,
    TooLarge,
}

fn e(code: &'static str, message: impl Into<String>, hint: Option<&'static str>) -> Error {
    Error {
        code,
        message: message.into(),
        hint,
        cause: None,
    }
}

/// The text a panic was raised with.
pub fn panic_message(payload: &(dyn Any + Send)) -> String {
    match (
        payload.downcast_ref::<&str>(),
        payload.downcast_ref::<String>(),
    ) {
        (Some(s), _) => s.to_string(),
        (_, Some(s)) => s.clone(),
        _ => "a panic without a message".into(),
    }
}

fn w(code: &'static str, message: impl Into<String>) -> Warning {
    Warning {
        code,
        message: message.into(),
    }
}

impl Error {
    /// The JSON for an error that stops a run: `{"error": {"code", "message", "hint"}}`.
    pub fn to_json(&self) -> String {
        serde_json::json!({ "error": self }).to_string()
    }

    /// The process exit code this error forces: 2 for usage, 3 for runtime and internal, else 1.
    pub fn exit_code(&self) -> u8 {
        match self.code {
            "no_input"
            | "bad_format"
            | "bad_fit"
            | "bad_size"
            | "bad_megapixels"
            | "bad_bytes"
            | "bad_quality"
            | "bad_color"
            | "bad_extension"
            | "format_conflict"
            | "output_needs_one_input"
            | "stdin_needs_output"
            | "json_with_stdout" => 2,
            "internal" | "port_in_use" | "io" => 3,
            _ => 1,
        }
    }

    // File errors.

    pub fn not_found(path: &str) -> Self {
        e(
            "not_found",
            format!("File not found: {path}"),
            Some("Check the path."),
        )
    }

    pub fn wildcard(path: &str) -> Self {
        let hint = "Windows shells don't expand * or ?; pass the folder instead, e.g. image-convert photos";
        e("wildcard", format!("No file named '{path}'."), Some(hint))
    }

    pub fn unreadable(path: &str, cause: impl Display) -> Self {
        e("unreadable", format!("Can't read {path}: {cause}."), None)
    }

    pub fn empty_file() -> Self {
        e("empty_file", "The file is empty.", None)
    }

    pub fn too_large() -> Self {
        let hint = Some("Shrink or split it first.");
        e("too_large", "The file is larger than 512 MiB.", hint)
    }

    /// A recognised format we don't read, by name ("PSD", "Cursor (CUR)").
    pub fn unsupported_format(name: &str) -> Self {
        let hint = if name == "PDF" {
            "Render the pages to PNG with a PDF tool first."
        } else {
            SUPPORTED
        };
        e(
            "unsupported_format",
            format!("{name} files aren't supported."),
            Some(hint),
        )
    }

    /// A camera RAW kind we don't read, by make and kind ("Olympus ORF").
    pub fn unsupported_raw(kind: &str) -> Self {
        let message = format!("{kind} RAW files aren't supported yet.");
        e("unsupported_format", message, Some(RAW_EDITOR))
    }

    pub fn unknown_format() -> Self {
        let message = "This isn't an image format image-convert reads.";
        e("unsupported_format", message, Some(SUPPORTED))
    }

    pub fn unsupported_variant(format: &str, variant: impl Display) -> Self {
        let message = format!("{format} with {variant} isn't supported.");
        let hint = Some("Convert it to PNG or JPEG in the app that made it.");
        e("unsupported_variant", message, hint)
    }

    pub fn unsupported_on_platform() -> Self {
        let message = "EMF and WMF files can only be converted on Windows for now.";
        let hint = "Convert it on Windows, or export it as SVG or PNG from the app that made it.";
        e("unsupported_on_platform", message, Some(hint))
    }

    /// The OS can't decode this HEIC (`heic` true) or AVIF file.
    pub fn codec_missing(codec: Codec, heic: bool) -> Self {
        let (name, linux_hint) = if heic {
            ("HEIC", APT_HEVC)
        } else {
            ("AVIF", APT_AV1)
        };
        let (message, hint) = match codec {
            Codec::WindowsHeifExtension => (
                "Windows can't read HEIF images: the HEIF Image Extension isn't installed.".into(),
                Some(
                    "Install \"HEIF Image Extensions\" (free) from the Microsoft Store: ms-windows-store://pdp/?ProductId=9PMMSR1CGPWG",
                ),
            ),
            Codec::WindowsHevc => (
                "Windows can't decode HEIC: the HEVC decoder isn't installed.".into(),
                Some(
                    "Install \"HEVC Video Extensions from Device Manufacturer\" (free) from the Microsoft Store: ms-windows-store://pdp/?ProductId=9N4WGH0Z6VHQ (or \"HEVC Video Extensions\", 9NMZLZ57R3T7).",
                ),
            ),
            Codec::WindowsAv1 => (
                "Windows can't decode AVIF: the AV1 decoder isn't installed.".into(),
                Some(
                    "Install \"AV1 Video Extension\" (free) from the Microsoft Store: ms-windows-store://pdp/?ProductId=9MVZQVXJBQ9V",
                ),
            ),
            Codec::MacOs => (
                format!("This version of macOS can't decode {name}."),
                (!heic).then_some("AVIF needs macOS 13 or later."),
            ),
            Codec::LinuxNoLibheif => (
                format!("{name} needs the system's libheif library, which isn't installed."),
                Some(linux_hint),
            ),
            Codec::LinuxOldLibheif(major, minor) => (
                format!(
                    "The system's libheif {major}.{minor} is too old; 1.13 or newer is needed."
                ),
                Some(linux_hint),
            ),
            Codec::LinuxNoPlugin => (
                format!(
                    "The system's libheif has no {} decoder.",
                    if heic { "HEVC" } else { "AV1" }
                ),
                Some(linux_hint),
            ),
        };
        e("codec_missing", message, hint)
    }

    /// A raster image over 200 MP or 65,535 px per side; `vector` sources get a sizing hint.
    pub fn image_too_large(width: u64, height: u64, vector: bool) -> Self {
        let message = format!(
            "The image is {width}×{height} pixels; the limit is 200 megapixels and 65,535 pixels per side."
        );
        e("image_too_large", message, Some(too_large_hint(vector)))
    }

    /// A `.svgz`, `.emz` or `.wmz` that inflates past 64 MiB.
    pub fn inflates_too_large() -> Self {
        let message = "The compressed file expands to more than 64 MiB.";
        e("image_too_large", message, Some(too_large_hint(true)))
    }

    pub fn corrupt(format: &str, reason: impl Display) -> Self {
        let reason = reason.to_string();
        let message = format!(
            "Can't decode this {format} file: {}.",
            reason.trim_end_matches('.')
        );
        e(
            "corrupt_image",
            message,
            Some("The file may be damaged or cut short."),
        )
    }

    pub fn raw_no_preview(kind: &str) -> Self {
        let message = format!("This {kind} RAW file has no embedded preview image.");
        let hint = "Export a JPEG from a RAW editor (darktable, RawTherapee, Lightroom).";
        e("raw_no_preview", message, Some(hint))
    }

    pub fn cannot_fit(max: u64, width: u32, height: u32, bytes: u64) -> Self {
        let message = format!(
            "Can't make this image smaller than {max} bytes; the smallest try was {width}×{height}, {bytes} bytes."
        );
        let hint = Some("Raise --max-bytes, or use --to jpeg or --to webp.");
        e("cannot_fit", message, hint)
    }

    pub fn output_exists(path: &str) -> Self {
        let hint = Some("Pass --overwrite to replace it, or choose another -o.");
        e("output_exists", format!("{path} already exists."), hint)
    }

    pub fn output_is_input() -> Self {
        let message = "The output would replace the input file.";
        e("output_is_input", message, Some("Choose another -o."))
    }

    pub fn write_failed(path: &str, cause: impl Display) -> Self {
        e(
            "write_failed",
            format!("Can't write {path}: {cause}."),
            None,
        )
    }

    pub fn internal(panic: &str) -> Self {
        let message = format!("Something went wrong while converting this file: {panic}.");
        let hint = Some("Please report it, with the file if you can share it.");
        e("internal", message, hint)
    }

    // Usage errors.

    pub fn no_input() -> Self {
        e(
            "no_input",
            "No file given.",
            Some("Try: image-convert photo.heic"),
        )
    }

    pub fn bad_format(value: &str) -> Self {
        let hint = Some("Use auto, png, jpeg or webp.");
        e(
            "bad_format",
            format!("Unknown output format '{value}'."),
            hint,
        )
    }

    pub fn bad_fit(value: &str) -> Self {
        let hint = Some("Use none, vision or vision-hd.");
        e("bad_fit", format!("Unknown preset '{value}'."), hint)
    }

    pub fn bad_size(option: &str, min: u32, max: u32) -> Self {
        let message = format!("--{option} must be between {min} and {max}.");
        e("bad_size", message, None)
    }

    pub fn bad_megapixels() -> Self {
        let message = "--max-megapixels must be between 0.01 and 200.";
        e("bad_megapixels", message, None)
    }

    pub fn bad_bytes(value: &str) -> Self {
        let hint = Some("Use a number with an optional KB, MB, KiB or MiB, e.g. 500KB.");
        e(
            "bad_bytes",
            format!("Can't read the byte size '{value}'."),
            hint,
        )
    }

    pub fn bad_quality() -> Self {
        let message = "--quality must be between 1 and 100.";
        e("bad_quality", message, Some("Try --quality 85."))
    }

    pub fn bad_color(value: &str) -> Self {
        let hint = Some("Use white, black or #rrggbb.");
        e(
            "bad_color",
            format!("Can't read the colour '{value}'."),
            hint,
        )
    }

    pub fn bad_extension(name: &str) -> Self {
        let hint = Some("End -o with .png, .jpg or .webp, or pass --to.");
        e(
            "bad_extension",
            format!("Can't tell the format from '{name}'."),
            hint,
        )
    }

    pub fn format_conflict(format: &str, extension: &str) -> Self {
        let message = format!("--to {format} doesn't match the -o extension .{extension}.");
        e("format_conflict", message, None)
    }

    pub fn output_needs_one_input(inputs: usize) -> Self {
        let message = format!("-o names one file, but {inputs} inputs were given.");
        e(
            "output_needs_one_input",
            message,
            Some("Pass a folder to -o instead."),
        )
    }

    pub fn stdin_needs_output() -> Self {
        let hint = Some("Try: image-convert - -o out.jpg");
        e("stdin_needs_output", "Reading from stdin needs -o.", hint)
    }

    pub fn json_with_stdout() -> Self {
        e("json_with_stdout", "--json can't be used with -o -.", None)
    }

    // Runtime errors.

    pub fn port_in_use(port: u16) -> Self {
        let message = format!("Can't start the page: port {port} is in use.");
        e(
            "port_in_use",
            message,
            Some("Leave out --port to pick a free one."),
        )
    }

    pub fn io(cause: impl Display) -> Self {
        e("io", format!("Can't write output: {cause}."), None)
    }
}

fn too_large_hint(vector: bool) -> &'static str {
    if vector {
        "Use --width or --height to render it smaller."
    } else {
        "Crop it first."
    }
}

impl Warning {
    pub fn first_frame_only() -> Self {
        w(
            "first_frame_only",
            "Only the first frame of this animation was converted.",
        )
    }

    pub fn first_page_only(pages: u32) -> Self {
        let message = format!("This TIFF has {pages} pages; only the first was converted.");
        w("first_page_only", message)
    }

    pub fn raw_preview(width: u32, height: u32) -> Self {
        let message = format!(
            "Converted the camera's embedded preview ({width}×{height}), not a full RAW development."
        );
        w("raw_preview", message)
    }

    pub fn small_preview(width: u32, height: u32) -> Self {
        let message = format!("The embedded preview is only {width}×{height} pixels.");
        w("small_preview", message)
    }

    /// A drawing of `from` (w, h) px rendered at `to`.
    pub fn render_reduced(from: (u64, u64), to: (u32, u32)) -> Self {
        let message = format!(
            "The drawing is {}×{} px, so it was rendered at {}×{}. Pass --width to choose a size.",
            from.0, from.1, to.0, to.1
        );
        w("render_reduced", message)
    }

    /// The first skipped `href` (cut to 80 characters) and how many others were skipped.
    pub fn external_image_skipped(href: &str, more: usize) -> Self {
        let mut shown: String = href.chars().take(80).collect();
        if shown.len() < href.len() {
            shown.push('…');
        }
        if more > 0 {
            shown = format!("{shown} … and {more} more");
        }
        let message = format!("An image linked from outside the file ({shown}) was left out.");
        w("external_image_skipped", message)
    }

    pub fn icc_ignored(reason: impl Display) -> Self {
        let message = format!(
            "The embedded colour profile couldn't be used ({reason}), so colours were treated as sRGB."
        );
        w("icc_ignored", message)
    }

    pub fn hdr_clipped() -> Self {
        let message = "This HDR image was converted to standard range without tone mapping; bright areas may look clipped.";
        w("hdr_clipped", message)
    }

    pub fn alpha_dropped() -> Self {
        let message = "Windows didn't provide this image's transparency, so it was dropped.";
        w("alpha_dropped", message)
    }

    /// `background` as the user wrote it: `white`, `black` or `#rrggbb`.
    pub fn transparency_flattened(background: &str) -> Self {
        let message = format!(
            "JPEG has no transparency, so transparent areas were filled with {background}."
        );
        w("transparency_flattened", message)
    }

    /// `format` is `JPEG` or `WebP`.
    pub fn format_changed(format: &str, max: u64) -> Self {
        let message = format!("Saved as {format} instead of PNG to fit {max} bytes.");
        w("format_changed", message)
    }

    pub fn quality_reduced(quality: u8, max: u64) -> Self {
        let message = format!("Quality lowered to {quality} to fit {max} bytes.");
        w("quality_reduced", message)
    }

    pub fn size_reduced(width: u32, height: u32, max: u64) -> Self {
        let message = format!("Shrunk to {width}×{height} to fit {max} bytes.");
        w("size_reduced", message)
    }

    pub fn exif_not_kept(why: ExifSkip) -> Self {
        let reason = match why {
            ExifSkip::NoneInSource => "the source has none".to_string(),
            ExifSkip::Source(format) => format!("{format} sources aren't supported"),
            ExifSkip::WebpOutput => "WebP output doesn't carry it".to_string(),
            ExifSkip::TooLarge => "it is larger than 64 KB".to_string(),
        };
        w("exif_not_kept", format!("EXIF wasn't kept: {reason}."))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_codes_follow_the_error_kind() {
        assert_eq!(Error::no_input().exit_code(), 2);
        assert_eq!(Error::bad_size("max-side", 1, 65535).exit_code(), 2);
        assert_eq!(Error::internal("boom").exit_code(), 3);
        assert_eq!(Error::port_in_use(8080).exit_code(), 3);
        assert_eq!(Error::not_found("a.png").exit_code(), 1);
        assert_eq!(Error::unknown_format().exit_code(), 1);
    }

    #[test]
    fn messages_and_hints_read_as_written() {
        let err = Error::unsupported_variant("TIFF", "tiles");
        assert_eq!(err.message, "TIFF with tiles isn't supported.");
        assert_eq!(
            err.hint,
            Some("Convert it to PNG or JPEG in the app that made it.")
        );
        let err = Error::corrupt("PNG", "CRC error.");
        assert_eq!(err.message, "Can't decode this PNG file: CRC error.");
        assert_eq!(
            Error::unsupported_format("PDF").hint,
            Some("Render the pages to PNG with a PDF tool first.")
        );
        assert_eq!(
            Error::image_too_large(100_000, 100_000, false).message,
            "The image is 100000×100000 pixels; the limit is 200 megapixels and 65,535 pixels per side."
        );
        assert_eq!(
            Error::codec_missing(Codec::LinuxOldLibheif(1, 12), true).message,
            "The system's libheif 1.12 is too old; 1.13 or newer is needed."
        );
        assert_eq!(Error::codec_missing(Codec::MacOs, true).hint, None);
        assert_eq!(
            Warning::exif_not_kept(ExifSkip::Source("TIFF".into())).message,
            "EXIF wasn't kept: TIFF sources aren't supported."
        );
    }

    #[test]
    fn skipped_links_are_cut_and_counted() {
        let long = "x".repeat(100);
        let warning = Warning::external_image_skipped(&long, 2);
        let expected = format!(
            "An image linked from outside the file ({}… … and 2 more) was left out.",
            "x".repeat(80)
        );
        assert_eq!(warning.message, expected);
        assert_eq!(
            Warning::external_image_skipped("photo.jpg", 0).message,
            "An image linked from outside the file (photo.jpg) was left out."
        );
    }
}
