//! The binary end to end: inputs and output paths, every file and usage error, exit codes, JSON
//! and --debug.

mod common;

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use common::*;
use image_convert::detect::Kind;
use image_convert::os;
use serde_json::Value;

struct Run {
    code: i32,
    stdout: String,
    stderr: String,
    bytes: Vec<u8>,
}

impl Run {
    fn json(&self) -> Value {
        serde_json::from_str(&self.stdout).expect(&self.stdout)
    }
}

/// Runs the binary in `dir` with `args`, feeding it `stdin` (empty when `None`).
fn run(dir: &Path, args: &[&str], stdin: Option<&[u8]>) -> Run {
    let mut child = Command::new(env!("CARGO_BIN_EXE_image-convert"))
        .args(args)
        .current_dir(dir)
        .env_remove("NO_COLOR")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    input.write_all(stdin.unwrap_or_default()).unwrap();
    drop(input);
    let out = child.wait_with_output().unwrap();
    Run {
        code: out.status.code().unwrap(),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8(out.stderr).unwrap(),
        bytes: out.stdout,
    }
}

/// A fresh, empty folder for one test.
fn folder(test: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("cli-{test}"));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// A 16 × 12 RGB gradient PNG.
fn small_png() -> Vec<u8> {
    let rgb = gradient(16, 12, 3);
    png(
        16,
        12,
        png::ColorType::Rgb,
        png::BitDepth::Eight,
        &rgb,
        |_| {},
    )
}

/// Files in `dir` and below, as sorted relative paths with `/`.
fn listing(dir: &Path) -> Vec<String> {
    let mut found = vec![];
    let mut open = vec![dir.to_path_buf()];
    while let Some(d) = open.pop() {
        for e in fs::read_dir(d).unwrap() {
            let path = e.unwrap().path();
            if path.is_dir() {
                open.push(path);
            } else {
                let rel = path.strip_prefix(dir).unwrap().to_string_lossy();
                found.push(rel.replace('\\', "/"));
            }
        }
    }
    found.sort();
    found
}

#[test]
fn prints_the_version_and_a_help_with_formats_presets_and_examples() {
    let dir = folder("help");
    let version = run(&dir, &["--version"], None);
    assert_eq!(
        (version.code, version.stdout.as_str()),
        (0, "image-convert 0.1.0\n")
    );
    assert_eq!(run(&dir, &["-V"], None).stdout, version.stdout);
    let help = run(&dir, &["--help"], None);
    assert_eq!(help.code, 0);
    for part in [
        "Convert HEIC, SVG, RAW and other images to PNG, JPEG or WebP.\n\nUsage: image-convert [COMMAND] [PATH]... [OPTIONS]\n",
        "  formats  Show which formats this machine can decode\n",
        "      --max-megapixels <X>  width × height in millions, 0.01-200\n",
        "  raw    CR2, CR3, NEF, ARW, DNG, RAF, RW2, PEF: the camera's embedded preview\n",
        "  vision-hd  longest side 2048, 2.4 megapixels, 7,500,000 bytes\n",
        "  cat photo.heic | image-convert - -o photo.jpg\n  image-convert formats\n",
    ] {
        assert!(help.stdout.contains(part), "{part}\n{}", help.stdout);
    }
    assert_eq!(run(&dir, &["-h"], None).stdout, help.stdout);
    let formats = run(&dir, &["formats", "--help"], None);
    assert!(formats.stdout.contains("--json"));
    assert!(formats.stdout.ends_with("  image-convert formats\n"));
}

/// One file error case: the files to make, the arguments, and the expected code, message and
/// hint (a message ending in a space only has to start with it and end in a full stop).
struct Case {
    files: Vec<(&'static str, Vec<u8>)>,
    args: Vec<String>,
    code: &'static str,
    message: String,
    hint: Option<&'static str>,
}

fn case(name: &'static str, bytes: Vec<u8>, code: &'static str, message: &str) -> Case {
    Case {
        files: vec![(name, bytes)],
        args: vec![name.into()],
        code,
        message: format!("{name}: {message}"),
        hint: None,
    }
}

impl Case {
    fn hint(self, hint: &'static str) -> Self {
        Self {
            hint: Some(hint),
            ..self
        }
    }

