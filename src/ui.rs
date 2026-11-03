//! The local page on 127.0.0.1: `GET /` serves it and `POST /api/convert` converts one uploaded
//! file and answers with the image. Nothing is written to disk.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{Ipv4Addr, Shutdown, TcpListener, TcpStream};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::thread;
use std::time::Duration;

use image_convert::Choices;
use image_convert::engine;
use image_convert::error::Error;
use image_convert::paths::MAX_FILE;

const PAGE: &str = include_str!("ui.html");
/// The most a request may send in its line and headers.
const HEAD_LIMIT: u64 = 16 * 1024;
/// The page reaches nothing but its own server; thumbnails are object URLs.
const CSP: &str = "default-src 'none'; style-src 'unsafe-inline'; script-src 'unsafe-inline'; img-src data: blob:; connect-src 'self'; base-uri 'none'; form-action 'none'";
const JSON: &str = "application/json";

/// Requests are answered side by side, but files are converted one at a time: each can take
/// hundreds of megabytes.
static CONVERT: Mutex<()> = Mutex::new(());

struct Response {
    status: u16,
    kind: String,
    body: Vec<u8>,
    /// `X-Result`: the file object of a converted image, percent-encoded.
    result: Option<String>,
}

fn json(status: u16, body: String) -> Response {
    Response {
        status,
        kind: JSON.into(),
        body: body.into_bytes(),
        result: None,
    }
}

fn error(status: u16, code: &'static str, message: &str) -> Response {
    json(status, Error::new(code, message).to_json())
}

/// Serves on 127.0.0.1:`port` (0 picks a free port) until the process is stopped, and opens
/// the page in the default browser when `open` is set. `stop` says how to stop it.
pub fn serve(port: u16, open: bool, stop: &str) -> Result<(), Error> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, port)).map_err(|e| {
        if e.kind() == io::ErrorKind::AddrInUse {
            Error::port_in_use(port)
        } else {
            Error::new("io", format!("Can't start the page: {e}."))
        }
    })?;
    let port = listener.local_addr().map_or(port, |a| a.port());
    let url = format!("http://127.0.0.1:{port}");
    // With stdout closed nobody learns the address, so end quietly as other commands do.
    match writeln!(io::stdout(), "Open {url}{stop}") {
        Err(e) if e.kind() == io::ErrorKind::BrokenPipe => return Ok(()),
        Err(e) => return Err(Error::io(e)),
        Ok(()) => {}
    }
    if open {
        browse(url);
    }
    for stream in listener.incoming().flatten() {
        // A client that hangs up or stalls loses only its own connection.
        thread::spawn(move || handle(&stream, port));
    }
    Ok(())
}

/// Asks the OS to open `url`; when it can't, the printed address is enough.
fn browse(url: String) {
    let mut command = if cfg!(windows) {
        let mut c = Command::new("cmd");
        c.args(["/C", "start", ""]);
        c
    } else if cfg!(target_os = "macos") {
        Command::new("open")
    } else {
        Command::new("xdg-open")
    };
    command
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // Some openers wait for the browser to close, so they run beside the server.
    thread::spawn(move || command.status());
}

/// One request, one response, then the connection closes.
fn handle(stream: &TcpStream, port: u16) -> io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    stream.set_write_timeout(Some(Duration::from_secs(10)))?;
    let r = respond(&mut BufReader::new(stream.take(HEAD_LIMIT)), port)?;
    let reason = match r.status {
        200 => "OK",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        413 => "Content Too Large",
        _ => "Unprocessable Content",
    };
    let result = r
        .result
        .map(|x| format!("X-Result: {x}\r\n"))
        .unwrap_or_default();
    let mut out = stream;
    write!(
        out,
        "HTTP/1.1 {} {reason}\r\nContent-Type: {}\r\nContent-Length: {}\r\n{result}\
         Cache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\n\
         Content-Security-Policy: {CSP}\r\nConnection: close\r\n\r\n",
        r.status,
        r.kind,
        r.body.len()
    )?;
    out.write_all(&r.body)?;
    stream.shutdown(Shutdown::Write)?;
    // Read what the client still sends, so closing doesn't reset the connection before it
    // has read the answer.
    io::copy(&mut stream.take(1 << 20), &mut io::sink()).map(drop)
}

