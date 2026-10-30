//! Text output for people: a line per converted file and the summary on stdout, warnings and
//! errors on stderr. Colour only on a terminal without `NO_COLOR`; ASCII marks on a Windows
//! console whose output code page isn't UTF-8.

use std::env;
use std::io::{self, IsTerminal, Write};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;
use std::time::Duration;

use image_convert::engine::{FileResult, Report};
use image_convert::os::FormatStatus;

pub const DIM: &str = "2";
const RED: &str = "31";
const GREEN: &str = "32";
const YELLOW: &str = "33";

/// How one stream shows text.
#[derive(Debug, Clone, Copy, Default)]
pub struct Look {
    /// A terminal that takes ANSI sequences.
    pub ansi: bool,
    pub colour: bool,
    pub ascii: bool,
}

impl Look {
    pub fn stdout() -> Self {
        Self::new(io::stdout().is_terminal(), console::STDOUT)
    }

    pub fn stderr() -> Self {
        Self::new(io::stderr().is_terminal(), console::STDERR)
    }

    fn new(terminal: bool, stream: console::Stream) -> Self {
        let ansi = terminal && console::enable_ansi(stream);
        Self {
            ansi,
            colour: ansi && env::var_os("NO_COLOR").is_none_or(|v| v.is_empty()),
            ascii: terminal && !console::utf8(),
        }
    }

    /// `s` in the SGR style `sgr` when colour is on.
    pub fn paint(self, sgr: &str, s: &str) -> String {
        if self.colour {
            format!("\x1b[{sgr}m{s}\x1b[0m")
        } else {
            s.into()
        }
    }

    /// `unicode`, or `ascii` where the console can't show it.
    pub fn pick(self, unicode: &'static str, ascii: &'static str) -> &'static str {
        if self.ascii { ascii } else { unicode }
    }
}

#[cfg(windows)]
mod console {
    use windows::Win32::System::Console::{
        CONSOLE_MODE, ENABLE_VIRTUAL_TERMINAL_PROCESSING, GetConsoleMode, GetConsoleOutputCP,
        GetStdHandle, STD_ERROR_HANDLE, STD_HANDLE, STD_OUTPUT_HANDLE, SetConsoleMode,
    };

    pub type Stream = STD_HANDLE;
    pub const STDOUT: Stream = STD_OUTPUT_HANDLE;
    pub const STDERR: Stream = STD_ERROR_HANDLE;

    /// Turns on ANSI sequences for the console behind `stream`.
    pub fn enable_ansi(stream: Stream) -> bool {
        let mut mode = CONSOLE_MODE::default();
        // SAFETY: plain console calls on a standard handle; `mode` outlives the call.
        unsafe {
            GetStdHandle(stream).is_ok_and(|handle| {
                GetConsoleMode(handle, &mut mode).is_ok()
                    && SetConsoleMode(handle, mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING).is_ok()
            })
        }
    }

    pub fn utf8() -> bool {
        // SAFETY: no arguments; returns 0 without a console.
        unsafe { GetConsoleOutputCP() == 65001 }
    }
}

#[cfg(not(windows))]
mod console {
    pub type Stream = ();
    pub const STDOUT: Stream = ();
    pub const STDERR: Stream = ();

    pub fn enable_ansi(_: Stream) -> bool {
        true
    }

    pub fn utf8() -> bool {
        true
    }
}

/// A stream, how it shows text, and the indent every line gets.
pub struct Out<W> {
    pub w: W,
    pub look: Look,
    pub indent: &'static str,
}

