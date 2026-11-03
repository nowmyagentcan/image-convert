//! The MCP server on stdio: `view_image` hands a model the converted image itself, and
//! `convert_image` saves files and answers with the CLI's `--json` object.

use std::path::PathBuf;
use std::sync::Arc;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use image_convert::Choices;
use image_convert::engine::{self, FileResult, Report};
use image_convert::error::Error;
use image_convert::paths::{self, Input, Target};
use rmcp::model::{CallToolResult, ContentBlock, JsonObject};
use rmcp::service::ServerInitializeError;
use rmcp::{ServerHandler, ServiceExt, tool, tool_handler, tool_router};
use schemars::generate::SchemaSettings;
use schemars::transform::RecursiveTransform;
use schemars::{JsonSchema, Schema};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ViewArgs {
    /// An image file; relative paths resolve against the server's working directory
    path: String,
    /// vision: longest side 1568, 1.15 megapixels; vision-hd: 2048, 2.4 megapixels
    #[schemars(extend("enum" = ["vision", "vision-hd", null], "default" = "vision"))]
    fit: Option<String>,
    /// Largest image to return, in bytes
    #[schemars(range(min = 10_000, max = 7_500_000), extend("default" = 1_000_000))]
    max_bytes: Option<i64>,
    /// Width to render SVG, EMF and WMF at; photos only shrink to fit it
    #[schemars(range(min = 1, max = 16384))]
    width: Option<i64>,
    /// Height to render SVG, EMF and WMF at; photos only shrink to fit it
    #[schemars(range(min = 1, max = 16384))]
    height: Option<i64>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ConvertArgs {
    /// Image files or folders; relative paths resolve against the server's working directory
    #[schemars(length(min = 1, max = 1000))]
    paths: Vec<String>,
    /// Output format; auto keeps PNG for lossless sources and transparency, else JPEG
    #[schemars(extend("enum" = ["auto", "png", "jpeg", "webp", null], "default" = "auto"))]
    to: Option<String>,
    /// Size preset: vision (longest side 1568, 1.15 megapixels) or vision-hd (2048, 2.4)
    #[schemars(extend("enum" = ["none", "vision", "vision-hd", null], "default" = "none"))]
    fit: Option<String>,
    /// Longest side in pixels
    #[schemars(range(min = 1, max = 65535))]
    max_side: Option<i64>,
    /// Width × height in millions
    #[schemars(range(min = 0.01, max = 200))]
    max_megapixels: Option<f64>,
    /// Largest output file in bytes
    #[schemars(range(min = 1000, max = 1_073_741_824))]
    max_bytes: Option<i64>,
    /// Fit inside this width (SVG, EMF, WMF: render at it)
    #[schemars(range(min = 1, max = 65535))]
    width: Option<i64>,
    /// Fit inside this height (SVG, EMF, WMF: render at it)
    #[schemars(range(min = 1, max = 65535))]
    height: Option<i64>,
    /// JPEG and WebP quality; left out, it may be lowered to fit max_bytes
    #[schemars(range(min = 1, max = 100), extend("default" = 85))]
    quality: Option<i64>,
    /// Fill behind transparency in JPEG: white, black or #rrggbb
    #[schemars(extend("default" = "white"))]
    background: Option<String>,
    /// Copy the EXIF block into JPEG and PNG output
    #[serde(default)]
    keep_exif: bool,
    /// Folder to write every output into, made as needed
    output_dir: Option<String>,
    /// Convert folders inside folders too
    #[serde(default)]
    recursive: bool,
}

#[derive(Clone)]
struct Server;

#[tool_router]
impl Server {
    #[tool(
        description = "Look at a local image file in any format, including ones you can't open yourself: HEIC/HEIF, AVIF, JPEG XL, SVG, TIFF, camera RAW (CR2, CR3, NEF, ARW, DNG, RAF, RW2, PEF; the camera's embedded preview), EMF/WMF (Windows only), and PNG, JPEG, GIF, WebP, BMP, ICO. Applies orientation, converts colour to sRGB, shrinks it to fit vision-model limits and returns it as an image plus a JSON line with its sizes and warnings. Writes nothing. On failure the result has isError true and one text block holding {\"error\":{\"code\",\"message\",\"hint\"}}.",
        input_schema = input::<ViewArgs>(),
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = false)
    )]
    fn view_image(&self, args: JsonObject) -> CallToolResult {
        answer(parse(args).and_then(view))
    }

    #[tool(
        description = "Convert local image files to PNG, JPEG or WebP and save them: HEIC/HEIF, AVIF, JPEG XL, SVG, TIFF, camera RAW (embedded preview), EMF/WMF (Windows only), PNG, JPEG, GIF, WebP, BMP, ICO. Applies orientation and sRGB colour, removes metadata unless keep_exif, and can shrink to vision-model limits (fit \"vision\"). Each output goes next to its input (folders: into a \"converted\" subfolder) unless output_dir is given; existing files are never overwritten. Returns each file's output path, size and warnings; a file that fails gets its own error entry; on invalid arguments the result has isError true and one text block holding {\"error\":{\"code\",\"message\",\"hint\"}}.",
        input_schema = input::<ConvertArgs>(),
        output_schema = schema::<Report>(SchemaSettings::draft2020_12().for_serialize()),
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    fn convert_image(&self, args: JsonObject) -> CallToolResult {
        answer(parse(args).and_then(convert))
    }
}

#[tool_handler(
    name = "image-convert",
    instructions = "Convert local images that you can't open (HEIC/HEIF, AVIF, JPEG XL, SVG, TIFF, camera RAW, EMF/WMF on Windows) to PNG, JPEG or WebP, sized for vision models. Use view_image to look at any image file, including HEIC, SVG and RAW. Use convert_image to save converted files. Pass absolute paths."
)]
impl ServerHandler for Server {}

