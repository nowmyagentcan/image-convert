//! Interactive mode: a banner, then a prompt that converts the paths typed or dropped on each
//! line.

use std::io::{self, BufRead, Write};
use std::mem;
use std::path::{Path, PathBuf};

use crate::text::{self, BOLD, DIM, Out};

const SENTENCE: &str = "Convert images agents and apps can't open to PNG, JPEG or WebP.";

const HELP: [&str; 5] = [
    "Type or drop image paths to convert them to PNG or JPEG beside the original.",
    "Quote a path with spaces. A folder converts every image in it into <folder>/converted/.",
    "For other formats and sizes, run: image-convert --help",
    "/help   show this",
    "/quit   leave (Ctrl+C works too)",
];

/// Shows the banner, then hands each line's paths to `convert` until `/quit` or the end of
/// `input`. Everything after the banner is indented under the prompt.
pub fn run<O: Write, E: Write>(
    mut input: impl BufRead,
    out: &mut Out<O>,
    err: &mut Out<E>,
    mut convert: impl FnMut(Vec<PathBuf>, &mut Out<O>, &mut Out<E>) -> io::Result<()>,
) -> io::Result<()> {
    banner(out)?;
    let prompt = out.look.paint(BOLD, out.look.pick("›", ">"));
    (out.indent, err.indent) = ("  ", "  ");
    let mut line = String::new();
    loop {
        write!(out.w, "{prompt} ")?;
        out.w.flush()?;
        line.clear();
        if input.read_line(&mut line)? == 0 {
            return writeln!(out.w);
        }
        let words = split(&line, cfg!(not(windows)));
        match words.as_slice() {
            [] => continue,
            [w] if w == "/quit" => return Ok(()),
            [w] if w == "/help" => {
                for help in HELP {
                    out.line(help)?;
                }
            }
            [w] if is_command(w) => {
                text::error(err, &format!("Unknown command '{w}'."), Some("Try /help."))?
            }
            _ => convert(words.into_iter().map(PathBuf::from).collect(), out, err)?,
        }
        writeln!(out.w)?;
    }
}

/// A word like `/x` that isn't an existing path.
fn is_command(word: &str) -> bool {
    word.strip_prefix('/')
        .is_some_and(|name| name.chars().all(|c| c.is_ascii_alphabetic()))
        && !Path::new(word).exists()
}

fn banner(out: &mut Out<impl Write>) -> io::Result<()> {
    let look = out.look;
    let version = env!("CARGO_PKG_VERSION");
    let name = format!("{} image-convert", look.pick("✻", "*"));
    let rows = [
        (
            format!("{name} {version}"),
            format!("{} {version}", look.paint(BOLD, &name)),
        ),
        (format!("  {SENTENCE}"), format!("  {SENTENCE}")),
    ];
    let width = rows.iter().map(|(plain, _)| plain.chars().count()).max();
    let width = width.unwrap_or_default();
    let [top_left, top_right, bottom_left, bottom_right, across, side] = if look.ascii {
        ["+", "+", "+", "+", "-", "|"]
    } else {
        ["╭", "╮", "╰", "╯", "─", "│"]
    };
    let rule = across.repeat(width + 2);
    let side = look.paint(DIM, side);
    out.line(&look.paint(DIM, &format!("{top_left}{rule}{top_right}")))?;
    for (plain, styled) in rows {
        let pad = " ".repeat(width - plain.chars().count());
        out.line(&format!("{side} {styled}{pad} {side}"))?;
    }
    out.line(&look.paint(DIM, &format!("{bottom_left}{rule}{bottom_right}")))?;
    let dot = look.pick("·", "-");
    out.line(&format!(
        "  Drop files here or type paths {dot} /help {dot} /quit"
    ))?;
    out.line("")
}

