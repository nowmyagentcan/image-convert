//! Text output for people: a line per converted file and the summary on stdout, warnings and
//! errors on stderr.

use std::io::{self, Write};

use image_convert::engine::{FileResult, Report};
use image_convert::os::FormatStatus;

/// A stream and the indent every line gets.
pub struct Out<W> {
    pub w: W,
    pub indent: &'static str,
}

impl<W: Write> Out<W> {
    pub fn new(w: W) -> Self {
        Self { w, indent: "" }
    }

    pub fn line(&mut self, s: &str) -> io::Result<()> {
        writeln!(self.w, "{}{s}", self.indent)
    }
}

/// `1234 bytes` as `1 KB`, `2.3 MB` (1 KB = 1000 bytes).
fn size(bytes: u64) -> String {
    let kb = (bytes as f64 / 1000.0).round();
    match bytes {
        0..1000 => format!("{bytes} bytes"),
        _ if kb < 1000.0 => format!("{kb} KB"),
        _ => format!("{:.1} MB", bytes as f64 / 1e6),
    }
}

/// `✓ in → out  W×H FORMAT  size · from w×h` on stdout, then each warning and the error on
/// stderr.
pub fn file(
    out: &mut Out<impl Write>,
    err: &mut Out<impl Write>,
    file: &FileResult,
) -> io::Result<()> {
    if let (Some(output), Some(w), Some(h), Some(bytes)) =
        (&file.output, file.width, file.height, file.bytes)
    {
        let format = match file.format {
            Some("png") => "PNG",
            Some("jpeg") => "JPEG",
            _ => "WebP",
        };
        let mut line = format!(
            "✓ {} → {output}  {w}×{h} {format}  {}",
            file.input,
            size(bytes)
        );
        if let (Some(true), Some((sw, sh))) = (file.resized, file.upright_source()) {
            line += &format!(" · from {sw}×{sh}");
        }
        out.line(&line)?;
    }
    for w in &file.warnings {
        err.line(&format!("! {}: {}", file.input, w.message))?;
    }
    match &file.error {
        Some(e) => error(err, &format!("{}: {}", file.input, e.message), e.hint),
        None => Ok(()),
    }
}

/// `✗ message` and the hint indented on the next line.
pub fn error(err: &mut Out<impl Write>, message: &str, hint: Option<&str>) -> io::Result<()> {
    err.line(&format!("✗ {message}"))?;
    match hint {
        Some(hint) => err.line(&format!("  {hint}")),
        None => Ok(()),
    }
}

/// `3 converted · 1 failed`.
pub fn summary(out: &mut Out<impl Write>, report: &Report) -> io::Result<()> {
    let mut line = format!("{} converted", report.converted);
    if report.failed > 0 {
        line += &format!(" · {} failed", report.failed);
    }
    out.line(&line)
}

/// One line per format: `heic  missing    Install …`.
pub fn formats(out: &mut Out<impl Write>, rows: &[FormatStatus]) -> io::Result<()> {
    for row in rows {
        let (status, rest) = match (row.status(), &row.missing) {
            ("available", _) => ("available", row.decoder),
            ("missing", Some(e)) => ("missing", e.hint.unwrap_or(&e.message)),
            (_, e) => (
                "windows only",
                e.as_ref().map_or("", |e| e.message.as_str()),
            ),
        };
        out.line(&format!("{:<4}  {status:<9}  {rest}", row.format.name()))?;
    }
    Ok(())
}

/// `--debug` lines: the type, decoder and time of each step, and where a caught panic happened.
pub fn debug(err: &mut Out<impl Write>, file: &FileResult) -> io::Result<()> {
    let path = &file.input;
    let kind = file.input_format.as_deref().unwrap_or("unknown type");
    let decoder = file.source.as_ref().map_or("none", |s| s.decoder);
    let mut line = format!("debug: {path}: {kind}, decoder {decoder}");
    for (i, (step, took)) in file.steps.iter().enumerate() {
        let ms = took.as_secs_f64() * 1000.0;
        line += &format!("{} {step} {ms:.1} ms", if i == 0 { ":" } else { "," });
    }
    err.line(&line)?;
    if let Some(e) = &file.error {
        let at = e
            .cause
            .as_ref()
            .map(|c| format!(" (panicked at {c})"))
            .unwrap_or_default();
        err.line(&format!("debug: {path}: error {}{at}", e.code))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_read_in_bytes_kilobytes_or_megabytes() {
        assert_eq!(size(999), "999 bytes");
        assert_eq!(size(1000), "1 KB");
        assert_eq!(size(214_877), "215 KB");
        assert_eq!(size(999_499), "999 KB");
        assert_eq!(size(999_500), "1.0 MB");
        assert_eq!(size(2_345_678), "2.3 MB");
    }
}
