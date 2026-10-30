//! The local page: the HTML works offline, and `image-convert ui` converts uploads over HTTP on
//! 127.0.0.1 exactly as the CLI converts files.

mod common;

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use common::*;
use serde_json::{Value, json};

const PAGE: &str = include_str!("../src/ui.html");
const BIN: &str = env!("CARGO_BIN_EXE_image-convert");

#[test]
fn page_loads_nothing_from_another_host() {
    let footer = [
        "https://nowmyagentcan.com",
        "https://github.com/nowmyagentcan/image-convert",
        "https://x.com/chayp05",
    ];
    for pat in ["src=", "href=", "url(", "@import", "fetch(", "action="] {
        for (i, _) in PAGE.match_indices(pat) {
            let rest = PAGE[i + pat.len()..].trim_start_matches(['"', '\'', ' ']);
            let target = &rest[..rest.find(['"', '\'', ' ', ')', '>']).unwrap_or(rest.len())];
            assert!(
                target.starts_with("data:") || !target.contains("://") || footer.contains(&target),
                "{pat} loads {target}"
            );
        }
    }
    assert!(PAGE.contains(r#"fetch("/api/convert""#));
}

/// A folder for one test holding `photo.png` (a 300 × 200 gradient) and `logo.svg`.
fn folder(test: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("ui-{test}"));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    let rgb = gradient(300, 200, 3);
    let photo = png(
        300,
        200,
        png::ColorType::Rgb,
        png::BitDepth::Eight,
        &rgb,
        |_| {},
    );
    fs::write(dir.join("photo.png"), photo).unwrap();
    let svg = r##"<svg xmlns="http://www.w3.org/2000/svg" width="64" height="32"><rect width="40" height="20" fill="#7c3aed"/></svg>"##;
    fs::write(dir.join("logo.svg"), svg).unwrap();
    dir
}

/// The CLI's `--json` output for `args`, run from `dir`.
fn cli(dir: &Path, args: &[&str]) -> Value {
    let out = Command::new(BIN)
        .args(args)
        .arg("--json")
        .current_dir(dir)
        .output()
        .unwrap();
    serde_json::from_slice(&out.stdout).unwrap()
}

/// Percent-decodes `s` as `decodeURIComponent` does.
fn unescape(s: &str) -> String {
    let mut bytes = vec![];
    let mut rest = s.as_bytes();
    while let Some((&b, tail)) = rest.split_first() {
        if b == b'%' {
            bytes.push(u8::from_str_radix(std::str::from_utf8(&tail[..2]).unwrap(), 16).unwrap());
            rest = &tail[2..];
        } else {
            bytes.push(b);
            rest = tail;
        }
    }
    String::from_utf8(bytes).unwrap()
}

/// `image-convert ui` on a free port, stopped when dropped.
struct Server {
    child: Child,
    port: u16,
}

/// A response: status, headers and body.
type Answer = (u16, String, Vec<u8>);

impl Server {
    fn start(args: &[&str]) -> Server {
        let mut child = Command::new(BIN)
            .arg("ui")
            .args(args)
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut line = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        let port = line
            .strip_prefix("Open http://127.0.0.1:")
            .and_then(|rest| rest.strip_suffix("  (Ctrl+C to stop)\n"))
            .unwrap_or_else(|| panic!("{line:?}"))
            .parse()
            .unwrap();
        Server { child, port }
    }

    /// Sends `head` (request line and headers, `{port}` filled in) and `body`.
    fn send(&self, head: &str, body: &[u8]) -> Answer {
        let mut stream = TcpStream::connect(("127.0.0.1", self.port)).unwrap();
        let head = head.replace("{port}", &self.port.to_string());
        stream
            .write_all(format!("{head}\r\n\r\n").as_bytes())
            .unwrap();
        // A server that answers before reading the whole body may close early.
        let _ = stream.write_all(body);
        let _ = stream.shutdown(Shutdown::Write);
        let mut response = vec![];
        stream.read_to_end(&mut response).unwrap();
        let end = response.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
        let headers = String::from_utf8(response[..end].to_vec()).unwrap();
        let status = headers[9..12].parse().unwrap();
        (status, headers, response[end + 4..].to_vec())
    }

    /// Uploads `body` as the page does: the status, the file object (from `X-Result` on 200)
    /// or error JSON, and the image.
    fn convert(&self, name: &str, options: &str, body: &[u8]) -> (u16, Value, Vec<u8>) {
        let head = format!(
            "POST /api/convert HTTP/1.1\r\nHost: 127.0.0.1:{{port}}\r\nOrigin: http://127.0.0.1:{{port}}\r\n\
             X-File-Name: {name}\r\nX-Options: {options}\r\nContent-Length: {}",
            body.len()
        );
        let (status, headers, body) = self.send(&head, body);
        let result = headers
            .lines()
            .find_map(|l| l.strip_prefix("X-Result: "))
            .map(unescape);
        match result {
            Some(file) => (status, serde_json::from_str(&file).unwrap(), body),
            None => (status, serde_json::from_slice(&body).unwrap(), vec![]),
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.child.kill().unwrap();
        self.child.wait().unwrap();
    }
}

#[test]
fn serves_the_page_and_converts_like_the_cli() {
    let dir = folder("converts");
    let server = Server::start(&["--no-open"]);
    let (status, headers, body) = server.send("GET / HTTP/1.1\r\nHost: 127.0.0.1:{port}", b"");
    assert_eq!((status, body.as_slice()), (200, PAGE.as_bytes()));
    assert!(headers.contains("Content-Type: text/html; charset=utf-8\r\n"));
    assert!(headers.contains("Content-Security-Policy: default-src 'none';"));
    for (name, options, args, mime) in [
        ("photo.png", "{}", &[][..], "png"),
        (
            "photo.png",
            r#"{"to":"jpeg","max_side":120,"quality":70}"#,
            &["--to", "jpeg", "--max-side", "120", "--quality", "70"][..],
            "jpeg",
        ),
        (
            "photo.png",
            r##"{"to":"webp","fit":"vision","background":"#102030","max_bytes":20000}"##,
            &[
                "--to",
                "webp",
                "--fit",
                "vision",
                "--background",
                "#102030",
                "--max-bytes",
                "20000",
            ],
            "webp",
        ),
        ("logo.svg", r#"{"width":256}"#, &["--width", "256"], "png"),
    ] {
        let mut want = cli(&dir, &[&[name][..], args].concat())["files"][0].clone();
        let written = fs::read(dir.join(want["output"].as_str().unwrap())).unwrap();
        let ext = if mime == "jpeg" { "jpg" } else { mime };
        let stem = name.split('.').next().unwrap();
        want["output"] = json!(format!("{stem}.{ext}"));
        let input = fs::read(dir.join(name)).unwrap();
        let (status, file, image) = server.convert(name, options, &input);
        assert_eq!((status, &file), (200, &want), "{options}");
        assert_eq!(image, written, "{options}");
        let head = format!(
            "POST /api/convert HTTP/1.1\r\nHost: localhost:{{port}}\r\nX-File-Name: {name}\r\nX-Options: {options}\r\nContent-Length: {}",
            input.len()
        );
        let (_, headers, _) = server.send(&head, &input);
        assert!(headers.contains(&format!("\r\nContent-Type: image/{mime}\r\n")));
    }
    let photo = fs::read(dir.join("photo.png")).unwrap();
    let (status, file, _) = server.convert("Gr%C3%BC%C3%9Fe%20%E6%9D%B1.png", "{}", &photo);
    assert_eq!(
        (status, &file["input"], &file["output"]),
        (200, &json!("Grüße 東.png"), &json!("Grüße 東.png"))
    );
    for (body, code) in [(&b""[..], "empty_file"), (b"hello", "unsupported_format")] {
        let (status, file, _) = server.convert("x.png", "{}", body);
        assert_eq!((status, &file["error"]["code"]), (422, &json!(code)));
        assert_eq!(file["output"], Value::Null);
    }
    for (options, args) in [
        (r#"{"quality":0}"#, &["--quality", "0"][..]),
        (r#"{"fit":"huge"}"#, &["--fit", "huge"]),
        (r#"{"background":"red"}"#, &["--background", "red"]),
    ] {
        let want = cli(&dir, &[&["photo.png"][..], args].concat());
        assert_eq!(server.convert("photo.png", options, &photo).0, 422);
        assert_eq!(server.convert("photo.png", options, &photo).1, want);
    }
    for (name, options) in [
        ("photo.png", "{nope"),
        ("photo.png", r#"{"colour":"red"}"#),
        ("%zz.png", "{}"),
        ("%C3.png", "{}"),
    ] {
        let (status, error, _) = server.convert(name, options, &photo);
        assert_eq!(
            (status, &error["error"]["code"]),
            (400, &json!("bad_request")),
            "{name} {options}"
        );
    }
    // Nothing was written beside the inputs but the CLI's own outputs.
    assert!(!dir.join("Grüße 東.png").exists());
}

#[test]
fn refuses_other_hosts_origins_routes_and_large_requests() {
    let server = Server::start(&["--no-open"]);
    let other = server.port.wrapping_add(1);
    for (head, status) in [
        ("GET / HTTP/1.1\r\nHost: localhost:{port}".to_owned(), 200),
        ("GET /?x=1 HTTP/1.1\r\nHost: LOCALHOST:{port}".into(), 200),
        ("GET / HTTP/1.1".into(), 403),
        ("GET / HTTP/1.1\r\nHost: evil.example".into(), 403),
        ("GET / HTTP/1.1\r\nHost: evil.example:{port}".into(), 403),
        (format!("GET / HTTP/1.1\r\nHost: 127.0.0.1:{other}"), 403),
        (
            "GET / HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nOrigin: http://evil.example".into(),
            403,
        ),
        (
            "GET / HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nOrigin: https://127.0.0.1:{port}".into(),
            403,
        ),
        (
            "GET / HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nOrigin: null".into(),
            403,
        ),
        (
            "POST /api/convert HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nOrigin: http://evil.example\r\nContent-Length: 0".into(),
            403,
        ),
        ("GET /api/convert HTTP/1.1\r\nHost: 127.0.0.1:{port}".into(), 404),
        ("GET /favicon.ico HTTP/1.1\r\nHost: 127.0.0.1:{port}".into(), 404),
        ("POST / HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Length: 0".into(), 404),
        ("POST /api HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Length: 0".into(), 404),
    ] {
        let (code, _, body) = server.send(&head, b"");
        assert_eq!(code, status, "{head}");
        if code != 200 {
            let body: Value = serde_json::from_slice(&body).unwrap();
            assert!(body["error"]["message"].is_string(), "{body}");
        }
    }
    // Refused from the length alone, before any of the body is read.
    let head = format!(
        "POST /api/convert HTTP/1.1\r\nHost: 127.0.0.1:{{port}}\r\nContent-Length: {}",
        (512u64 << 20) + 1
    );
    let (status, _, body) = server.send(&head, b"");
    assert_eq!(
        (status, String::from_utf8(body).unwrap().as_str()),
        (
            413,
            r#"{"error":{"code":"too_large","message":"The file is larger than 512 MiB.","hint":null}}"#
        )
    );
    let head = format!(
        "GET / HTTP/1.1\r\nHost: 127.0.0.1:{{port}}\r\nX-Pad: {}",
        "a".repeat(17 * 1024)
    );
    assert_eq!(server.send(&head, b"").0, 413);
}

#[test]
fn refuses_repeated_headers_framing_and_malformed_requests() {
    let dir = folder("malformed");
    let photo = fs::read(dir.join("photo.png")).unwrap();
    let server = Server::start(&["--no-open"]);
    let post = |headers: &str, length: usize| {
        let head = format!("POST /api/convert HTTP/1.1\r\n{headers}\r\nContent-Length: {length}");
        server.send(&head, &photo).0
    };
    let local = "Host: 127.0.0.1:{port}";
    assert_eq!(post(local, photo.len()), 200);
    for headers in [
        "Host: evil.example\r\nHost: 127.0.0.1:{port}",
        "Host: 127.0.0.1:{port}\r\nHost: 127.0.0.1:{port}",
        "Host: 127.0.0.1:{port}\r\nOrigin: http://127.0.0.1:{port}\r\nOrigin: http://127.0.0.1:{port}",
        "Host: 127.0.0.1:{port}\r\nContent-Length: 1",
        "Host: 127.0.0.1:{port}\r\nTransfer-Encoding: chunked",
        "Host: 127.0.0.1:{port}\r\nTransfer-Encoding: identity",
        "Host: 127.0.0.1:{port}\r\nX-File-Name: a.png\r\nX-File-Name: b.png",
        "Host: 127.0.0.1:{port}\r\nX-Options: {}\r\nX-Options: {}",
        "Host: 127.0.0.1:{port}\r\nOrigin : http://evil.example",
        "Host: 127.0.0.1:{port}\r\nBad Header: value",
        "Host: 127.0.0.1:{port}\r\nNoColon",
        "Host: 127.0.0.1:{port}\r\nX-File-Name: a\0b.png",
        "Host: 127.0.0.1:{port}\r\nX-File-Name: a\rb.png",
        "Host: 127.0.0.1:{port}\r\nX-Other: a\r ",
    ] {
        assert_eq!(post(headers, photo.len()), 400, "{headers:?}");
    }
    // A body shorter than its length.
    assert_eq!(post(local, photo.len() + 10), 400);
    let length = format!("Content-Length: {}", photo.len());
    let head = format!("POST /api/convert HTTP/1.1\r\n{local}\r\n{length}");
    for head in [
        head.replace("Length: ", "Length: +"),
        head.replace(" HTTP/1.1", ""),
        head.replace("HTTP/1.1", "XYZ"),
    ] {
        assert_eq!(server.send(&head, &photo).0, 400, "{head}");
    }
    // Non-UTF-8 bytes: fine in a header the server doesn't read, never in the request line or
    // in the headers it reads.
    let raw = |line: &[u8], header: &[u8]| {
        let mut stream = TcpStream::connect(("127.0.0.1", server.port)).unwrap();
        let host = format!("\r\nHost: 127.0.0.1:{}\r\n", server.port);
        stream
            .write_all(&[line, host.as_bytes(), header, b"\r\n\r\n"].concat())
            .unwrap();
        let mut response = Vec::new();
        // A server that drops the request may reset the connection; that reads as no status.
        let _ = stream.read_to_end(&mut response);
        String::from_utf8_lossy(response.get(9..12).unwrap_or_default()).into_owned()
    };
    assert_eq!(raw(b"GET / HTTP/1.1", b"X-Other: \xe9\xff"), "200");
    assert_eq!(raw(b"GET /\xe9 HTTP/1.1", b"X-Other: a"), "400");
    assert_eq!(raw(b"GET / HTTP/1.1", b"\xe9: x"), "400");
    let post = b"POST /api/convert HTTP/1.1";
    assert_eq!(
        raw(post, b"X-File-Name: \xe9.png\r\nContent-Length: 0"),
        "400"
    );
}

#[test]
fn converts_one_file_at_a_time_while_a_client_stalls() {
    let server = Server::start(&["--no-open"]);
    let _stalled = TcpStream::connect(("127.0.0.1", server.port)).unwrap();
    // Each upload has its own name and size, so a mixed-up answer shows.
    let uploads: Vec<_> = (1..=8u32)
        .map(|i| {
            let rgb = gradient(8 * i, 4 * i, 3);
            let png = png(
                8 * i,
                4 * i,
                png::ColorType::Rgb,
                png::BitDepth::Eight,
                &rgb,
                |_| {},
            );
            (format!("{i}.png"), png)
        })
        .collect();
    let answers: Vec<_> = std::thread::scope(|s| {
        let runs: Vec<_> = uploads
            .iter()
            .map(|(name, png)| s.spawn(|| server.convert(name, "{}", png)))
            .collect();
        runs.into_iter().map(|r| r.join().unwrap()).collect()
    });
    for (i, (status, file, _)) in (1..).zip(answers) {
        assert_eq!(status, 200);
        assert_eq!(
            (&file["input"], &file["width"]),
            (&json!(format!("{i}.png")), &json!(8 * i))
        );
    }
}

#[test]
fn reports_a_busy_port_and_shows_its_options() {
    let taken = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = taken.local_addr().unwrap().port().to_string();
    let out = Command::new(BIN)
        .args(["ui", "--no-open", "--port", &port])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(3));
    let stderr = String::from_utf8(out.stderr).unwrap();
    let want = format!(
        " Can't start the page: port {port} is in use.\n  Leave out --port to pick a free one.\n"
    );
    assert!(stderr.ends_with(&want), "{stderr}");
    let bad = Command::new(BIN)
        .args(["ui", "--port", "x"])
        .output()
        .unwrap();
    assert_eq!(bad.status.code(), Some(2));
    let help = Command::new(BIN).args(["ui", "--help"]).output().unwrap();
    let help = String::from_utf8(help.stdout).unwrap();
    for part in [
        "Open the local web page.\n",
        "--port <N>  port to listen on [default: a free one]\n",
        "--no-open   don't open the browser\n",
        "  image-convert formats\n",
    ] {
        assert!(help.contains(part), "{help}");
    }
    let main = Command::new(BIN).arg("--help").output().unwrap().stdout;
    let main = String::from_utf8(main).unwrap();
    assert!(
        main.contains("\n  ui       Open the local web page\n"),
        "{main}"
    );
}

#[test]
fn ends_quietly_when_stdout_closes() {
    // The read end closes before the spawn: dropped after it, a test spawning beside this one
    // could inherit a copy, and the server's first write would succeed.
    let (reader, writer) = std::io::pipe().unwrap();
    drop(reader);
    let out = Command::new(BIN)
        .args(["ui", "--no-open"])
        .stdout(writer)
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    assert_eq!(
        (
            out.status.code(),
            String::from_utf8_lossy(&out.stderr).as_ref()
        ),
        (Some(0), "")
    );
}

/// A stand-in for xdg-open and open that writes down the address it was given.
#[cfg(unix)]
#[test]
fn opens_the_browser_unless_told_not_to() {
    use std::os::unix::fs::PermissionsExt;
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("ui-browser");
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    let opened = dir.join("opened");
    for opener in ["xdg-open", "open"] {
        let script = dir.join(opener);
        let body = format!("#!/bin/sh\necho \"$1\" > '{}'\n", opened.display());
        fs::write(&script, body).unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let path = format!("{}:{}", dir.display(), std::env::var("PATH").unwrap());
    let start = |args: &[&str]| {
        let mut child = Command::new(BIN)
            .arg("ui")
            .args(args)
            .env("PATH", &path)
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut line = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(500));
        child.kill().unwrap();
        child.wait().unwrap();
        line
    };
    start(&["--no-open"]);
    assert!(!opened.exists());
    let line = start(&[]);
    let url = line.split_whitespace().nth(1).unwrap();
    assert_eq!(fs::read_to_string(&opened).unwrap(), format!("{url}\n"));
}

#[cfg(windows)]
#[test]
fn the_exe_carries_the_icon() {
    let ico = fs::read(Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/icon.ico")).unwrap();
    let exe = fs::read(BIN).unwrap();
    // The 256 px entry is a PNG, stored in the exe's resources byte for byte.
    let png = &ico[ico.windows(4).position(|w| w == b"\x89PNG").unwrap()..];
    assert!(exe.windows(png.len()).any(|w| w == png));
}

/// A console of its own, as Explorer gives a double-clicked exe: the page starts. Run by CI
/// (`--ignored`), since it opens the default browser.
#[cfg(windows)]
#[test]
#[ignore = "opens the default browser"]
fn a_double_click_starts_the_page() {
    use std::os::windows::process::CommandExt;
    const CREATE_NEW_CONSOLE: u32 = 0x10;
    let mut child = Command::new(BIN)
        .creation_flags(CREATE_NEW_CONSOLE)
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut line = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    let port = line
        .strip_prefix("Open http://127.0.0.1:")
        .and_then(|rest| rest.split_once(' '))
        .filter(|(_, rest)| rest.ends_with(" close this window to stop.\n"))
        .unwrap_or_else(|| panic!("{line:?}"))
        .0;
    let server = Server {
        child,
        port: port.parse().unwrap(),
    };
    let (status, _, body) = server.send("GET / HTTP/1.1\r\nHost: 127.0.0.1:{port}", b"");
    assert_eq!((status, body.as_slice()), (200, PAGE.as_bytes()));
}