fn respond(reader: &mut BufReader<io::Take<&TcpStream>>, port: u16) -> io::Result<Response> {
    // Lines are bytes: a header value may carry opaque ones (RFC 9110 §5.5).
    let (mut request, mut line) = (Vec::new(), Vec::new());
    reader.read_until(b'\n', &mut request)?;
    let (mut host, mut origin, mut length, mut name, mut options) = (None, None, None, None, None);
    loop {
        line.clear();
        if reader.read_until(b'\n', &mut line)? == 0 {
            return Ok(match reader.get_ref().limit() {
                0 => error(
                    413,
                    "too_large",
                    "The request headers are larger than 16 KB.",
                ),
                _ => error(400, "bad_request", "The request ended early."),
            });
        }
        if line.iter().all(|b| b"\r\n".contains(b)) {
            break;
        }
        let token = |n: &[u8]| {
            !n.is_empty()
                && n.iter()
                    .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(b))
        };
        // RFC 9110 §5.5: a field value is visible bytes, spaces and tabs; any other control byte
        // makes it invalid, and only the line's own CRLF ends it.
        let value = |v: &[u8]| {
            let v = v
                .strip_suffix(b"\r\n")
                .or(v.strip_suffix(b"\n"))
                .unwrap_or(v);
            !v.iter().any(|&b| (b < b' ' && b != b'\t') || b == 0x7f)
        };
        let Some(colon) = line
            .iter()
            .position(|&b| b == b':')
            .filter(|&i| token(&line[..i]) && value(&line[i + 1..]))
        else {
            return Ok(error(400, "bad_request", "Can't read a request header."));
        };
        let field = match line[..colon].to_ascii_lowercase().as_slice() {
            b"host" => &mut host,
            b"origin" => &mut origin,
            b"content-length" => &mut length,
            b"x-file-name" => &mut name,
            b"x-options" => &mut options,
            // Bodies come only with a Content-Length; chunked ones aren't read.
            b"transfer-encoding" => {
                return Ok(error(
                    400,
                    "bad_request",
                    "Send the body with a Content-Length.",
                ));
            }
            _ => continue,
        };
        // The headers read here are text; other headers may carry any bytes.
        let Ok(value) = std::str::from_utf8(&line[colon + 1..]) else {
            return Ok(error(400, "bad_request", "Can't read a request header."));
        };
        // A second value could disagree with the first one checked (RFC 9112 §3.2, §6.3).
        if field.replace(value.trim().to_owned()).is_some() {
            return Ok(error(400, "bad_request", "A request header is repeated."));
        }
    }
    // RFC 9112 §3: the request line is ASCII.
    let request = std::str::from_utf8(&request).ok().filter(|r| r.is_ascii());
    let mut parts = request
        .unwrap_or_default()
        .trim_end_matches(['\r', '\n'])
        .split(' ');
    let (Some(method), Some(target), Some("HTTP/1.1" | "HTTP/1.0"), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Ok(error(400, "bad_request", "Can't read the request line."));
    };
    let local = |scheme: &str, value: &str| {
        ["127.0.0.1", "localhost"]
            .iter()
            .any(|h| value.eq_ignore_ascii_case(&format!("{scheme}{h}:{port}")))
    };
    if !host.is_some_and(|h| local("", &h)) || origin.is_some_and(|o| !local("http://", &o)) {
        return Ok(error(
            403,
            "forbidden",
            "Only this computer's page can use image-convert ui.",
        ));
    }
    let path = target.split_once('?').map_or(target, |(path, _)| path);
    match (method, path) {
        ("GET", "/") => Ok(Response {
            status: 200,
            kind: "text/html; charset=utf-8".into(),
            body: PAGE.into(),
            result: None,
        }),
        ("POST", "/api/convert") => {
            // Digits only: Rust would also read a leading `+`.
            let digits = |l: &String| l.bytes().all(|b| b.is_ascii_digit());
            let Some(length) = length.map_or(Some(0), |l| l.parse().ok().filter(|_| digits(&l)))
            else {
                return Ok(error(400, "bad_request", "Can't read the request length."));
            };
            if length > MAX_FILE {
                return Ok(error(413, "too_large", "The file is larger than 512 MiB."));
            }
            let buffered = reader.buffer().len() as u64;
            reader.get_mut().set_limit(length.saturating_sub(buffered));
            let mut body = Vec::new();
            reader.take(length).read_to_end(&mut body)?;
            if (body.len() as u64) < length {
                return Ok(error(400, "bad_request", "The request ended early."));
            }
            Ok(convert(name.as_deref(), options.as_deref(), &body))
        }
        _ => Ok(error(404, "not_found", "There's nothing here.")),
    }
}

