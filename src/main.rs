//! The command line: `image-convert [convert] <PATH>... [OPTIONS]` and `image-convert formats`.

mod text;

use std::ffi::OsString;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Instant;

use clap::{ArgAction, CommandFactory, FromArgMatches, Parser};
use image_convert::engine::{self, FileResult, Report};
use image_convert::error::Error;
use image_convert::paths::{self, Input, Target};
use image_convert::{Fit, Format, Options, os};
use serde::Serialize;
use text::{Look, Out};

const HELP: &str = "\
{about}

{usage-heading} {usage}

Commands:
  convert  Convert files and folders (the default command)
  formats  Show which formats this machine can decode

Options:
{options}{after-help}";

const READS: &str = "\
Reads (image-convert formats shows what this machine can decode):
  png    PNG, APNG first frame
  jpeg   JPEG, baseline and progressive, CMYK
  gif    GIF, first frame
  bmp    BMP
  ico    ICO, largest image
  webp   WebP, first frame
  tiff   TIFF, first page, strips
  jxl    JPEG XL
  heic   HEIC/HEIF, through the system's decoder
  avif   AVIF, through the system's decoder
  svg    SVG and SVGZ
  emf    EMF and EMZ, Windows only
  wmf    placeable WMF and WMZ, Windows only
  raw    CR2, CR3, NEF, ARW, DNG, RAF, RW2, PEF: the camera's embedded preview
Writes PNG, JPEG or WebP, upright and sRGB, with no metadata unless --keep-exif.

Presets:
  vision     longest side 1568, 1.15 megapixels, 3,750,000 bytes
  vision-hd  longest side 2048, 2.4 megapixels, 7,500,000 bytes";

const EXAMPLES: &str = "\
Examples:
  image-convert IMG_0042.HEIC
  image-convert logo.svg --width 1024 -o logo.png
  image-convert photos/ -r --to jpeg
  image-convert screenshot.png --fit vision --json
  image-convert DSC_0001.NEF --fit vision -o preview.jpg
  cat photo.heic | image-convert - -o photo.jpg
  image-convert formats";

/// `image-convert formats`
#[derive(Parser)]
#[command(
    name = "image-convert formats",
    about = "Show which formats this machine can decode.",
    after_help = EXAMPLES,
    disable_version_flag = true
)]
struct Formats {
    /// print one JSON object
    #[arg(long)]
    json: bool,
}

#[derive(Parser, Default)]
#[command(
    name = "image-convert",
    version,
    about = "Convert HEIC, SVG, RAW and other images to PNG, JPEG or WebP.",
    help_template = HELP,
    override_usage = "image-convert [COMMAND] [PATH]... [OPTIONS]",
    disable_help_flag = true,
    disable_version_flag = true
)]
struct Cli {
    #[arg(value_name = "PATH", hide = true)]
    paths: Vec<PathBuf>,
    /// auto, png, jpeg or webp [default: auto]
    #[arg(long, value_name = "FORMAT")]
    to: Option<String>,
    /// output file, folder, or - for stdout [default: beside the input]
    #[arg(short, long, value_name = "PATH")]
    output: Option<PathBuf>,
    /// none, vision or vision-hd [default: none]
    #[arg(long, value_name = "PRESET")]
    fit: Option<String>,
    /// longest side in pixels, 1-65535
    #[arg(long, value_name = "N")]
    max_side: Option<String>,
    /// width × height in millions, 0.01-200
    #[arg(long, value_name = "X")]
    max_megapixels: Option<String>,
    /// output size, e.g. 500KB, 2MB, 3MiB
    #[arg(long, value_name = "N")]
    max_bytes: Option<String>,
    /// fit inside this width (SVG, EMF, WMF: render at it)
    #[arg(long, value_name = "N")]
    width: Option<String>,
    /// fit inside this height (SVG, EMF, WMF: render at it)
    #[arg(long, value_name = "N")]
    height: Option<String>,
    /// JPEG and WebP quality, 1-100 [default: 85]
    #[arg(long, value_name = "N")]
    quality: Option<String>,
    /// fill behind transparency in JPEG: white, black or #rrggbb [default: white]
    #[arg(long, value_name = "COLOR")]
    background: Option<String>,
    /// copy the EXIF block into JPEG and PNG output
    #[arg(long)]
    keep_exif: bool,
    /// replace an existing -o file
    #[arg(long)]
    overwrite: bool,
    /// convert folders inside folders too
    #[arg(short, long)]
    recursive: bool,
    /// print one JSON object
    #[arg(long)]
    json: bool,
    /// show causes, the decoder and per-step timings
    #[arg(long)]
    debug: bool,
    /// print help
    #[arg(short, long, action = ArgAction::Help)]
    help: Option<bool>,
    /// print version
    #[arg(short = 'V', long, action = ArgAction::Version)]
    version: Option<bool>,
}

