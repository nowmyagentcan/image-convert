# image-convert

Convert HEIC, AVIF, JPEG XL, SVG, TIFF, camera RAW and other images to PNG, JPEG or WebP: a command line tool with JSON output, a local web page, and an MCP server that lets AI agents look at any image. Everything runs on your computer.

## Install

Windows (PowerShell, no admin; installs to `%LOCALAPPDATA%\Programs\image-convert` and adds it to your user PATH):

```powershell
irm https://github.com/nowmyagentcan/image-convert/releases/latest/download/install.ps1 | iex
```

macOS and Linux (installs to `~/.local/bin`, with the licence files in `~/.local/share/doc/image-convert`; Linux needs glibc 2.35 or newer, so Ubuntu 22.04, Debian 12, Fedora 36 or later):

```sh
curl -fsSL https://github.com/nowmyagentcan/image-convert/releases/latest/download/install.sh | sh
```

Both scripts check the archive's SHA-256 against `SHA256SUMS` before installing anything. Or download an archive yourself; each holds one `image-convert` binary that needs no runtime:

| System | Download |
|---|---|
| Windows x64 | [image-convert-v0.1.0-windows-x64.zip](https://github.com/nowmyagentcan/image-convert/releases/download/v0.1.0/image-convert-v0.1.0-windows-x64.zip) |
| macOS Apple silicon | [image-convert-v0.1.0-macos-arm64.tar.gz](https://github.com/nowmyagentcan/image-convert/releases/download/v0.1.0/image-convert-v0.1.0-macos-arm64.tar.gz) |
| macOS Intel | [image-convert-v0.1.0-macos-x64.tar.gz](https://github.com/nowmyagentcan/image-convert/releases/download/v0.1.0/image-convert-v0.1.0-macos-x64.tar.gz) |
| Linux x64 | [image-convert-v0.1.0-linux-x64.tar.gz](https://github.com/nowmyagentcan/image-convert/releases/download/v0.1.0/image-convert-v0.1.0-linux-x64.tar.gz) |
| Linux arm64 | [image-convert-v0.1.0-linux-arm64.tar.gz](https://github.com/nowmyagentcan/image-convert/releases/download/v0.1.0/image-convert-v0.1.0-linux-arm64.tar.gz) |

With Rust and a C compiler: `cargo install --git https://github.com/nowmyagentcan/image-convert --tag v0.1.0 --locked`.
To remove it, delete the binary and `~/.local/share/doc/image-convert` (on Windows, the `image-convert` folder above).

## Use

```sh
image-convert IMG_0042.HEIC                        # writes IMG_0042.jpg beside it
image-convert logo.svg --width 1024 -o logo.png    # render a drawing at a size
image-convert photos/ -r --to jpeg                 # a folder, into photos/converted/
image-convert DSC_0001.NEF --fit vision            # the camera's preview, sized for AI
cat photo.heic | image-convert - -o photo.jpg      # stdin
image-convert formats                              # what this machine can decode
image-convert                                      # interactive: drop files, /help, /quit
```

```text
$ image-convert IMG_0042.avif logo.svg DSC_0001.NEF --fit vision
✓ IMG_0042.avif → IMG_0042.jpg  1238×928 JPEG  142 KB · from 4032×3024
✓ logo.svg → logo.png  1516×758 PNG  20 KB · from 256×128
✓ DSC_0001.NEF → DSC_0001.jpg  1313×875 JPEG  223 KB · from 5568×3712
! DSC_0001.NEF: Converted the camera's embedded preview (5568×3712), not a full RAW development.
3 converted
```

- Outputs go beside each input (folders into a `converted` subfolder) and never replace a file: when `photo.jpg` exists, the output is `photo-1.jpg`. `-o` names a file or folder; `--overwrite` replaces an `-o` file.
- Every output is upright and sRGB, with no metadata unless `--keep-exif`. `--to auto` (the default) keeps PNG for lossless sources and transparency, else JPEG.
- Sizing: `--fit vision` (longest side 1568, 1.15 megapixels) or `--fit vision-hd` (2048, 2.4), `--max-side`, `--max-megapixels`, `--width`, `--height`, and `--max-bytes 300KB`, which changes the format, lowers the quality or shrinks the image until the file fits, with a warning for each.
- Exit codes: 0 every file converted, 1 a file failed (the others are still converted), 2 usage, 3 runtime.

**The page:** `image-convert ui` opens a small page in your browser, served on `127.0.0.1` only. Drop images, choose them, or paste one with Ctrl+V; each result has a preview and Save. Nothing is written to disk. On Windows, double-clicking `image-convert.exe` opens the same page.

### Use with AI agents

Agents can run the CLI with `--json` (one JSON object on stdout; errors as `{"error":{"code","message","hint"}}`), then open the file it wrote:

```sh
image-convert photo.heic --fit vision --json
image-convert diagram.svg --width 1600 --json
image-convert scans/ -r --fit vision -o out/ --json
```

```json
{"files":[{"input":"IMG_0042.avif","output":"IMG_0042.jpg","input_format":"avif","source":{"width":4032,"height":3024,"bit_depth":10,"alpha":false,"frames":1,"orientation":1,"color":"srgb","profile":null,"decoder":"wic"},"format":"jpeg","width":1238,"height":928,"bytes":141829,"quality":85,"resized":true,"exif":false,"warnings":[],"error":null}],"converted":1,"failed":0}
```

Or add the local MCP server (stdio), which has two tools:

- `view_image` returns the image itself, converted and shrunk to fit vision-model limits (at most 1,000,000 bytes by default), plus a JSON line with its sizes and warnings. It writes nothing, so a model can look at a HEIC, SVG or RAW file in one call. Arguments: `path`, and optionally `fit` (`vision` or `vision-hd`), `max_bytes`, `width`, `height`.
- `convert_image` saves files as the CLI does and returns its `--json` object as `structuredContent`. Arguments: `paths` (files or folders; absolute paths are safest), and optionally `to`, `fit`, `max_side`, `max_megapixels`, `max_bytes`, `width`, `height`, `quality`, `background`, `keep_exif`, `output_dir`, `recursive`.

| Agent | Setup |
|---|---|
| Claude Code | `claude mcp add image-convert -- image-convert mcp` |
| Codex | `codex mcp add image-convert -- image-convert mcp` |
| Cursor (`~/.cursor/mcp.json`), Claude Desktop | `{"mcpServers":{"image-convert":{"command":"image-convert","args":["mcp"]}}}` |
| VS Code (`.vscode/mcp.json`) | `{"servers":{"image-convert":{"type":"stdio","command":"image-convert","args":["mcp"]}}}` |

Invalid arguments and files that can't be converted with `view_image` return `isError: true` with one text block holding the error object. Agents that save a pasted image to a file can pass its path.

## Supported inputs and limits

- **Built in, on every system:** PNG (APNG first frame), JPEG (baseline, progressive, grey, CMYK and YCCK), GIF and WebP (first frame), BMP, ICO (largest image), TIFF (first page; uncompressed, LZW, Deflate, PackBits, JPEG and Group 4 fax), JPEG XL, SVG and SVGZ (with a built-in font; images linked from outside the file are left out with a warning), and camera RAW from Canon (CR2, CR3), Nikon (NEF), Sony (ARW), Adobe DNG, Fujifilm (RAF), Panasonic (RW2) and Pentax (PEF).
- **RAW:** the camera's embedded JPEG preview is converted, not a full RAW development; the result says so.
- **HEIC/HEIF and AVIF** use the system's decoder, so nothing patent-encumbered ships in the binary. `image-convert formats` shows what this machine can decode and how to add what's missing.
  - Windows: the free HEIF Image Extensions plus HEVC Video Extensions (HEIC) or AV1 Video Extension (AVIF) from the Microsoft Store; the error message links the Store page.
  - macOS: built in (AVIF needs macOS 13).
  - Linux: the distribution's libheif 1.13 or newer, e.g. `sudo apt install libheif1 libheif-plugin-libde265 libheif-plugin-dav1d`.
- **EMF, EMZ and placeable WMF:** Windows only, drawn by GDI+.
- **Limits:** 512 MiB per file; images up to 200 megapixels and 65,535 pixels per side; drawings render up to 16,384 pixels per side.
- **Not read:** PSD, JPEG 2000, PDF, PNM, QOI, OpenEXR, DICOM and RAW formats not listed above (each gets its own error and a hint); TIFF and JPEG variants named in the error, such as 12-bit or arithmetic-coded JPEG. Animations and multi-page files give their first frame or page, with a warning.
- It makes no network requests; `ui` only listens on `127.0.0.1`.

Known limitations in v0.1.0:

- HEIC through Windows was tested only for its missing-decoder message: no test machine had the HEVC extension. AVIF through Windows was tested on Windows 11 with the AV1 extension.
- HDR HEIC and AVIF (PQ or HLG) come out differently by system: Windows and macOS tone-map them in the system's decoder, while Linux maps reference white to sRGB white without tone mapping and warns `hdr_clipped`, so one file can look different on each.
- A 12 MP JPEG XL needs about 300 MB of memory while it decodes, and a 12 MP AVIF through Windows about 380 MB (other photos of that size need under 200 MB).
- A damaged AVIF can make the system's AV1 decoder print its own messages to stderr; stdout and `--json` are unaffected.

## How it works

Each file is identified by its bytes, decoded to pixels ([zune-jpeg](https://github.com/etemesi254/zune-image) and [jpeg-decoder](https://github.com/image-rs/jpeg-decoder), [image](https://github.com/image-rs/image), [tiff](https://github.com/image-rs/image-tiff), [jxl-oxide](https://github.com/tirr-c/jxl-oxide), [resvg](https://github.com/linebender/resvg) with the Noto Sans font, or the system's decoder: Windows Imaging Component and GDI+, macOS ImageIO, Linux libheif), turned to sRGB by [moxcms](https://github.com/awxkee/moxcms), resized by [fast_image_resize](https://github.com/Cykooz/fast_image_resize) and made upright.
It is written as PNG by [image](https://github.com/image-rs/image), JPEG by [jpeg-encoder](https://github.com/vstroebel/jpeg-encoder) or WebP by [libwebp](https://chromium.googlesource.com/webm/libwebp). This software is based in part on the work of the Independent JPEG Group.
The CLI uses [clap](https://github.com/clap-rs/clap) and the MCP server the [official Rust SDK](https://github.com/modelcontextprotocol/rust-sdk).

## First run on an unsigned build

Windows: if "Windows protected your PC" appears, choose More info, then Run anyway.
The install script doesn't trigger it; a zip extracted in Explorer may.

macOS: if it says the developer can't be verified, open System Settings, Privacy & Security, and choose Open Anyway.
The install script doesn't trigger it, because curl downloads carry no quarantine flag.

## Licence

Apache-2.0, see [LICENSE](LICENSE). Each release archive includes `THIRD-PARTY-LICENSES.txt` for the bundled dependencies, including libwebp's licence and patent grant and the Noto Sans font licence; the install scripts install both files with the binary.

---

[nowmyagentcan.com](https://nowmyagentcan.com) · [x.com/chayp05](https://x.com/chayp05)