impl<W: Write> Out<W> {
    pub fn new(w: W, look: Look) -> Self {
        Self {
            w,
            look,
            indent: "",
        }
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
    let look = out.look;
    let input = escape(&file.input);
    if let (Some(output), Some(w), Some(h), Some(bytes)) =
        (&file.output, file.width, file.height, file.bytes)
    {
        let format = match file.format {
            Some("png") => "PNG",
            Some("jpeg") => "JPEG",
            _ => "WebP",
        };
        let (mark, arrow) = (look.pick("✓", "ok"), look.pick("→", "->"));
        let by = look.pick("×", "x");
        let mut line = format!(
            "{} {input} {} {}  {w}{by}{h} {format}  {}",
            look.paint(GREEN, mark),
            look.paint(DIM, arrow),
            escape(output),
            size(bytes)
        );
        if let (Some(true), Some((sw, sh))) = (file.resized, file.upright_source()) {
            let from = format!(" {} from {sw}{by}{sh}", look.pick("·", "-"));
            line += &look.paint(DIM, &from);
        }
        out.line(&line)?;
    }
    for w in &file.warnings {
        let mark = err.look.paint(YELLOW, "!");
        err.line(&format!("{mark} {input}: {}", escape(&w.message)))?;
    }
    match &file.error {
        Some(e) => error(err, &format!("{}: {}", file.input, e.message), e.hint),
        None => Ok(()),
    }
}

/// `✗ message` and the hint indented on the next line.
pub fn error(err: &mut Out<impl Write>, message: &str, hint: Option<&str>) -> io::Result<()> {
    let mark = err.look.paint(RED, err.look.pick("✗", "x"));
    err.line(&format!("{mark} {}", escape(message)))?;
    match hint {
        Some(hint) => err.line(&format!("  {hint}")),
        None => Ok(()),
    }
}

/// `3 converted · 1 failed`.
pub fn summary(out: &mut Out<impl Write>, report: &Report) -> io::Result<()> {
    let mut line = format!("{} converted", report.converted);
    if report.failed > 0 {
        line += &format!(" {} {} failed", out.look.pick("·", "-"), report.failed);
    }
    let line = out.look.paint(DIM, &line);
    out.line(&line)
}

/// One line per format: `heic  missing    Install …`.
pub fn formats(out: &mut Out<impl Write>, rows: &[FormatStatus]) -> io::Result<()> {
    for row in rows {
        let (status, colour, rest) = match (row.status(), &row.missing) {
            ("available", _) => ("available", GREEN, row.decoder),
            ("missing", Some(e)) => ("missing  ", YELLOW, e.hint.unwrap_or(&e.message)),
            (_, e) => (
                "windows only",
                DIM,
                e.as_ref().map_or("", |e| e.message.as_str()),
            ),
        };
        let status = out.look.paint(colour, status);
        out.line(&format!("{:<4}  {status}  {rest}", row.format.name()))?;
    }
    Ok(())
}

/// `s` safe for a terminal: C0 controls but tab, DEL, C1 controls and the bidi controls that
/// can disguise text, as `<U+202E>` and the like.
pub fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\t' => out.push(c),
            '\0'..='\x1F'
            | '\x7F'..='\u{9F}'
            | '\u{61C}'
            | '\u{200E}'
            | '\u{200F}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2066}'..='\u{2069}' => out += &format!("<U+{:04X}>", c as u32),
            c => out.push(c),
        }
    }
    out
}

/// How long a file converts before the spinner shows, and how long each frame stays.
const SPIN_AFTER: Duration = Duration::from_millis(300);
const FRAME: Duration = Duration::from_millis(80);

/// Runs `work`; when it takes a while and stderr is a terminal, `⠋ {label}` spins on stderr
/// meanwhile and is erased before this returns.
pub fn spin<T>(err: &Out<impl Write>, label: &str, work: impl FnOnce() -> T) -> T {
    if !err.look.ansi {
        return work();
    }
    let (stop, stopped) = mpsc::channel();
    let (look, indent) = (err.look, err.indent);
    let line = format!("{indent}{{}} {}", escape(label));
    thread::scope(|s| {
        s.spawn(|| spinner(io::stderr(), look, &line, stopped));
        let result = work();
        drop(stop);
        result
    })
}

/// Draws `line` (its `{}` the frame) until `stopped` ends, then erases it.
fn spinner(mut w: impl Write, look: Look, line: &str, stopped: Receiver<()>) {
    let frames: &[&str] = if look.ascii {
        &["-", "\\", "|", "/"]
    } else {
        &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"]
    };
    let mut drawn = false;
    for (i, frame) in frames.iter().cycle().enumerate() {
        let wait = if i == 0 { SPIN_AFTER } else { FRAME };
        if stopped.recv_timeout(wait) != Err(RecvTimeoutError::Timeout) {
            break;
        }
        let text = look.paint(DIM, &line.replacen("{}", frame, 1));
        if write!(w, "\r{text}").and_then(|()| w.flush()).is_err() {
            return;
        }
        drawn = true;
    }
    if drawn {
        let _ = write!(w, "\r\x1b[K");
    }
}