impl Cli {
    /// The conversion options, or the usage error a value gives.
    fn options(&self) -> Result<Options, Error> {
        let size = |value: &Option<String>, name| {
            let n = value
                .as_deref()
                .map(|v| v.parse().ok().filter(|n| (1..=65535).contains(n)));
            n.map(|n| n.ok_or_else(|| Error::bad_size(name, 1, 65535)))
                .transpose()
        };
        let to = match self.to.as_deref().map(str::to_ascii_lowercase).as_deref() {
            None | Some("auto") => None,
            Some("png") => Some(Format::Png),
            Some("jpeg" | "jpg") => Some(Format::Jpeg),
            Some("webp") => Some(Format::Webp),
            Some(_) => return Err(Error::bad_format(self.to.as_deref().unwrap_or_default())),
        };
        let fit = match self.fit.as_deref() {
            None | Some("none") => Fit::None,
            Some("vision") => Fit::Vision,
            Some("vision-hd") => Fit::VisionHd,
            Some(other) => return Err(Error::bad_fit(other)),
        };
        let max_megapixels = self.max_megapixels.as_deref().map(|v| {
            v.parse()
                .ok()
                .filter(|x: &f64| (0.01..=200.0).contains(x))
                .ok_or_else(Error::bad_megapixels)
        });
        let quality = self.quality.as_deref().map(|v| {
            v.parse()
                .ok()
                .filter(|q| (1..=100).contains(q))
                .ok_or_else(Error::bad_quality)
        });
        let max_bytes = self
            .max_bytes
            .as_deref()
            .map(|v| byte_size(v).ok_or_else(|| Error::bad_bytes(v)));
        let background = self
            .background
            .as_deref()
            .map(|v| colour(v).ok_or_else(|| Error::bad_color(v)));
        Ok(Options {
            to,
            fit,
            max_side: size(&self.max_side, "max-side")?,
            max_megapixels: max_megapixels.transpose()?,
            max_bytes: max_bytes.transpose()?,
            width: size(&self.width, "width")?,
            height: size(&self.height, "height")?,
            quality: quality.transpose()?,
            background: background.transpose()?.unwrap_or([255; 3]),
            keep_exif: self.keep_exif,
        })
    }

    /// Where outputs go, with `options.to` set from a `-o` file's extension when `--to` isn't
    /// given; checks `-o` against `--json`, stdin and `--to`.
    fn target(&self, options: &mut Options) -> Result<Target, Error> {
        let target = Target::new(self.output.as_deref());
        let stdin = self.paths.iter().any(|p| p.as_os_str() == "-");
        match &target {
            Target::Stdout if self.json => return Err(Error::json_with_stdout()),
            Target::Beside if stdin => return Err(Error::stdin_needs_output()),
            Target::File(file) => {
                let ext = file
                    .extension()
                    .map(|e| e.to_string_lossy().to_ascii_lowercase());
                let by_ext = match ext.as_deref() {
                    Some("png") => Some(Format::Png),
                    Some("jpg" | "jpeg") => Some(Format::Jpeg),
                    Some("webp") => Some(Format::Webp),
                    _ => None,
                };
                match (options.to, by_ext) {
                    (None, None) => return Err(Error::bad_extension(&file.to_string_lossy())),
                    (None, by_ext) => options.to = by_ext,
                    (Some(to), Some(by_ext)) if to != by_ext => {
                        let to = self.to.as_deref().unwrap_or_default().to_ascii_lowercase();
                        return Err(Error::format_conflict(
                            &to,
                            ext.as_deref().unwrap_or_default(),
                        ));
                    }
                    _ => {}
                }
            }
            _ => {}
        }
        Ok(target)
    }
}

