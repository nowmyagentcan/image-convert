//! Talks to `image-convert mcp` over stdio the way MCP clients do.

mod common;

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use common::*;
use image_convert::Options;
use image_convert::engine;
use image_convert::error::Error;
use serde_json::{Value, json};

const BIN: &str = env!("CARGO_BIN_EXE_image-convert");

const INSTRUCTIONS: &str = "Convert local images that you can't open (HEIC/HEIF, AVIF, JPEG XL, SVG, TIFF, camera RAW, EMF/WMF on Windows) to PNG, JPEG or WebP, sized for vision models. Use view_image to look at any image file, including HEIC, SVG and RAW. Use convert_image to save converted files. Pass absolute paths.";

const VIEW: &str = "Look at a local image file in any format, including ones you can't open yourself: HEIC/HEIF, AVIF, JPEG XL, SVG, TIFF, camera RAW (CR2, CR3, NEF, ARW, DNG, RAF, RW2, PEF; the camera's embedded preview), EMF/WMF (Windows only), and PNG, JPEG, GIF, WebP, BMP, ICO. Applies orientation, converts colour to sRGB, shrinks it to fit vision-model limits and returns it as an image plus a JSON line with its sizes and warnings. Writes nothing. On failure the result has isError true and one text block holding {\"error\":{\"code\",\"message\",\"hint\"}}.";

const CONVERT: &str = "Convert local image files to PNG, JPEG or WebP and save them: HEIC/HEIF, AVIF, JPEG XL, SVG, TIFF, camera RAW (embedded preview), EMF/WMF (Windows only), PNG, JPEG, GIF, WebP, BMP, ICO. Applies orientation and sRGB colour, removes metadata unless keep_exif, and can shrink to vision-model limits (fit \"vision\"). Each output goes next to its input (folders: into a \"converted\" subfolder) unless output_dir is given; existing files are never overwritten. Returns each file's output path, size and warnings; a file that fails gets its own error entry; on invalid arguments the result has isError true and one text block holding {\"error\":{\"code\",\"message\",\"hint\"}}.";

const INITIALIZE: &str = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"test","version":"1.0.0"}}}"#;

/// A folder for one test holding a 2000 × 1500 photo-like PNG, a small SVG and a subfolder
/// with a copy of the SVG.
fn folder(test: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("mcp-{test}"));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("sub")).unwrap();
    let rgb = gradient(2000, 1500, 3);
    let photo = png(
        2000,
        1500,
        png::ColorType::Rgb,
        png::BitDepth::Eight,
        &rgb,
        |_| {},
    );
    fs::write(dir.join("photo.png"), photo).unwrap();
    let svg = r##"<svg xmlns="http://www.w3.org/2000/svg" width="64" height="32"><rect width="40" height="20" fill="#7c3aed"/></svg>"##;
    fs::write(dir.join("logo.svg"), svg).unwrap();
    fs::write(dir.join("sub/logo.svg"), svg).unwrap();
    dir
}

/// Sends each message from `dir`, closes stdin, and returns the responses by id and the exit
/// code. The server writes nothing to stderr.
fn session(dir: &Path, messages: &[Value]) -> (Vec<Value>, i32) {
    let mut child = Command::new(BIN)
        .arg("mcp")
        .current_dir(dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    for m in messages {
        writeln!(stdin, "{m}").unwrap();
    }
    drop(stdin);
    let out = child.wait_with_output().unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stderr), "");
    let mut responses: Vec<Value> = String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    responses.sort_by_key(|r| r["id"].as_u64());
    (responses, out.status.code().unwrap())
}

/// The CLI's `--json` output for the same input, run from `dir`.
fn cli(dir: &Path, args: &[&str]) -> String {
    let out = Command::new(BIN)
        .args(args)
        .arg("--json")
        .current_dir(dir)
        .output()
        .unwrap();
    String::from_utf8(out.stdout).unwrap()
}

fn request(id: u64, method: &str, params: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
}

