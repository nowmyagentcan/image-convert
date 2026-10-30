//! Converting one file end to end, the same for the CLI, the MCP server and the page: its type,
//! the decoder for it, the pixel pipeline and the result every face reports. Decoding runs on a
//! worker thread with a large stack, and a panic there becomes this file's `internal` error, so
//! the next file still converts.

use std::cell::Cell;
use std::fs::{self, File};
use std::io::{self, Write};
use std::panic::{self, AssertUnwindSafe, catch_unwind};
use std::path::Path;
use std::sync::Once;
use std::sync::atomic::{AtomicU32, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use schemars::JsonSchema;
use serde::Serialize;

use crate::detect::{Kind, detect};
use crate::error::{Error, Warning, panic_message};
use crate::paths::{Input, Target};
use crate::{Options, Pixels, bmff, color, jpeg, jxl, os, raster, raw, render, svg, tiff};

/// The worker's name, which tells the panic hook to stay quiet.
const WORKER: &str = "converter";
const WORKER_STACK: usize = 64 << 20;

/// What converting one file gave: one entry of the JSON `files` list.
#[derive(Debug, Clone, Default, PartialEq, Serialize, JsonSchema)]
pub struct FileResult {
    pub input: String,
    /// The path written, `-` for stdout; null when nothing was written.
    pub output: Option<String>,
    pub input_format: Option<String>,
    pub source: Option<Source>,
    pub format: Option<&'static str>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub bytes: Option<u64>,
    pub quality: Option<u8>,
    pub resized: Option<bool>,
    pub exif: Option<bool>,
    pub warnings: Vec<Warning>,
    pub error: Option<Error>,
    /// How long each step took, for `--debug`.
    #[serde(skip)]
    pub steps: Vec<(&'static str, Duration)>,
}

/// The decoded source, as stored (before orientation).
#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
pub struct Source {
    pub width: u32,
    pub height: u32,
    pub bit_depth: u8,
    pub alpha: bool,
    pub frames: u32,
    pub orientation: u8,
    pub color: &'static str,
    pub profile: Option<String>,
    pub decoder: &'static str,
}

/// The JSON result of a run: every file, the files written and the files that failed.
#[derive(Debug, Serialize, JsonSchema)]
pub struct Report<'a> {
    pub files: &'a [FileResult],
    pub converted: usize,
    pub failed: usize,
}

impl<'a> Report<'a> {
    pub fn new(files: &'a [FileResult]) -> Self {
        Self {
            files,
            converted: files.iter().filter(|f| f.output.is_some()).count(),
            failed: files.iter().filter(|f| f.error.is_some()).count(),
        }
    }

    /// 0 when every file converted, else the highest exit code of the files' errors.
    pub fn exit_code(&self) -> u8 {
        let errors = self.files.iter().filter_map(|f| f.error.as_ref());
        errors.map(Error::exit_code).max().unwrap_or_default()
    }
}

impl FileResult {
    /// A result for `input` that failed with `error` before anything was decoded.
    pub fn failed(input: String, error: Error) -> Self {
        let mut file = FileResult {
            input,
            ..Default::default()
        };
        file.fail(error);
        file
    }

    /// Marks the file failed: nothing written, no output facts.
    pub fn fail(&mut self, error: Error) {
        self.output = None;
        (self.format, self.width, self.height, self.bytes) = (None, None, None, None);
        (self.quality, self.resized, self.exif) = (None, None, None);
        self.error = Some(error);
    }

    /// The output's file extension: `jpg` for JPEG.
    pub fn extension(&self) -> &'static str {
        match self.format {
            Some("jpeg") => "jpg",
            format => format.unwrap_or_default(),
        }
    }

    /// The source's size upright, as `· from W×H` shows it.
    pub fn upright_source(&self) -> Option<(u32, u32)> {
        let s = self.source.as_ref()?;
        Some(if s.orientation >= 5 {
            (s.height, s.width)
        } else {
            (s.width, s.height)
        })
    }
}

/// Converts a file's bytes; `input` names it in the result and gives the extension that a few
/// RAW kinds are told by. Returns the result and the encoded image, which the caller writes and
/// then records as `output`.
pub fn convert(input: String, data: &[u8], options: &Options) -> (FileResult, Option<Vec<u8>>) {
    let mut file = FileResult {
        input,
        ..Default::default()
    };
    match on_worker(|| run(data, options, &mut file)).and_then(|r| r) {
        Ok(bytes) => (file, Some(bytes)),
        Err(e) => {
            file.fail(e);
            (file, None)
        }
    }
}