/// `500KB`, `2MB`, `3MiB` or plain digits as bytes, between 1 KB and 1 GiB.
fn byte_size(value: &str) -> Option<u64> {
    let digits = value.len() - value.trim_start_matches(|c: char| c.is_ascii_digit()).len();
    let (number, unit) = value.split_at(digits);
    let unit = match unit.to_ascii_lowercase().as_str() {
        "" => 1,
        "kb" => 1000,
        "mb" => 1_000_000,
        "kib" => 1024,
        "mib" => 1 << 20,
        _ => return None,
    };
    let bytes = number.parse::<u64>().ok()?.checked_mul(unit)?;
    (1000..=1 << 30).contains(&bytes).then_some(bytes)
}

/// `white`, `black` or `#rrggbb`.
fn colour(value: &str) -> Option<[u8; 3]> {
    match value.to_ascii_lowercase().as_str() {
        "white" => Some([255; 3]),
        "black" => Some([0; 3]),
        hex => {
            let hex = hex
                .strip_prefix('#')
                .filter(|h| h.len() == 6 && h.is_ascii())?;
            let byte = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).ok();
            Some([byte(0)?, byte(2)?, byte(4)?])
        }
    }
}

fn main() -> ExitCode {
    let mut args: Vec<OsString> = std::env::args_os().collect();
    // Only an exact first argument is a command: `./formats` is a file.
    match args.get(1).and_then(|a| a.to_str()) {
        Some("convert") => drop(args.remove(1)),
        Some("formats") => {
            let cli = Formats::parse_from(&args[1..]);
            let mut out = Out::new(io::stdout().lock(), Look::stdout());
            return finish(formats(cli.json, &mut out).map(|()| 0));
        }
        _ => {}
    }
    let json = args
        .iter()
        .skip(1)
        .take_while(|a| *a != "--")
        .any(|a| a == "--json");
    let parsed = Cli::command()
        .after_help(format!("{READS}\n\n{EXAMPLES}"))
        .try_get_matches_from(&args)
        .and_then(|m| Cli::from_arg_matches(&m));
    let cli = match parsed {
        Ok(cli) => cli,
        Err(e) if json && e.use_stderr() => return fail(&Error::usage(clap_message(&e)), true),
        Err(e) => e.exit(),
    };
    let mut options = match cli.options() {
        Ok(options) => options,
        Err(e) => return fail(&e, cli.json),
    };
    if cli.paths.is_empty() {
        return fail(&Error::no_input(), cli.json);
    }
    let target = match cli.target(&mut options) {
        Ok(target) => target,
        Err(e) => return fail(&e, cli.json),
    };
    let skip = match &target {
        Target::Dir(dir) => Some(dir.as_path()),
        _ => None,
    };
    let inputs = paths::expand(&cli.paths, cli.recursive, skip);
    if matches!(target, Target::File(_) | Target::Stdout) && inputs.len() != 1 {
        return fail(&Error::output_needs_one_input(inputs.len()), cli.json);
    }
    // With `-o -` the image goes to stdout, so the text goes to stderr.
    let mut out = if target == Target::Stdout {
        Out::new(Box::new(io::stderr()) as Box<dyn Write>, Look::stderr())
    } else {
        Out::new(
            Box::new(io::stdout().lock()) as Box<dyn Write>,
            Look::stdout(),
        )
    };
    let mut err = Out::new(io::stderr(), Look::stderr());
    let run = convert(&cli, &options, &target, &inputs, &mut out, &mut err);
    finish(run)
}

/// The exit code of a run: a reader that hung up on stdout or stderr ends it quietly with 0,
/// any other write error is `io`.
fn finish(run: io::Result<u8>) -> ExitCode {
    match run {
        Ok(code) => ExitCode::from(code),
        Err(e) if e.kind() == io::ErrorKind::BrokenPipe => ExitCode::SUCCESS,
        Err(e) => fail(&Error::io(e), false),
    }
}