fn call(id: u64, tool: &str, arguments: Value) -> Value {
    request(
        id,
        "tools/call",
        json!({"name": tool, "arguments": arguments}),
    )
}

fn names(schema: &Value) -> Vec<&str> {
    let mut names: Vec<_> = schema["properties"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    names.sort();
    names
}

/// Exactly `view_image` and `convert_image`, with the documented descriptions, hints and
/// schemas, closed to other argument names.
fn check_tools(list: &Value) {
    let tools = list["result"]["tools"].as_array().unwrap();
    let mut tools: Vec<_> = tools.iter().collect();
    tools.sort_by_key(|t| t["name"].as_str());
    let [convert, view] = tools[..] else {
        panic!("{tools:?}")
    };
    assert_eq!(
        (&convert["name"], &convert["description"]),
        (&json!("convert_image"), &json!(CONVERT))
    );
    assert_eq!(
        (&view["name"], &view["description"]),
        (&json!("view_image"), &json!(VIEW))
    );
    assert_eq!(
        view["annotations"],
        json!({"readOnlyHint": true, "idempotentHint": true, "openWorldHint": false})
    );
    assert_eq!(
        convert["annotations"],
        json!({"readOnlyHint": false, "destructiveHint": false, "idempotentHint": false, "openWorldHint": false})
    );
    let (vi, ci) = (&view["inputSchema"], &convert["inputSchema"]);
    for schema in [vi, ci] {
        assert_eq!(schema["additionalProperties"], false);
        assert!(!schema.to_string().contains("\"format\""), "{schema}");
    }
    assert_eq!(names(vi), ["fit", "height", "max_bytes", "path", "width"]);
    assert_eq!(vi["required"], json!(["path"]));
    let fit = &vi["properties"]["fit"];
    assert_eq!(
        (&fit["enum"], &fit["default"]),
        (&json!(["vision", "vision-hd", null]), &json!("vision"))
    );
    let bytes = &vi["properties"]["max_bytes"];
    assert_eq!(
        (&bytes["minimum"], &bytes["maximum"], &bytes["default"]),
        (&json!(10000), &json!(7500000), &json!(1000000))
    );
    assert_eq!(vi["properties"]["width"]["maximum"], 16384);
    assert_eq!(
        names(ci),
        [
            "background",
            "fit",
            "height",
            "keep_exif",
            "max_bytes",
            "max_megapixels",
            "max_side",
            "output_dir",
            "paths",
            "quality",
            "recursive",
            "to",
            "width"
        ]
    );
    assert_eq!(ci["required"], json!(["paths"]));
    let paths = &ci["properties"]["paths"];
    assert_eq!(
        (&paths["minItems"], &paths["maxItems"]),
        (&json!(1), &json!(1000))
    );
    assert_eq!(
        ci["properties"]["to"]["enum"],
        json!(["auto", "png", "jpeg", "webp", null])
    );
    assert_eq!(ci["properties"]["quality"]["default"], 85);
    assert_eq!(ci["properties"]["max_megapixels"]["minimum"], 0.01);
    assert!(view.get("outputSchema").is_none());
    let output = &convert["outputSchema"];
    assert_eq!(output["required"], json!(["files", "converted", "failed"]));
}

/// A result equal to the CLI's `--json` output: as structured content, and byte for byte as
/// the text block.
fn check_answer(response: &Value, cli: &str) {
    let result = &response["result"];
    assert_eq!(result["isError"], false);
    let text = result["content"][0]["text"].as_str().unwrap();
    assert_eq!(format!("{text}\n"), cli);
    assert_eq!(
        result["structuredContent"],
        serde_json::from_str::<Value>(cli).unwrap()
    );
}

/// An error result: no structured content, and one text block holding the error object.
fn check_error(response: &Value, error: &str) {
    let result = &response["result"];
    assert_eq!(result["isError"], true, "{response}");
    assert!(result.get("structuredContent").is_none());
    assert_eq!(result["content"].as_array().unwrap().len(), 1);
    let text = result["content"][0]["text"].as_str().unwrap();
    assert_eq!(format!("{text}\n"), error);
}

/// The code of an error result, which carries no structured content.
fn error_code(response: &Value) -> String {
    assert_eq!(response["result"]["isError"], true, "{response}");
    assert!(response["result"].get("structuredContent").is_none());
    let text = response["result"]["content"][0]["text"].as_str().unwrap();
    let error: Value = serde_json::from_str(text).unwrap();
    error["error"]["code"].as_str().unwrap().to_owned()
}

/// One image block within the limits and one text block with the file object, nothing else;
/// returns the file object and the decoded image's size.
fn check_view(response: &Value) -> (Value, (u32, u32)) {
    let result = &response["result"];
    assert_eq!(result["isError"], false, "{response}");
    assert!(result.get("structuredContent").is_none());
    let [image, text] = result["content"].as_array().unwrap().as_slice() else {
        panic!("{result}")
    };
    assert_eq!(image["type"], "image");
    assert!(image.get("annotations").is_none());
    let data = STANDARD.decode(image["data"].as_str().unwrap()).unwrap();
    assert!(data.len() <= 1_000_000, "{}", data.len());
    let file: Value = serde_json::from_str(text["text"].as_str().unwrap()).unwrap();
    assert_eq!(
        image["mimeType"],
        format!("image/{}", file["format"].as_str().unwrap())
    );
    assert_eq!(file["output"], Value::Null);
    assert_eq!(file["bytes"], data.len());
    let shown = engine::convert("shown".into(), &data, &Options::default()).0;
    let source = shown.source.unwrap();
    (file, (source.width, source.height))
}

#[test]
fn converts_like_the_cli_after_an_initialize_handshake() {
    let (dir, twin) = (folder("initialize"), folder("initialize-cli"));
    let (responses, code) = session(
        &dir,
        &[
            serde_json::from_str(INITIALIZE).unwrap(),
            json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
            request(2, "tools/list", json!({})),
            call(
                3,
                "convert_image",
                json!({"paths": ["photo.png", "gone.png", "sub"], "fit": "vision", "to": "jpeg"}),
            ),
            call(
                4,
                "convert_image",
                json!({"paths": ["logo.svg"], "width": 640, "output_dir": "out/", "quality": 90}),
            ),
            call(5, "convert_image", json!({"paths": []})),
            call(
                6,
                "convert_image",
                json!({"paths": ["photo.png"], "quality": 0}),
            ),
            call(
                7,
                "convert_image",
                json!({"paths": ["photo.png"], "fit": "huge"}),
            ),
            call(
                8,
                "convert_image",
                json!({"paths": ["photo.png"], "max_bytes": 5}),
            ),
            call(
                9,
                "convert_image",
                json!({"paths": ["photo.png"], "colour": "red"}),
            ),
            call(10, "convert_image", json!({"paths": "photo.png"})),
            call(
                11,
                "convert_image",
                json!({"paths": vec!["photo.png"; 1001]}),
            ),
        ],
    );
    assert_eq!(code, 0);
    assert_eq!(responses.len(), 11);
    let init = &responses[0]["result"];
    assert_eq!(init["protocolVersion"], "2025-11-25");
    assert_eq!(
        init["serverInfo"],
        json!({"name": "image-convert", "version": "0.1.0"})
    );
    assert_eq!(init["instructions"], INSTRUCTIONS);
    assert_eq!(init["capabilities"]["tools"], json!({}));
    check_tools(&responses[1]);
    let first = cli(
        &twin,
        &[
            "photo.png",
            "gone.png",
            "sub",
            "--fit",
            "vision",
            "--to",
            "jpeg",
        ],
    );
    check_answer(&responses[2], &first);
    let files = &responses[2]["result"]["structuredContent"]["files"];
    assert_eq!(
        (&files[0]["output"], &files[0]["width"], &files[0]["height"]),
        (&json!("photo.jpg"), &json!(1238), &json!(928))
    );
    assert_eq!(files[1]["error"]["code"], "not_found");
    assert!(dir.join("sub/converted/logo.jpg").is_file());
    let second = cli(
        &twin,
        &[
            "logo.svg",
            "--width",
            "640",
            "-o",
            "out/",
            "--quality",
            "90",
        ],
    );
    check_answer(&responses[3], &second);
    assert!(dir.join("out/logo.png").is_file());
    check_error(&responses[4], &cli(&twin, &[]));
    check_error(&responses[5], &cli(&twin, &["photo.png", "--quality", "0"]));
    check_error(&responses[6], &cli(&twin, &["photo.png", "--fit", "huge"]));
    check_error(
        &responses[7],
        &cli(&twin, &["photo.png", "--max-bytes", "5"]),
    );
    for response in &responses[8..] {
        assert_eq!(error_code(response), "usage");
    }
}

#[test]
fn views_an_image_sized_for_vision_without_writing() {
    let dir = folder("view");
    let view = |id, arguments| call(id, "view_image", arguments);
    let mut messages = vec![
        serde_json::from_str(INITIALIZE).unwrap(),
        json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
        view(2, json!({"path": "photo.png"})),
        view(3, json!({"path": "logo.svg", "width": 1000})),
        view(4, json!({"path": "gone.png"})),
        view(5, json!({"path": "photo.png", "fit": "none"})),
        view(6, json!({"path": "photo.png", "max_bytes": 9999})),
        view(7, json!({"path": "photo.png", "width": 16385})),
        view(8, json!({"path": "photo.png", "out": "x.png"})),
        view(9, json!({"path": "sub"})),
        view(
            10,
            json!({"path": "photo.png", "fit": "vision-hd", "max_bytes": 200000}),
        ),
    ];
    let heic = os_heif("heif-irot.heic").is_some();
    fs::write(dir.join("irot.heic"), fixture("heif-irot.heic")).unwrap();
    messages.push(view(11, json!({"path": "irot.heic"})));
    let (responses, code) = session(&dir, &messages);
    assert_eq!((code, responses.len()), (0, 11));
    let (file, size) = check_view(&responses[1]);
    assert_eq!(size, (1238, 928));
    assert_eq!(
        (&file["input"], &file["format"], &file["resized"]),
        (&json!("photo.png"), &json!("png"), &json!(true))
    );
    let (file, size) = check_view(&responses[2]);
    assert_eq!((size, &file["format"]), ((1000, 500), &json!("png")));
    let missing = Error::not_found("gone.png").to_json();
    check_error(&responses[3], &format!("{missing}\n"));
    for response in &responses[4..8] {
        assert_eq!(error_code(response), "usage");
    }
    assert_eq!(error_code(&responses[8]), "unreadable");
    let (file, (w, h)) = check_view(&responses[9]);
    assert!(file["bytes"].as_u64().unwrap() <= 200_000 && w <= 2048 && h <= 2048);
    if heic {
        // Stored 90° turned with EXIF orientation 6 as well: upright once, not twice.
        let (_, size) = check_view(&responses[10]);
        assert_eq!(size, (64, 128));
    } else {
        assert_eq!(error_code(&responses[10]), "codec_missing");
    }
    let mut left: Vec<_> = fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    left.sort();
    assert_eq!(left, ["irot.heic", "logo.svg", "photo.png", "sub"]);
}

#[test]
fn converts_after_server_discover() {
    let (dir, twin) = (folder("discover"), folder("discover-cli"));
    let meta = json!({
        "io.modelcontextprotocol/protocolVersion": "2026-07-28",
        "io.modelcontextprotocol/clientInfo": {"name": "test", "version": "1.0.0"},
        "io.modelcontextprotocol/clientCapabilities": {}
    });
    let convert = json!({"_meta": meta, "name": "convert_image", "arguments": {"paths": ["logo.svg"], "to": "webp"}});
    let (responses, code) = session(
        &dir,
        &[
            request(1, "server/discover", json!({"_meta": meta})),
            request(2, "tools/list", json!({"_meta": meta})),
            request(3, "tools/call", convert),
        ],
    );
    assert_eq!(code, 0);
    let discover = &responses[0]["result"];
    let versions = discover["supportedVersions"].as_array().unwrap();
    assert!(versions.contains(&json!("2026-07-28")) && versions.contains(&json!("2025-11-25")));
    assert_eq!(
        discover["_meta"]["io.modelcontextprotocol/serverInfo"]["name"],
        "image-convert"
    );
    assert_eq!(discover["instructions"], INSTRUCTIONS);
    check_tools(&responses[1]);
    check_answer(&responses[2], &cli(&twin, &["logo.svg", "--to", "webp"]));
}

#[test]
fn exits_quietly_when_stdin_closes() {
    assert_eq!(session(&folder("stdin"), &[]), (vec![], 0));
}

#[test]
fn exits_quietly_when_stdout_closes() {
    // The read end closes before the spawn: dropped after it, a test spawning beside this one
    // could inherit a copy and keep the pipe open.
    let (reader, writer) = std::io::pipe().unwrap();
    drop(reader);
    let mut child = Command::new(BIN)
        .arg("mcp")
        .stdin(Stdio::piped())
        .stdout(writer)
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let _ = writeln!(stdin, "{INITIALIZE}");
    let start = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        assert!(start.elapsed() < Duration::from_secs(10), "still running");
        thread::sleep(Duration::from_millis(20));
    };
    drop(stdin);
    let out = child.wait_with_output().unwrap();
    assert_eq!(
        (status.code(), String::from_utf8_lossy(&out.stderr).as_ref()),
        (Some(0), "")
    );
}

