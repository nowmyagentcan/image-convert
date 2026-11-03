//! The command line: `image-convert [convert] <PATH>... [OPTIONS]`, `image-convert formats`,
//! `image-convert ui`, `image-convert mcp`, and interactive mode when started with no arguments in a terminal.

mod interactive;
mod mcp;
mod text;
mod ui;

use std::ffi::OsString;
use std::io::{self, IsTerminal, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{ArgAction, CommandFactory, FromArgMatches, Parser};
use image_convert::engine::{self, Report};
use image_convert::error::Error;
use image_convert::paths::{self, Input, Target};
use image_convert::{Choices, Format, Options, os};
use serde::Serialize;
use text::{Look, Out};

const HELP: &str = "\
{about}

{usage-heading} {usage}

Commands:
  convert  Convert files and folders (the default command)
  formats  Show which formats this machine can decode
  ui       Open the local web page
  mcp      Run the MCP server on stdio

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

/// `image-convert ui`
#[derive(Parser)]
#[command(
    name = "image-convert ui",
    about = "Open the local web page.",
    after_help = EXAMPLES,
    disable_version_flag = true
)]
struct Ui {
    /// port to listen on [default: a free one]
    #[arg(long, value_name = "N", default_value_t = 0, hide_default_value = true)]
    port: u16,
    /// don't open the browser
    #[arg(long)]
    no_open: bool,
}

/// `image-convert mcp`
#[derive(Parser)]
#[command(
    name = "image-convert mcp",
    about = "Run the MCP server on stdio.",
    after_help = EXAMPLES,
    disable_version_flag = true
)]
struct Mcp {}

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
        Choices {
            to: self.to.clone(),
            fit: self.fit.clone(),
            max_side: self.max_side.clone(),
            max_megapixels: self.max_megapixels.clone(),
            max_bytes: self.max_bytes.clone(),
            width: self.width.clone(),
            height: self.height.clone(),
            quality: self.quality.clone(),
            background: self.background.clone(),
            keep_exif: self.keep_exif,
        }
        .options()
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

fn main() -> ExitCode {
    let mut args: Vec<OsString> = std::env::args_os().collect();
    if args.len() == 1 && double_clicked() {
        let stop = format!(
            " {} close this window to stop.",
            Look::stdout().pick("·", "-")
        );
        return served(ui::serve(0, true, &stop));
    }
    // Only an exact first argument is a command: `./formats` is a file.
    match args.get(1).and_then(|a| a.to_str()) {
        Some("convert") => drop(args.remove(1)),
        Some("formats") => {
            let cli: Formats = match parse(Formats::command(), &args[1..]) {
                Ok(cli) => cli,
                Err(code) => return code,
            };
            let mut out = Out::new(io::stdout().lock(), Look::stdout());
            return finish(formats(cli.json, &mut out).map(|()| 0));
        }
        Some("ui") => {
            let ui = Ui::parse_from(&args[1..]);
            return served(ui::serve(ui.port, !ui.no_open, "  (Ctrl+C to stop)"));
        }
        Some("mcp") => {
            Mcp::parse_from(&args[1..]);
            return served(mcp::serve());
        }
        _ => {}
    }
    if args.len() == 1 && io::stdin().is_terminal() && io::stdout().is_terminal() {
        let mut out = Out::new(io::stdout().lock(), Look::stdout());
        let mut err = Out::new(io::stderr(), Look::stderr());
        let convert = |paths: Vec<PathBuf>, out: &mut _, err: &mut _| {
            let inputs = paths::expand(&paths, false, None);
            let cli = Cli::default();
            let options = Options::default();
            convert(&cli, &options, &Target::Beside, &inputs, out, err).map(drop)
        };
        let run = interactive::run(io::stdin().lock(), &mut out, &mut err, convert);
        return finish(run.map(|()| 0));
    }
    let command = Cli::command().after_help(format!("{READS}\n\n{EXAMPLES}"));
    let cli: Cli = match parse(command, &args) {
        Ok(cli) => cli,
        Err(code) => return code,
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
            engine::convert_input(input, options, target, cli.overwrite)
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

/// Whether Explorer started this process: it then has a console to itself, while a terminal
/// shares its console with the shell.
#[cfg(windows)]
fn double_clicked() -> bool {
    use windows::Win32::System::Console::GetConsoleProcessList;
    let mut ids = [0u32; 2];
    // SAFETY: the call writes at most `ids.len()` process ids into `ids`.
    unsafe { GetConsoleProcessList(&mut ids) == 1 }
}

#[cfg(not(windows))]
fn double_clicked() -> bool {
    false
}

/// How a server command ended: quietly, or with its error on stderr.
fn served(result: Result<(), Error>) -> ExitCode {
    result.map_or_else(|e| fail(&e, false), |()| ExitCode::SUCCESS)
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

/// Parses `args` with `command`; a usage error is a JSON object when `--json` comes before any
/// `--`, else clap's own text.
fn parse<T: FromArgMatches>(command: clap::Command, args: &[OsString]) -> Result<T, ExitCode> {
    let json = args
        .iter()
        .skip(1)
        .take_while(|a| *a != "--")
        .any(|a| a == "--json");
    match command
        .try_get_matches_from(args)
        .and_then(|m| T::from_arg_matches(&m))
    {
        Ok(parsed) => Ok(parsed),
        Err(e) if json && e.use_stderr() => Err(fail(&Error::usage(clap_message(&e)), true)),
        Err(e) => e.exit(),
    }
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