fn run(data: &[u8], options: &Options, file: &mut FileResult) -> Result<Vec<u8>, Error> {
    let start = Instant::now();
    let mut kind = detect(data, Path::new(&file.input))?;
    let inflated: Vec<u8>;
    let mut data = data;
    if kind == Kind::Gzip {
        (inflated, kind) = svg::ungzip(data)?;
        data = &inflated;
    }
    file.input_format = Some(kind.name());
    let builtin = "builtin";
    // The pixels, the decoder, and the size and orientation results report when they aren't
    // the pixels' own: a drawing's size before rendering, a HEIF image's before the OS turned it.
    let (px, decoder, stored): (Pixels, _, Option<(u32, u32, u8)>) = match kind {
        Kind::Svg => {
            let (px, (w, h)) = svg::render(data, options)?;
            (px, builtin, Some((w, h, 1)))
        }
        Kind::Emf | Kind::Wmf => {
            let (px, (w, h)) = os::render_metafile(data, kind, options)?;
            (px, "gdiplus", Some((w, h, 1)))
        }
        Kind::Heic | Kind::Avif => {
            let heif = bmff::heif(data, kind)?;
            kind = heif.kind;
            file.input_format = Some(kind.name());
            let px = os::decode_heif(data, &heif)?;
            let orientation = match px.orientation {
                1 => heif.orientation(),
                turned => turned,
            };
            let stored = Some((heif.width, heif.height, orientation));
            (px, os::HEIF_DECODER, stored)
        }
        Kind::Jpeg => (jpeg::decode(data)?, builtin, None),
        Kind::Tiff => (tiff::decode(data)?, builtin, None),
        Kind::Jxl => (jxl::decode(data)?, builtin, None),
        Kind::Cr2
        | Kind::Cr3
        | Kind::Nef
        | Kind::Arw
        | Kind::Dng
        | Kind::Raf
        | Kind::Rw2
        | Kind::Pef => (raw::decode(data, kind)?, "preview", None),
        _ => (raster::decode(data, kind)?, builtin, None),
    };
    let (width, height, orientation) = stored.unwrap_or((px.width, px.height, px.orientation));
    let described = color::describe(px.colour.as_ref());
    file.source = Some(Source {
        width,
        height,
        bit_depth: px.bit_depth,
        alpha: px.layout.has_alpha(),
        frames: px.frames,
        orientation,
        color: described.kind,
        profile: described.profile,
        decoder,
    });
    file.steps.push(("decode", start.elapsed()));

    let start = Instant::now();
    let out = render(px, kind, options)?;
    file.steps.push(("convert", start.elapsed()));
    if let Some(source) = &mut file.source {
        (source.color, source.profile) = (out.color.kind, out.color.profile);
    }
    let vector = matches!(kind, Kind::Svg | Kind::Emf | Kind::Wmf);
    file.resized = Some(match file.upright_source() {
        Some(upright) if vector => upright != (out.width, out.height),
        _ => out.resized,
    });
    file.format = Some(out.format.name());
    (file.width, file.height) = (Some(out.width), Some(out.height));
    file.bytes = Some(out.bytes.len() as u64);
    (file.quality, file.exif) = (out.quality, Some(out.exif));
    file.warnings = out.warnings;
    Ok(out.bytes)
}

/// Reads, converts and writes one input to where `target` says; only a failed write to stdout
/// is an `Err`.
pub fn convert_input(
    input: &Input,
    options: &Options,
    target: &Target,
    overwrite: bool,
) -> io::Result<FileResult> {
    let start = Instant::now();
    let data = match input.load() {
        Ok(data) => data,
        Err(e) => return Ok(FileResult::failed(input.name(), e)),
    };
    let read = start.elapsed();
    let (mut file, bytes) = convert(input.name(), &data, options);
    file.steps.insert(0, ("read", read));
    let Some(bytes) = bytes else {
        return Ok(file);
    };
    let start = Instant::now();
    match input.output(target, file.extension(), overwrite) {
        Ok(Some(path)) => match write(&path, &bytes) {
            Ok(()) => file.output = Some(path.to_string_lossy().into_owned()),
            Err(e) => file.fail(e),
        },
        Ok(None) => {
            let mut stdout = io::stdout().lock();
            stdout.write_all(&bytes)?;
            stdout.flush()?;
            file.output = Some("-".into());
        }
        Err(e) => file.fail(e),
    }
    file.steps.push(("write", start.elapsed()));
    Ok(file)
}