/// Converts one upload: 200 with the image and its file object in `X-Result`, 422 with the
/// file object or the options' usage error, 400 when the name or options can't be read.
fn convert(name: Option<&str>, options: Option<&str>, data: &[u8]) -> Response {
    let Some(name) = unescape(name.unwrap_or_default()) else {
        return error(400, "bad_request", "Can't read the file name.");
    };
    let choices = options.map(serde_json::from_str::<Choices>).transpose();
    let options = match choices {
        Ok(choices) => choices.unwrap_or_default().options(),
        Err(e) => return error(400, "bad_request", &format!("Can't read the options: {e}.")),
    };
    let options = match options {
        Ok(options) => options,
        Err(e) => return json(422, e.to_json()),
    };
    let _one_at_a_time = CONVERT.lock().unwrap_or_else(|e| e.into_inner());
    let (mut file, bytes) = engine::convert(name, data, &options);
    let Some(bytes) = bytes else {
        return json(
            422,
            serde_json::to_string(&file).expect("results are plain data"),
        );
    };
    let stem = Path::new(&file.input)
        .file_stem()
        .map_or("image".into(), |s| s.to_string_lossy());
    file.output = Some(format!("{stem}.{}", file.extension()));
    let result = serde_json::to_string(&file).expect("results are plain data");
    Response {
        status: 200,
        kind: format!("image/{}", file.format.unwrap_or_default()),
        body: bytes,
        result: Some(escape(&result)),
    }
}

/// Percent-encodes every byte but letters, digits and `-_.~`, which `decodeURIComponent` reads.
fn escape(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                char::from(b).to_string()
            }
            b => format!("%{b:02X}"),
        })
        .collect()
}

/// Percent-decodes `s` as UTF-8.
fn unescape(s: &str) -> Option<String> {
    let mut bytes = Vec::with_capacity(s.len());
    let mut rest = s.as_bytes();
    while let Some((&b, tail)) = rest.split_first() {
        rest = tail;
        if b == b'%' {
            let hex = tail
                .get(..2)
                .filter(|h| h.iter().all(u8::is_ascii_hexdigit))?;
            bytes.push(u8::from_str_radix(std::str::from_utf8(hex).ok()?, 16).ok()?);
            rest = &tail[2..];
        } else {
            bytes.push(b);
        }
    }
    String::from_utf8(bytes).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_percent_decoded_utf_8_and_results_encoded_back() {
        assert_eq!(
            unescape("Gr%C3%BC%C3%9Fe%20scan.heic").as_deref(),
            Some("Grüße scan.heic")
        );
        assert_eq!(unescape("plain.png").as_deref(), Some("plain.png"));
        for bad in ["%", "%4", "%zz", "%+1", "%C3"] {
            assert_eq!(unescape(bad), None, "{bad}");
        }
        let text = r#"{"input":"a b/ü<.png"}"#;
        assert_eq!(escape("a b~"), "a%20b~");
        assert_eq!(unescape(&escape(text)).as_deref(), Some(text));
    }
}