#[test]
fn has_a_help_and_takes_no_arguments() {
    let out = Command::new(BIN).args(["mcp", "--help"]).output().unwrap();
    let help = String::from_utf8(out.stdout).unwrap();
    assert_eq!(out.status.code(), Some(0));
    assert!(help.starts_with("Run the MCP server on stdio.\n"), "{help}");
    assert!(help.ends_with("  image-convert formats\n"), "{help}");
    let main = Command::new(BIN).arg("--help").output().unwrap().stdout;
    let main = String::from_utf8(main).unwrap();
    assert!(
        main.contains("\n  mcp      Run the MCP server on stdio\n"),
        "{main}"
    );
    let extra = Command::new(BIN).args(["mcp", "x.png"]).output().unwrap();
    assert_eq!(extra.status.code(), Some(2));
}

#[test]
fn a_dash_path_leaves_the_message_stream_alone() {
    let mut child = Command::new(BIN)
        .arg("mcp")
        .current_dir(folder("dash"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let stdout = BufReader::new(child.stdout.take().unwrap());
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        stdout
            .lines()
            .map_while(Result::ok)
            .for_each(|l| drop(tx.send(l)))
    });
    let mut send = |message: Value| writeln!(stdin, "{message}").unwrap();
    let answer = |id: u64| {
        let line = rx.recv_timeout(Duration::from_secs(5)).expect("an answer");
        let response: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(response["id"], id);
        response
    };
    send(serde_json::from_str(INITIALIZE).unwrap());
    answer(1);
    send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
    // The server's stdin carries the protocol, so "-" can't mean a file read from it.
    // On Linux and macOS the device paths name that same stream.
    let paths: &[&str] = if cfg!(unix) {
        &["-", "/dev/stdin", "/dev/fd/0"]
    } else {
        &["-"]
    };
    send(call(2, "convert_image", json!({ "paths": paths })));
    let files = answer(2)["result"]["structuredContent"]["files"].clone();
    assert_eq!(files.as_array().unwrap().len(), paths.len(), "{files}");
    for (path, file) in paths.iter().zip(files.as_array().unwrap()) {
        assert_eq!(file["input"], *path);
        assert_eq!(file["error"]["code"], "unreadable", "{file}");
    }
    send(call(3, "view_image", json!({ "path": "-" })));
    assert_eq!(error_code(&answer(3)), "unreadable");
    send(request(4, "ping", json!({})));
    answer(4);
    drop(stdin);
    assert!(child.wait().unwrap().success());
}