/// Writes `bytes` to `path` through a temporary file in the same folder that is then renamed
/// into place, so a failed write never leaves a partial output; makes the folder as needed.
pub fn write(path: &Path, bytes: &[u8]) -> Result<(), Error> {
    static COUNT: AtomicU32 = AtomicU32::new(0);
    let failed = |e| Error::write_failed(&path.to_string_lossy(), e);
    let dir = match path.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir,
        _ => Path::new("."),
    };
    fs::create_dir_all(dir).map_err(failed)?;
    let n = COUNT.fetch_add(1, Ordering::Relaxed);
    let temp = dir.join(format!(".image-convert-{}-{n}.tmp", std::process::id()));
    let written = File::create_new(&temp)
        .and_then(|mut f| f.write_all(bytes))
        .and_then(|()| fs::rename(&temp, path));
    if written.is_err() {
        let _ = fs::remove_file(&temp);
    }
    written.map_err(failed)
}

thread_local! {
    /// Where the last panic on this thread happened.
    static PANIC_AT: Cell<Option<String>> = const { Cell::new(None) };
}

/// Runs `f` on a worker thread with a 64 MiB stack. A panic there prints nothing and becomes
/// the error `internal`, its location kept as the cause.
fn on_worker<T: Send>(f: impl FnOnce() -> T + Send) -> Result<T, Error> {
    static HOOK: Once = Once::new();
    HOOK.call_once(|| {
        let default = panic::take_hook();
        panic::set_hook(Box::new(move |info| {
            if thread::current().name() == Some(WORKER) {
                PANIC_AT.set(info.location().map(ToString::to_string));
            } else {
                default(info);
            }
        }));
    });
    thread::scope(|scope| {
        let worker = thread::Builder::new()
            .name(WORKER.into())
            .stack_size(WORKER_STACK)
            .spawn_scoped(scope, || {
                catch_unwind(AssertUnwindSafe(f)).map_err(|panic| Error {
                    cause: PANIC_AT.take(),
                    ..Error::internal(&panic_message(&*panic))
                })
            })
            .map_err(|e| Error::internal(&e.to_string()))?;
        worker.join().expect("the worker catches its panics")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_panic_is_an_internal_error_and_the_next_call_still_works() {
        let e = on_worker(|| -> u8 { panic!("boom") }).unwrap_err();
        assert_eq!(
            (e.code, e.message.as_str()),
            (
                "internal",
                "Something went wrong while converting this file: boom."
            )
        );
        assert!(e.cause.as_ref().unwrap().contains("engine.rs"));
        assert_eq!(e.exit_code(), 3);
        assert_eq!(on_worker(|| 7), Ok(7));
    }

    #[test]
    fn deep_recursion_fits_the_worker_stack() {
        fn depth(n: u32) -> u32 {
            let pad = std::hint::black_box([0u8; 1024]);
            if n == 0 {
                pad[0].into()
            } else {
                depth(n - 1) + 1
            }
        }
        // About 30 MiB of frames: far past a default 2 MiB thread stack.
        assert_eq!(on_worker(|| depth(30_000)), Ok(30_000));
    }

    /// A 3 × 2 RGBA PNG with one transparent pixel.
    fn png() -> Vec<u8> {
        let mut out = vec![];
        let mut rgba = [200u8; 24];
        rgba[3] = 0;
        image::ImageEncoder::write_image(
            image::codecs::png::PngEncoder::new(&mut out),
            &rgba,
            3,
            2,
            image::ExtendedColorType::Rgba8,
        )
        .unwrap();
        out
    }

    #[test]
    fn the_json_keeps_the_documented_keys_and_order() {
        let (mut ok, bytes) = convert("a.png".into(), &png(), &Options::default());
        assert!(bytes.unwrap().starts_with(b"\x89PNG"));
        ok.output = Some("a-1.png".into());
        let bad = convert("x.bin".into(), b"hello", &Options::default()).0;
        let files = [ok, bad];
        let report = Report::new(&files);
        assert_eq!(
            serde_json::to_string(&report).unwrap(),
            concat!(
                r#"{"files":[{"input":"a.png","output":"a-1.png","input_format":"png","#,
                r#""source":{"width":3,"height":2,"bit_depth":8,"alpha":true,"frames":1,"#,
                r#""orientation":1,"color":"srgb","profile":null,"decoder":"builtin"},"#,
                r#""format":"png","width":3,"height":2,"bytes":"#,
                "BYTES",
                r#","quality":null,"resized":false,"exif":false,"warnings":[],"error":null},"#,
                r#"{"input":"x.bin","output":null,"input_format":null,"source":null,"#,
                r#""format":null,"width":null,"height":null,"bytes":null,"quality":null,"#,
                r#""resized":null,"exif":null,"warnings":[],"error":{"code":"unsupported_format","#,
                r#""message":"This isn't an image format image-convert reads.","hint":"#,
                r#""Supported: PNG, JPEG, GIF, BMP, ICO, WebP, TIFF, JPEG XL, HEIC/HEIF, AVIF, SVG, "#,
                r#"EMF/WMF (Windows), and RAW from Canon (CR2, CR3), Nikon (NEF), Sony (ARW), "#,
                r#"Adobe DNG, Fujifilm (RAF), Panasonic (RW2) and Pentax (PEF)."}}],"#,
                r#""converted":1,"failed":1}"#
            )
            .replace("BYTES", &files[0].bytes.unwrap().to_string())
        );
        assert_eq!(report.exit_code(), 1);
        assert_eq!(Report::new(&files[..1]).exit_code(), 0);
    }

    #[test]
    fn an_internal_error_outranks_the_others() {
        let files = [
            FileResult::default(),
            FileResult::failed("a".into(), Error::internal("boom")),
            FileResult::failed("b".into(), Error::empty_file()),
        ];
        assert_eq!(Report::new(&files).exit_code(), 3);
        assert_eq!(Report::new(&[]).exit_code(), 0);
    }

    #[test]
    fn a_failure_after_decoding_keeps_the_source_and_drops_the_output_facts() {
        let options = Options {
            max_bytes: Some(10),
            ..Options::default()
        };
        let (file, bytes) = convert("a.png".into(), &png(), &options);
        assert_eq!(bytes, None);
        assert_eq!(file.error.unwrap().code, "cannot_fit");
        assert_eq!(file.source.unwrap().width, 3);
        assert_eq!((file.format, file.bytes, file.resized), (None, None, None));
    }

    #[test]
    fn drawings_report_their_own_size_and_resizing_against_it() {
        let svg = br#"<svg xmlns="http://www.w3.org/2000/svg" width="40" height="20"/>"#;
        let wide = Options {
            width: Some(80),
            ..Options::default()
        };
        let (file, _) = convert("d.svg".into(), svg, &wide);
        let source = file.source.unwrap();
        assert_eq!((source.width, source.height), (40, 20));
        assert_eq!(
            (file.width, file.height, file.resized),
            (Some(80), Some(40), Some(true))
        );
        let gz = {
            let mut e = flate2::write::GzEncoder::new(vec![], flate2::Compression::fast());
            e.write_all(svg).unwrap();
            e.finish().unwrap()
        };
        let (file, _) = convert("d.svgz".into(), &gz, &Options::default());
        assert_eq!(
            (file.input_format.as_deref(), file.resized),
            (Some("svg"), Some(false))
        );
    }

    #[test]
    fn writes_replace_atomically_and_leave_nothing_behind_on_failure() {
        let dir = std::env::temp_dir().join(format!("image-convert-write-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let path = dir.join("sub/out.png");
        write(&path, b"one").unwrap();
        write(&path, b"two").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"two");
        let blocked = dir.join("sub/out.png/inner.png");
        assert_eq!(write(&blocked, b"x").unwrap_err().code, "write_failed");
        let left: Vec<_> = fs::read_dir(dir.join("sub")).unwrap().collect();
        assert_eq!(left.len(), 1);
        fs::remove_dir_all(dir).unwrap();
    }
}