/// `--debug` lines: the type, decoder and time of each step, and where a caught panic happened.
pub fn debug(err: &mut Out<impl Write>, file: &FileResult) -> io::Result<()> {
    let path = escape(&file.input);
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
    use image_convert::Options;
    use image_convert::engine::convert;
    use image_convert::error::Error;

    const COLOUR: Look = Look {
        ansi: true,
        colour: true,
        ascii: false,
    };
    const ASCII: Look = Look {
        ansi: true,
        colour: false,
        ascii: true,
    };

    #[test]
    fn sizes_read_in_bytes_kilobytes_or_megabytes() {
        assert_eq!(size(999), "999 bytes");
        assert_eq!(size(1000), "1 KB");
        assert_eq!(size(214_877), "215 KB");
        assert_eq!(size(999_499), "999 KB");
        assert_eq!(size(999_500), "1.0 MB");
        assert_eq!(size(2_345_678), "2.3 MB");
    }

    /// What `file` and `summary` print for a resized SVG with a skipped link and a missing
    /// file, on stdout and stderr.
    fn printed(look: Look) -> (String, String) {
        let svg = br#"<svg xmlns="http://www.w3.org/2000/svg" width="40" height="20"><image href="photo.jpg" width="4" height="4"/></svg>"#;
        let options = Options {
            width: Some(20),
            ..Options::default()
        };
        let (mut logo, _) = convert("logo.svg".into(), svg, &options);
        logo.output = Some("logo.png".into());
        let files = [
            logo,
            FileResult::failed("x.png".into(), Error::not_found("x.png")),
        ];
        let (mut out, mut err) = (Out::new(vec![], look), Out::new(vec![], look));
        for f in &files {
            file(&mut out, &mut err, f).unwrap();
        }
        summary(&mut out, &Report::new(&files)).unwrap();
        let bytes = files[0].bytes.unwrap();
        let text = |w: Vec<u8>| {
            String::from_utf8(w)
                .unwrap()
                .replace(&bytes.to_string(), "N")
        };
        (text(out.w), text(err.w))
    }

    #[test]
    fn colour_marks_the_lines_and_dims_the_details() {
        let (out, err) = printed(COLOUR);
        assert_eq!(
            out,
            "\x1b[32m✓\x1b[0m logo.svg \x1b[2m→\x1b[0m logo.png  20×10 PNG  N bytes\x1b[2m · from 40×20\x1b[0m\n\
             \x1b[2m1 converted · 1 failed\x1b[0m\n"
        );
        assert_eq!(
            err,
            "\x1b[33m!\x1b[0m logo.svg: An image linked from outside the file (photo.jpg) was left out.\n\
             \x1b[31m✗\x1b[0m x.png: File not found: x.png\n  Check the path.\n"
        );
    }

    #[test]
    fn plain_and_ascii_output_has_no_escape_sequences() {
        let (out, err) = printed(Look::default());
        assert_eq!(
            out,
            "✓ logo.svg → logo.png  20×10 PNG  N bytes · from 40×20\n1 converted · 1 failed\n"
        );
        assert!(err.ends_with("✗ x.png: File not found: x.png\n  Check the path.\n"));
        let (out, err) = printed(ASCII);
        assert_eq!(
            out,
            "ok logo.svg -> logo.png  20x10 PNG  N bytes - from 40x20\n1 converted - 1 failed\n"
        );
        assert!(err.ends_with("x x.png: File not found: x.png\n  Check the path.\n"));
    }

    #[test]
    fn an_indent_starts_every_line() {
        let mut err = Out::new(vec![], Look::default());
        err.indent = "  ";
        error(&mut err, "Unknown command '/x'.", Some("Try /help.")).unwrap();
        assert_eq!(
            String::from_utf8(err.w).unwrap(),
            "  ✗ Unknown command '/x'.\n    Try /help.\n"
        );
    }

    #[test]
    fn the_spinner_shows_only_after_a_while_and_erases_itself() {
        let (stop, stopped) = mpsc::channel::<()>();
        drop(stop);
        let mut quick = vec![];
        spinner(&mut quick, COLOUR, "{} Converting a.png", stopped);
        assert!(quick.is_empty());

        let (stop, stopped) = mpsc::channel::<()>();
        let slow = thread::spawn(move || {
            let mut w = vec![];
            spinner(&mut w, ASCII, "  {} Converting 2 of 5 · b.heic", stopped);
            String::from_utf8(w).unwrap()
        });
        thread::sleep(Duration::from_secs(1));
        drop(stop);
        let slow = slow.join().unwrap();
        assert!(
            slow.starts_with("\r  - Converting 2 of 5 · b.heic\r  \\ Converting 2 of 5 · b.heic\r")
        );
        assert!(slow.ends_with("\r\x1b[K"));
    }

    #[test]
    fn controls_and_bidi_characters_show_as_code_points() {
        assert_eq!(
            escape("hi\x1b[31m\r\n\0\x7f\u{9b}\u{202e}gnp.exe\u{61c}\u{2069}"),
            "hi<U+001B>[31m<U+000D><U+000A><U+0000><U+007F><U+009B><U+202E>gnp.exe<U+061C><U+2069>"
        );
        assert_eq!(escape("a\tb Grüße 東京 ✓"), "a\tb Grüße 東京 ✓");
        let all: String = (0..=0x2070).filter_map(char::from_u32).collect();
        let safe = escape(&all);
        assert!(!safe.chars().any(|c| c.is_control() && c != '\t'));
        assert!(!safe.contains(['\u{200E}', '\u{200F}', '\u{202A}', '\u{2066}']));
    }
}