/// The words of a line, split on spaces outside quotes. With `posix` as a POSIX shell reads a
/// dropped file (backslash escapes, single and double quotes), otherwise as Windows quotes
/// paths (double quotes only, backslashes kept).
fn split(line: &str, posix: bool) -> Vec<String> {
    let (mut words, mut word, mut quote, mut started) = (vec![], String::new(), None, false);
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        match (quote, c) {
            (None, c) if c.is_ascii_whitespace() => {
                if mem::take(&mut started) {
                    words.push(mem::take(&mut word));
                }
                continue;
            }
            (None, '\\') if posix => word.extend(chars.next()),
            (None, '"' | '\'') if posix || c == '"' => quote = Some(c),
            (Some(q), c) if c == q => quote = None,
            (Some('"'), '\\') if posix => match chars.next() {
                Some(escaped @ ('"' | '\\' | '$' | '`')) => word.push(escaped),
                other => {
                    word.push('\\');
                    word.extend(other);
                }
            },
            _ => word.push(c),
        }
        started = true;
    }
    if started {
        words.push(word);
    }
    words
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::text::Look;

    #[test]
    fn posix_lines_split_like_a_shell() {
        let split = |line| split(line, true);
        assert_eq!(
            split("/tmp/a\\ b.heic  c.png\n"),
            ["/tmp/a b.heic", "c.png"]
        );
        assert_eq!(split("'/x y/it''s.svg'"), ["/x y/its.svg"]);
        assert_eq!(split(r#""a \"b\" \\c\d" e"#), [r#"a "b" \c\d"#, "e"]);
        assert_eq!(split(r#"'it'\''s' "#), ["it's"]);
        assert!(split(" \t\n").is_empty());
    }

    #[test]
    fn windows_lines_split_on_spaces_outside_double_quotes() {
        let split = |line| split(line, false);
        assert_eq!(
            split("\"C:\\a b\\c.heic\" d.png\r\n"),
            ["C:\\a b\\c.heic", "d.png"]
        );
        assert_eq!(split("C:\\x\\it's.png"), ["C:\\x\\it's.png"]);
        assert_eq!(split("dir\\\"a b\"\\c.png"), ["dir\\a b\\c.png"]);
    }

    /// Runs interactive mode on `typed`, recording what `convert` was given.
    fn session(typed: &str, look: Look) -> (String, String) {
        let (mut out, mut err) = (Out::new(vec![], look), Out::new(vec![], look));
        run(typed.as_bytes(), &mut out, &mut err, |paths, out, _| {
            out.line(&format!("convert {paths:?}"))
        })
        .unwrap();
        let text = |w: Vec<u8>| String::from_utf8(w).unwrap();
        (text(out.w), text(err.w))
    }

    #[test]
    fn a_session_converts_paths_until_quit() {
        let typed = if cfg!(windows) {
            "\"a b.heic\" c.svg"
        } else {
            "a\\ b.heic c.svg"
        };
        let (out, err) = session(
            &format!("{typed}\n\n/help\n/x\n/quit\nlater.png\n"),
            Look::default(),
        );
        let banner = [
            "╭───────────────────────────────────────────────────────────────────╮",
            "│ ✻ image-convert 0.1.0                                             │",
            "│   Convert images agents and apps can't open to PNG, JPEG or WebP. │",
            "╰───────────────────────────────────────────────────────────────────╯",
            "  Drop files here or type paths · /help · /quit",
            "",
            "›   convert [\"a b.heic\", \"c.svg\"]",
            "",
            "› ›   Type or drop image paths to convert them to PNG or JPEG beside the original.",
            "  Quote a path with spaces. A folder converts every image in it into <folder>/converted/.",
            "  For other formats and sizes, run: image-convert --help",
            "  /help   show this",
            "  /quit   leave (Ctrl+C works too)",
            "",
            "› ",
            "› ",
        ];
        assert_eq!(out, banner.join("\n"));
        assert_eq!(err, "  ✗ Unknown command '/x'.\n    Try /help.\n");
    }

    #[test]
    fn the_end_of_input_leaves_and_an_ascii_console_gets_ascii() {
        let look = Look {
            ascii: true,
            ..Look::default()
        };
        let (out, _) = session("", look);
        assert!(out.starts_with("+------"));
        assert!(out.contains("| * image-convert 0.1.0 "));
        assert!(out.ends_with("Drop files here or type paths - /help - /quit\n\n> \n"));
    }
}