/// Converts every input in order, printing text per file as it finishes or one JSON object at
/// the end, and gives the exit code.
fn convert(
    cli: &Cli,
    options: &Options,
    target: &Target,
    inputs: &[Input],
    out: &mut Out<impl Write>,
    err: &mut Out<impl Write>,
) -> io::Result<u8> {
    let mut files = vec![];
    for (n, input) in (1..).zip(inputs) {
        let name = input.path.file_name().unwrap_or(input.path.as_os_str());
        let name = name.to_string_lossy();
        let label = match inputs.len() {
            1 => format!("Converting {name}"),
            all => format!("Converting {n} of {all} {} {name}", err.look.pick("·", "-")),
        };
        let file = text::spin(err, &label, || {
            convert_one(input, options, target, cli.overwrite)
        })?;
        if cli.debug {
            text::debug(err, &file)?;
        }
        if !cli.json {
            text::file(out, err, &file)?;
        }
        files.push(file);
    }
    let report = Report::new(&files);
    if cli.json {
        serde_json::to_writer(&mut out.w, &report)?;
        writeln!(out.w)?;
    } else if files.len() > 1 {
        text::summary(out, &report)?;
    }
    out.w.flush()?;
    Ok(report.exit_code())
}

/// Reads, converts and writes one input; only a failed write to stdout is an `Err`.
fn convert_one(
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
    let (mut file, bytes) = engine::convert(input.name(), &data, options);
    file.steps.insert(0, ("read", read));
    let Some(bytes) = bytes else {
        return Ok(file);
    };
    let start = Instant::now();
    let ext = match file.format {
        Some("jpeg") => "jpg",
        format => format.unwrap_or_default(),
    };
    match input.output(target, ext, overwrite) {
        Ok(Some(path)) => match engine::write(&path, &bytes) {
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

/// One `formats --json` row.
#[derive(Serialize)]
struct Row<'a> {
    format: String,
    status: &'a str,
    decoder: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    hint: Option<&'a str>,
}

/// `image-convert formats`: each input format and whether this machine decodes it.
fn formats(json: bool, out: &mut Out<impl Write>) -> io::Result<()> {
    let rows = os::formats();
    if json {
        let rows: Vec<_> = rows
            .iter()
            .map(|r| Row {
                format: r.format.name(),
                status: r.status(),
                decoder: r.decoder,
                message: r.missing.as_ref().map(|e| e.message.as_str()),
                hint: r.missing.as_ref().and_then(|e| e.hint),
            })
            .collect();
        serde_json::to_writer(&mut out.w, &serde_json::json!({ "formats": rows }))?;
        writeln!(out.w)?;
    } else {
        text::formats(out, &rows)?;
    }
    out.w.flush()
}

/// Reports an error that stops the run, as two lines on stderr or as JSON on stdout, and
/// exits with its code.
fn fail(e: &Error, json: bool) -> ExitCode {
    let _ = if json {
        writeln!(io::stdout(), "{}", e.to_json())
    } else {
        text::error(
            &mut Out::new(io::stderr(), Look::stderr()),
            &e.message,
            e.hint,
        )
    };
    ExitCode::from(e.exit_code())
}

/// clap's message as a sentence: "unexpected argument '--x' found" -> "Unexpected argument
/// '--x' found."
fn clap_message(e: &clap::Error) -> String {
    let text = e.render().to_string();
    let line = text.lines().next().unwrap_or_default();
    let line = line.strip_prefix("error: ").unwrap_or(line);
    let mut chars = line.chars();
    let first = chars
        .next()
        .map(|c| c.to_uppercase().to_string())
        .unwrap_or_default();
    format!("{first}{}.", chars.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn byte_sizes_take_decimal_and_binary_units_within_range() {
        assert_eq!(byte_size("500KB"), Some(500_000));
        assert_eq!(byte_size("2mb"), Some(2_000_000));
        assert_eq!(byte_size("3MiB"), Some(3 << 20));
        assert_eq!(byte_size("1KiB"), Some(1024));
        assert_eq!(byte_size("1000"), Some(1000));
        assert_eq!(byte_size("1024MiB"), Some(1 << 30));
        for bad in [
            "999",
            "1025MiB",
            "KB",
            "1.5MB",
            "5 KB",
            "-5KB",
            "5GB",
            "",
            "99999999999999999999",
        ] {
            assert_eq!(byte_size(bad), None, "{bad}");
        }
    }

    #[test]
    fn colours_are_names_or_hex() {
        assert_eq!(colour("White"), Some([255; 3]));
        assert_eq!(colour("black"), Some([0; 3]));
        assert_eq!(colour("#1A2b3C"), Some([0x1a, 0x2b, 0x3c]));
        for bad in ["#12345", "#1234567", "red", "123456", "#12345g", "#ééé"] {
            assert_eq!(colour(bad), None, "{bad}");
        }
    }
}