/// The arguments, refusing names the tool doesn't define.
fn parse<T: DeserializeOwned>(args: JsonObject) -> Result<T, Error> {
    serde_json::from_value(Value::Object(args))
        .map_err(|e| bad_argument(format!("Can't read the arguments: {e}.")))
}

/// The tool's result, or the error object as an error result. Errors carry no structured
/// content, so clients don't check them against the output schema.
fn answer(result: Result<CallToolResult, Error>) -> CallToolResult {
    result.unwrap_or_else(|e| CallToolResult::error(vec![ContentBlock::text(e.to_json())]))
}

fn bad_argument(message: String) -> Error {
    Error::new("usage", message)
}

/// A number argument as the text option checks take.
fn text(n: Option<impl ToString>) -> Option<String> {
    n.map(|n| n.to_string())
}

/// The image as one image block and its file object (output null) as one text block.
fn view(args: ViewArgs) -> Result<CallToolResult, Error> {
    let fit = args.fit.unwrap_or_else(|| "vision".into());
    if !["vision", "vision-hd"].contains(&fit.as_str()) {
        return Err(bad_argument(format!(
            "Unknown preset '{fit}'; view_image takes vision or vision-hd."
        )));
    }
    let max_bytes = args.max_bytes.unwrap_or(1_000_000);
    if !(10_000..=7_500_000).contains(&max_bytes) {
        return Err(bad_argument(
            "max_bytes must be between 10000 and 7500000.".into(),
        ));
    }
    for (name, side) in [("width", args.width), ("height", args.height)] {
        if side.is_some_and(|s| !(1..=16384).contains(&s)) {
            return Err(bad_argument(format!("{name} must be between 1 and 16384.")));
        }
    }
    let options = Choices {
        fit: Some(fit),
        max_bytes: text(Some(max_bytes)),
        width: text(args.width),
        height: text(args.height),
        ..Choices::default()
    }
    .options()?;
    let input = paths::file(args.path.into());
    let (file, bytes) = engine::convert(input.name(), &input.load()?, &options);
    let (Some(bytes), Some(format)) = (bytes, file.format) else {
        return Err(file.error.expect("a file without output failed"));
    };
    let json = serde_json::to_string(&file).expect("results are plain data");
    Ok(CallToolResult::success(vec![
        ContentBlock::image(STANDARD.encode(bytes), format!("image/{format}")),
        ContentBlock::text(json),
    ]))
}

/// Converts and writes every path as the CLI does, answering with its `--json` object.
fn convert(args: ConvertArgs) -> Result<CallToolResult, Error> {
    if args.paths.is_empty() {
        return Err(bad_argument("Pass at least one path.".into()));
    }
    if args.paths.len() > 1000 {
        return Err(bad_argument("Pass at most 1000 paths per call.".into()));
    }
    let options = Choices {
        to: args.to,
        fit: args.fit,
        max_side: text(args.max_side),
        max_megapixels: text(args.max_megapixels),
        max_bytes: text(args.max_bytes),
        width: text(args.width),
        height: text(args.height),
        quality: text(args.quality),
        background: args.background,
        keep_exif: args.keep_exif,
    }
    .options()?;
    let dir = args.output_dir.map(PathBuf::from);
    let target = dir.clone().map_or(Target::Beside, Target::Dir);
    let paths: Vec<PathBuf> = args.paths.into_iter().map(PathBuf::from).collect();
    let files: Vec<FileResult> = paths::expand(&paths, args.recursive, dir.as_deref())
        .into_iter()
        .map(Input::file_only)
        .map(|input| {
            engine::convert_input(&input, &options, &target, false)
                .unwrap_or_else(|e| FileResult::failed(input.name(), Error::io(e)))
        })
        .collect();
    let report = serde_json::to_value(Report::new(&files)).expect("results are plain data");
    Ok(CallToolResult::structured(report))
}

/// A tool's arguments, closed to names it doesn't define.
fn input<T: JsonSchema>() -> Arc<JsonObject> {
    let mut schema = schema::<T>(SchemaSettings::draft2020_12());
    Arc::make_mut(&mut schema).insert("additionalProperties".into(), false.into());
    schema
}

/// A schema with an object at its root, as 2025-11-25 clients require, and without the Rust
/// number formats schemars adds (uint32, int64), which JSON Schema doesn't define.
fn schema<T: JsonSchema>(settings: SchemaSettings) -> Arc<JsonObject> {
    let no_format = |s: &mut Schema| drop(s.remove("format"));
    let schema = settings
        .with_transform(RecursiveTransform(no_format))
        .into_generator()
        .into_root_schema_for::<T>();
    let Value::Object(mut schema) = schema.to_value() else {
        unreachable!("a root schema is an object")
    };
    schema.remove("title");
    schema.remove("description");
    schema.insert("type".into(), "object".into());
    Arc::new(schema)
}

/// Serves until the client closes stdin. Calls run one at a time on this thread; the engine
/// decodes each file on its own worker.
pub fn serve() -> Result<(), Error> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(Error::io)?;
    runtime.block_on(async {
        match Server.serve(rmcp::transport::stdio()).await {
            Ok(service) => service.waiting().await.map(drop).map_err(Error::io),
            // stdin closed before the handshake ended: the client left, which is no failure.
            Err(ServerInitializeError::ConnectionClosed(_)) => Ok(()),
            // stdout closed before the answer went out: the client left too.
            Err(ServerInitializeError::TransportError { error, .. })
                if error
                    .error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|e| e.kind() == std::io::ErrorKind::BrokenPipe) =>
            {
                Ok(())
            }
            Err(e) => Err(Error::io(e)),
        }
    })
}