    fn args(self, args: &[&str]) -> Self {
        let args = self
            .args
            .into_iter()
            .chain(args.iter().map(|a| a.to_string()));
        Self {
            args: args.collect(),
            ..self
        }
    }
}

fn matches(message: &str, expected: &str) -> bool {
    if expected.ends_with(' ') {
        message.starts_with(expected) && message.ends_with('.')
    } else {
        message == expected
    }
}

#[test]
fn every_file_error_has_its_message_and_hint_in_text_and_json() {
    let dir = folder("errors");
    let damaged = small_png()[..60].to_vec();
    let mut huge = small_png();
    huge[16..24].copy_from_slice(&[0, 1, 0x86, 0xa0, 0, 1, 0x86, 0xa0]);
    let noise: Vec<u8> = (0..64 * 64 * 3)
        .scan(0x9E37_79B9u32, |s, _: u32| {
            *s ^= *s << 13;
            *s ^= *s >> 17;
            *s ^= *s << 5;
            Some(*s as u8)
        })
        .collect();
    let noise = png(
        64,
        64,
        png::ColorType::Rgb,
        png::BitDepth::Eight,
        &noise,
        |_| {},
    );
    let no_preview = raw_tiff(42, &[], &[(0x100, 4, 1, V::N(8))], &[]);
    let svg = br#"<svg xmlns="http://www.w3.org/2000/svg" width="10" height="10"/>"#.to_vec();
    let supported = "Supported: PNG, JPEG, GIF, BMP, ICO, WebP, TIFF, JPEG XL, HEIC/HEIF, AVIF, SVG, EMF/WMF (Windows), and RAW from Canon (CR2, CR3), Nikon (NEF), Sony (ARW), Adobe DNG, Fujifilm (RAF), Panasonic (RW2) and Pentax (PEF).";
    let damaged_hint = "The file may be damaged or cut short.";
    let mut cases = vec![
        Case {
            files: vec![],
            args: vec!["gone.png".into()],
            code: "not_found",
            message: "gone.png: File not found: gone.png".into(),
            hint: Some("Check the path."),
        },
        Case {
            files: vec![],
            args: vec!["*.heic".into()],
            code: "wildcard",
            message: "*.heic: No file named '*.heic'.".into(),
            hint: Some("Windows shells don't expand * or ?; pass the folder instead, e.g. image-convert photos"),
        },
        case("empty.png", vec![], "empty_file", "The file is empty."),
        case("notes.psd", b"8BPS\0\x01".to_vec(), "unsupported_format", "PSD files aren't supported.").hint(supported),
        case("x.orf", b"IIRO\x08\0\0\0".to_vec(), "unsupported_format", "Olympus ORF RAW files aren't supported yet.")
            .hint("Export a JPEG from the camera's software, or use a RAW editor (darktable, RawTherapee)."),
        case("a.jpg", fixture("jpeg-arithmetic.jpg"), "unsupported_variant", "JPEG with arithmetic coding isn't supported.")
            .hint("Convert it to PNG or JPEG in the app that made it."),
        case("huge.png", huge, "image_too_large", "The image is 100000×100000 pixels; the limit is 200 megapixels and 65,535 pixels per side.")
            .hint("Crop it first."),
        case("wide.svg", svg, "image_too_large", "The image is 20000×20000 pixels; the limit is 200 megapixels and 65,535 pixels per side.")
            .hint("Use --width or --height to render it smaller.")
            .args(&["--width", "20000"]),
        case("cut.png", damaged, "corrupt_image", "Can't decode this PNG file: ").hint(damaged_hint),
        case("d.nef", no_preview, "raw_no_preview", "This Nikon NEF RAW file has no embedded preview image.")
            .hint("Export a JPEG from a RAW editor (darktable, RawTherapee, Lightroom)."),
        case("noise.png", noise, "cannot_fit", "Can't make this image smaller than 1000 bytes; the smallest try was ")
            .hint("Raise --max-bytes, or use --to jpeg or --to webp.")
            .args(&["--max-bytes", "1KB", "--to", "png"]),
        Case {
            files: vec![("taken.png", small_png()), ("in.png", small_png())],
            args: vec!["in.png".into(), "-o".into(), "taken.png".into()],
            code: "output_exists",
            message: "in.png: taken.png already exists.".into(),
            hint: Some("Pass --overwrite to replace it, or choose another -o."),
        },
        case("same.png", small_png(), "output_is_input", "The output would replace the input file.")
            .hint("Choose another -o.")
            .args(&["-o", "./same.png", "--overwrite"]),
        case("w.png", small_png(), "write_failed", "Can't write w.png/out.png: ").args(&["-o", "w.png/out.png"]),
    ];
    if !cfg!(windows) {
        cases.push(
            case(
                "a.emf",
                plain_emf(),
                "unsupported_on_platform",
                "EMF and WMF files can only be converted on Windows for now.",
            )
            .hint("Convert it on Windows, or export it as SVG or PNG from the app that made it."),
        );
    }
    if let Err(missing) = os::heif_available(Kind::Heic) {
        let mut heic = case(
            "p.heic",
            fixture("heif-irot.heic"),
            "codec_missing",
            &missing.message,
        );
        heic.hint = missing.hint;
        cases.push(heic);
    }
    #[cfg(windows)]
    let _held = {
        use std::os::windows::fs::OpenOptionsExt;
        fs::write(dir.join("locked.png"), small_png()).unwrap();
        let held = fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(dir.join("locked.png"));
        cases.push(case(
            "locked.png",
            vec![],
            "unreadable",
            "Can't read locked.png: ",
        ));
        cases.last_mut().unwrap().files.clear();
        held.unwrap()
    };
    #[cfg(target_os = "linux")]
    cases.push(Case {
        files: vec![],
        args: vec!["/proc/self/mem".into()],
        code: "unreadable",
        message: "/proc/self/mem: Can't read /proc/self/mem: ".into(),
        hint: None,
    });

    for c in cases {
        for (name, bytes) in &c.files {
            fs::write(dir.join(name), bytes).unwrap();
        }
        let args: Vec<&str> = c.args.iter().map(String::as_str).collect();
        let text = run(&dir, &args, None);
        assert_eq!(
            (text.code, text.stdout.as_str()),
            (1, ""),
            "{}: {}",
            c.code,
            text.stderr
        );
        let mut lines = text.stderr.lines();
        let first = lines.next().unwrap();
        assert!(
            matches(first.strip_prefix("✗ ").unwrap(), &c.message),
            "{first}"
        );
        assert_eq!(
            lines.next(),
            c.hint.map(|h| format!("  {h}")).as_deref(),
            "{}",
            c.code
        );
        assert_eq!(lines.next(), None, "{}", text.stderr);

        let json = run(&dir, &[&args[..], &["--json"]].concat(), None);
        assert_eq!((json.code, json.stderr.as_str()), (1, ""));
        let report = json.json();
        let file = &report["files"][0];
        let error = &file["error"];
        assert_eq!(error["code"], c.code);
        let message = c.message.split_once(": ").unwrap().1;
        assert!(
            matches(error["message"].as_str().unwrap(), message),
            "{error}"
        );
        assert_eq!(error["hint"].as_str(), c.hint);
        for key in [
            "output", "format", "width", "height", "bytes", "quality", "resized", "exif",
        ] {
            assert_eq!(file[key], Value::Null, "{key}");
        }
        assert_eq!(
            (&report["converted"], &report["failed"]),
            (&0.into(), &1.into())
        );
    }
    // Nothing half-written is left behind.
    assert!(
        listing(&dir).iter().all(|f| !f.contains(".tmp")),
        "{:?}",
        listing(&dir)
    );
}

#[test]
fn a_file_over_512_mib_is_refused_from_its_size() {
    let dir = folder("large");
    let file = fs::File::create(dir.join("huge.png")).unwrap();
    file.set_len((512 << 20) + 1).unwrap();
    drop(file);
    let run = run(&dir, &["huge.png"], None);
    assert_eq!(run.code, 1);
    assert_eq!(
        run.stderr,
        "✗ huge.png: The file is larger than 512 MiB.\n  Shrink or split it first.\n"
    );
}

#[test]
fn usage_errors_exit_2_as_two_lines_or_a_json_object() {
    let dir = folder("usage");
    fs::write(dir.join("a.png"), small_png()).unwrap();
    fs::write(dir.join("b.png"), small_png()).unwrap();
    let cases: [(&[&str], &str, &str, Option<&str>); 16] = [
        (
            &["a.png", "--to", "gif"],
            "bad_format",
            "Unknown output format 'gif'.",
            Some("Use auto, png, jpeg or webp."),
        ),
        (
            &["a.png", "--fit", "huge"],
            "bad_fit",
            "Unknown preset 'huge'.",
            Some("Use none, vision or vision-hd."),
        ),
        (
            &["a.png", "--max-side", "0"],
            "bad_size",
            "--max-side must be between 1 and 65535.",
            None,
        ),
        (
            &["a.png", "--width", "70000"],
            "bad_size",
            "--width must be between 1 and 65535.",
            None,
        ),
        (
            &["a.png", "--height", "x"],
            "bad_size",
            "--height must be between 1 and 65535.",
            None,
        ),
        (
            &["a.png", "--max-megapixels", "0"],
            "bad_megapixels",
            "--max-megapixels must be between 0.01 and 200.",
            None,
        ),
        (
            &["a.png", "--max-bytes", "5GB"],
            "bad_bytes",
            "Can't read the byte size '5GB'.",
            Some("Use a number with an optional KB, MB, KiB or MiB, e.g. 500KB."),
        ),
        (
            &["a.png", "--quality", "101"],
            "bad_quality",
            "--quality must be between 1 and 100.",
            Some("Try --quality 85."),
        ),
        (
            &["a.png", "--background", "red"],
            "bad_color",
            "Can't read the colour 'red'.",
            Some("Use white, black or #rrggbb."),
        ),
        (
            &["a.png", "-o", "out.gif"],
            "bad_extension",
            "Can't tell the format from 'out.gif'.",
            Some("End -o with .png, .jpg or .webp, or pass --to."),
        ),
        (
            &["a.png", "--to", "png", "-o", "out.JPG"],
            "format_conflict",
            "--to png doesn't match the -o extension .jpg.",
            None,
        ),
        (
            &["a.png", "b.png", "-o", "out.png"],
            "output_needs_one_input",
            "-o names one file, but 2 inputs were given.",
            Some("Pass a folder to -o instead."),
        ),
        (
            &["-"],
            "stdin_needs_output",
            "Reading from stdin needs -o.",
            Some("Try: image-convert - -o out.jpg"),
        ),
        (
            &["a.png", "-o", "-", "--json"],
            "json_with_stdout",
            "--json can't be used with -o -.",
            None,
        ),
        (
            &["--fit", "vision"],
            "no_input",
            "No file given.",
            Some("Try: image-convert photo.heic"),
        ),
        (
            &[],
            "no_input",
            "No file given.",
            Some("Try: image-convert photo.heic"),
        ),
    ];
    for (args, code, message, hint) in cases {
        // `-o - --json` is itself the error, so it only has a JSON form.
        if code != "json_with_stdout" {
            let text = run(&dir, args, None);
            assert_eq!((text.code, text.stdout.as_str()), (2, ""), "{args:?}");
            let expected = format!(
                "✗ {message}\n{}",
                hint.map(|h| format!("  {h}\n")).unwrap_or_default()
            );
            assert_eq!(text.stderr, expected, "{args:?}");
        }
        let json = [args, &["--json"][usize::from(args.contains(&"--json"))..]].concat();
        let json = run(&dir, &json, None);
        assert_eq!((json.code, json.stderr.as_str()), (2, ""), "{args:?}");
        let expected =
            serde_json::json!({"error": {"code": code, "message": message, "hint": hint}});
        assert_eq!(json.stdout, format!("{expected}\n"));
    }
    let unknown = run(&dir, &["a.png", "--bogus", "--json"], None);
    assert_eq!(unknown.code, 2);
    assert_eq!(
        unknown.stdout,
        "{\"error\":{\"code\":\"usage\",\"message\":\"Unexpected argument '--bogus' found.\",\"hint\":\"Try: image-convert --help\"}}\n"
    );
    let text = run(&dir, &["a.png", "--bogus"], None);
    assert_eq!(text.code, 2);
    assert!(text.stderr.contains("'--bogus'") && text.stderr.contains("--help"));
    assert_eq!(
        listing(&dir),
        ["a.png", "b.png"],
        "a usage error writes nothing"
    );
}

#[test]
fn outputs_go_beside_into_converted_or_where_o_says() {
    let dir = folder("paths");
    for name in [
        "photos/b.png",
        "photos/a.PNG",
        "photos/.hidden.png",
        "photos/notes.txt",
        "photos/trip/c.png",
        "photos/out/old.png",
        "one.png",
    ] {
        fs::create_dir_all(dir.join(name).parent().unwrap()).unwrap();
        fs::write(dir.join(name), small_png()).unwrap();
    }
    let ok = |args: &[&str]| {
        let r = run(&dir, args, None);
        assert_eq!(r.code, 0, "{args:?}: {}", r.stderr);
        r
    };
    let outputs = |r: &Run| -> Vec<String> {
        let json = r.json();
        let files = json["files"].as_array().unwrap().clone();
        files
            .iter()
            .map(|f| {
                format!(
                    "{} -> {}",
                    f["input"].as_str().unwrap(),
                    f["output"].as_str().unwrap()
                )
                .replace('\\', "/")
            })
            .collect()
    };
    assert_eq!(
        outputs(&ok(&["one.png", "--to", "jpeg", "--json"])),
        ["one.png -> one.jpg"]
    );
    // The input itself and earlier outputs are never replaced.
    assert_eq!(
        outputs(&ok(&["one.png", "one.png", "--json"])),
        ["one.png -> one-1.png", "one.png -> one-2.png"]
    );
    assert_eq!(
        outputs(&ok(&["photos", "-r", "--to", "webp", "--json"])),
        [
            "photos/a.PNG -> photos/converted/a.webp",
            "photos/b.png -> photos/converted/b.webp",
            "photos/out/old.png -> photos/converted/out/old.webp",
            "photos/trip/c.png -> photos/converted/trip/c.webp"
        ]
    );
    // The folder's own converted/ and an -o folder inside it are skipped.
    assert_eq!(
        outputs(&ok(&[
            "photos",
            "-r",
            "--to",
            "webp",
            "-o",
            "photos/out/",
            "--json"
        ])),
        [
            "photos/a.PNG -> photos/out/a.webp",
            "photos/b.png -> photos/out/b.webp",
            "photos/trip/c.png -> photos/out/trip/c.webp"
        ]
    );
    assert_eq!(
        outputs(&ok(&[
            "photos/trip/c.png",
            "one.png",
            "-o",
            "flat/",
            "--to",
            "jpeg",
            "--json"
        ]))[0],
        "photos/trip/c.png -> flat/c.jpg",
        "an existing folder or one ending in a slash"
    );
    assert_eq!(
        outputs(&ok(&["one.png", "-o", "deep/x.webp", "--json"])),
        ["one.png -> deep/x.webp"]
    );
    let again = run(&dir, &["one.png", "-o", "deep/x.webp"], None);
    assert_eq!(again.code, 1);
    assert_eq!(
        outputs(&ok(&[
            "one.png",
            "-o",
            "deep/x.webp",
            "--overwrite",
            "--json"
        ])),
        ["one.png -> deep/x.webp"]
    );
    assert!(
        fs::read(dir.join("deep/x.webp"))
            .unwrap()
            .starts_with(b"RIFF")
    );

    let stdout = run(&dir, &["one.png", "-o", "-", "--to", "jpeg"], None);
    assert_eq!(stdout.code, 0);
    assert!(stdout.bytes.starts_with(b"\xff\xd8\xff"));
    assert_eq!(
        stdout.stderr,
        format!("✓ one.png → -  16×12 JPEG  {} bytes\n", stdout.bytes.len())
    );
    let piped = run(
        &dir,
        &["-", "-o", "piped.png"],
        Some(&fs::read(dir.join("one.png")).unwrap()),
    );
    let bytes = fs::metadata(dir.join("piped.png")).unwrap().len();
    assert_eq!(piped.code, 0);
    assert_eq!(
        piped.stdout,
        format!("✓ - → piped.png  16×12 PNG  {bytes} bytes\n")
    );
}

#[test]
fn text_lines_name_each_file_its_size_warnings_and_a_summary() {
    let dir = folder("text");
    let rgba = gradient(40, 20, 4);
    let alpha = png(
        40,
        20,
        png::ColorType::Rgba,
        png::BitDepth::Eight,
        &rgba,
        |_| {},
    );
    fs::write(dir.join("a.png"), &alpha).unwrap();
    let run1 = run(
        &dir,
        &["a.png", "gone.png", "--to", "jpeg", "--max-side", "10"],
        None,
    );
    assert_eq!(run1.code, 1);
    let bytes = fs::metadata(dir.join("a.jpg")).unwrap().len();
    assert_eq!(
        run1.stdout,
        format!("✓ a.png → a.jpg  10×5 JPEG  {bytes} bytes · from 40×20\n1 converted · 1 failed\n")
    );
    assert_eq!(
        run1.stderr,
        "! a.png: JPEG has no transparency, so transparent areas were filled with white.\n✗ gone.png: File not found: gone.png\n  Check the path.\n"
    );
}

#[test]
fn exit_codes_follow_what_converted_and_what_failed() {
    let dir = folder("exit");
    fs::write(dir.join("a.png"), small_png()).unwrap();
    assert_eq!(run(&dir, &["a.png"], None).code, 0);
    assert_eq!(run(&dir, &["a.png", "missing.png"], None).code, 1);
    assert_eq!(run(&dir, &["a.png", "--quality", "0"], None).code, 2);
    let json = run(&dir, &["a.png", "a.png", "--json"], None).json();
    assert_eq!(
        (&json["converted"], &json["failed"]),
        (&2.into(), &0.into())
    );
}

#[test]
fn debug_shows_the_decoder_and_steps_on_stderr_only() {
    let dir = folder("debug");
    fs::write(dir.join("a.png"), small_png()).unwrap();
    let plain = run(&dir, &["a.png", "--json", "-o", "x.png"], None);
    let debug = run(&dir, &["a.png", "--json", "--debug", "-o", "y.png"], None);
    assert_eq!(debug.stdout, plain.stdout.replace("x.png", "y.png"));
    assert_eq!(plain.stderr, "");
    assert!(
        debug
            .stderr
            .starts_with("debug: a.png: png, decoder builtin: read "),
        "{}",
        debug.stderr
    );
    for step in [", decode ", ", convert ", ", write "] {
        assert!(debug.stderr.contains(step), "{}", debug.stderr);
    }
    let missing = run(&dir, &["gone.png", "--debug"], None);
    assert!(
        missing.stderr.starts_with(
            "debug: gone.png: unknown type, decoder none\ndebug: gone.png: error not_found\n"
        ),
        "{}",
        missing.stderr
    );
}

#[test]
fn formats_lists_every_matrix_row_in_text_and_json() {
    let dir = folder("formats");
    let rows = os::formats();
    let json = run(&dir, &["formats", "--json"], None);
    assert_eq!(json.code, 0);
    let listed = json.json()["formats"].as_array().unwrap().clone();
    assert_eq!(listed.len(), rows.len());
    for (row, listed) in rows.iter().zip(&listed) {
        assert_eq!(listed["format"], row.format.name());
        assert_eq!(listed["status"], row.status());
        assert_eq!(
            listed["message"].as_str(),
            row.missing.as_ref().map(|e| e.message.as_str())
        );
    }
    let text = run(&dir, &["formats"], None);
    let lines: Vec<_> = text.stdout.lines().collect();
    assert_eq!(lines[0], "png   available  builtin");
    assert_eq!(lines.len(), rows.len());
    let emf = if cfg!(windows) {
        "emf   available  gdiplus"
    } else {
        "emf   windows only  EMF and WMF files can only be converted on Windows for now."
    };
    assert_eq!(lines[11], emf);
}

#[cfg(unix)]
#[test]
fn a_path_that_is_not_utf8_prints_lossily() {
    use std::os::unix::ffi::OsStrExt;
    let dir = folder("lossy");
    let name = std::ffi::OsStr::from_bytes(b"caf\xE9.png");
    fs::write(dir.join(name), small_png()).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_image-convert"))
        .args([name, "--json".as_ref()])
        .current_dir(&dir)
        .output()
        .unwrap();
    let report: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(report["files"][0]["input"], "caf\u{FFFD}.png");
    assert_eq!(report["files"][0]["output"], "caf\u{FFFD}-1.png");
    assert!(
        dir.join(std::ffi::OsStr::from_bytes(b"caf\xE9-1.png"))
            .exists()
    );
}

#[test]
fn a_closed_stdout_or_stderr_ends_the_run_quietly_with_0() {
    let dir = folder("pipes");
    fs::write(dir.join("a.png"), small_png()).unwrap();
    let rgb = gradient(1500, 1000, 3);
    let big = png(
        1500,
        1000,
        png::ColorType::Rgb,
        png::BitDepth::Eight,
        &rgb,
        |_| {},
    );
    fs::write(dir.join("big.png"), big).unwrap();
    let cases: [(&[&str], bool); 10] = [
        (&["a.png", "a.png"], true),
        (&["a.png", "--json"], true),
        (&["a.png", "--debug"], true),
        (&["--help"], true),
        (&["formats"], true),
        (&["big.png", "-o", "-"], true),
        (&["a.png", "--debug"], false),
        (&["missing.png", "a.png"], false),
        (&["a.png", "-o", "-"], false),
        (&["a.png", "--quality", "0"], false),
    ];
    for (args, close_stdout) in cases {
        // The read end closes before the binary starts, so its first write fails.
        let (reader, writer) = std::io::pipe().unwrap();
        drop(reader);
        let mut command = Command::new(env!("CARGO_BIN_EXE_image-convert"));
        command.args(args).current_dir(&dir).stdin(Stdio::null());
        if close_stdout {
            command.stdout(writer).stderr(Stdio::piped());
        } else {
            command.stderr(writer).stdout(Stdio::piped());
        }
        let out = command.output().unwrap();
        let other = if close_stdout { out.stderr } else { out.stdout };
        let other = String::from_utf8_lossy(&other);
        let code = out.status.code();
        // A usage error still exits 2; only the hung-up stream's output is lost.
        let expected = if args.contains(&"--quality") { 2 } else { 0 };
        assert_eq!(code, Some(expected), "{args:?}: {other}");
        assert!(
            !other.contains("Can't write") && !other.contains("panicked"),
            "{args:?}: {other}"
        );
    }
}

#[test]
fn control_and_bidi_characters_in_names_never_reach_the_terminal() {
    let dir = folder("escape");
    let name = if cfg!(windows) {
        "\u{202e}gnp.exe.png"
    } else {
        "\x1b[31mred\u{202e}.png"
    };
    fs::write(dir.join(name), small_png()).unwrap();
    let text = run(&dir, &[name, "--to", "webp"], None);
    assert_eq!(text.code, 0, "{}", text.stderr);
    assert!(
        !text.stdout.contains(['\x1b', '\u{202e}']),
        "{}",
        text.stdout
    );
    let shown = if cfg!(windows) {
        "<U+202E>gnp.exe.png → <U+202E>gnp.exe.webp"
    } else {
        "<U+001B>[31mred<U+202E>.png → <U+001B>[31mred<U+202E>.webp"
    };
    assert!(text.stdout.contains(shown), "{}", text.stdout);
    let missing = run(&dir, &[&format!("gone{name}")], None);
    assert!(
        !missing.stderr.contains(['\x1b', '\u{202e}']),
        "{}",
        missing.stderr
    );
    let json = run(&dir, &[name, "--to", "webp", "--json"], None);
    assert_eq!(
        json.json()["files"][0]["input"],
        name,
        "JSON keeps the name as it is"
    );
}
