//! Tells the input type from the file's first bytes (and, for a few RAW kinds, its extension).

use std::path::Path;

use crate::error::Error;
use crate::tiff::Tiff;

/// An input type we read. `Gzip` is inflated and detected again, so it never reaches a result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Png,
    Jpeg,
    Gif,
    Bmp,
    Ico,
    Webp,
    Tiff,
    Jxl,
    Heic,
    Avif,
    Svg,
    Emf,
    Wmf,
    Cr2,
    Cr3,
    Nef,
    Arw,
    Dng,
    Raf,
    Rw2,
    Pef,
    Gzip,
}

impl Kind {
    /// The `input_format` value in results: the variant's name in lower case.
    pub fn name(self) -> String {
        format!("{self:?}").to_ascii_lowercase()
    }

    /// The format's name in messages ("JPEG XL", "WebP").
    pub fn label(self) -> String {
        match self {
            Kind::Webp => "WebP".into(),
            Kind::Jxl => "JPEG XL".into(),
            Kind::Heic => "HEIF".into(),
            _ => format!("{self:?}").to_ascii_uppercase(),
        }
    }
}

/// RAW kinds refused by extension whatever the content.
const RAW_BY_EXTENSION: [(&str, &str); 14] = [
    ("nrw", "Nikon NRW"),
    ("srw", "Samsung SRW"),
    ("3fr", "Hasselblad 3FR"),
    ("fff", "Hasselblad FFF"),
    ("iiq", "Phase One IIQ"),
    ("erf", "Epson ERF"),
    ("mef", "Mamiya MEF"),
    ("mos", "Leaf MOS"),
    ("kdc", "Kodak KDC"),
    ("dcr", "Kodak DCR"),
    ("srf", "Sony SRF"),
    ("sr2", "Sony SR2"),
    ("rwl", "Leica RWL"),
    ("gpr", "GoPro GPR"),
];

const HEIF_BRANDS: [&[u8; 4]; 11] = [
    b"heic", b"heix", b"hevc", b"hevx", b"heim", b"heis", b"mif1", b"msf1", b"miaf", b"avif",
    b"avis",
];

/// The type of `data`, read from at most its first 4096 bytes (IFD0 for DNG may lie further in).
/// `name` gives the extension that NEF, ARW, PEF and the refused RAW kinds need.
pub fn detect(data: &[u8], name: &Path) -> Result<Kind, Error> {
    if data.is_empty() {
        return Err(Error::empty_file());
    }
    let ext = name
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if let Some((_, kind)) = RAW_BY_EXTENSION.iter().find(|(e, _)| *e == ext) {
        return Err(Error::unsupported_raw(kind));
    }
    let head = &data[..data.len().min(4096)];
    let at = |offset: usize, sig: &[u8]| head.get(offset..offset + sig.len()) == Some(sig);

    let kind = if at(0, b"\x89PNG\r\n\x1a\n") {
        Kind::Png
    } else if at(0, b"\xff\xd8\xff") {
        Kind::Jpeg
    } else if at(0, b"GIF87a") || at(0, b"GIF89a") {
        Kind::Gif
    } else if at(0, b"BM") {
        Kind::Bmp
    } else if at(0, b"\0\0\x01\0") {
        Kind::Ico
    } else if at(0, b"RIFF") && at(8, b"WEBP") {
        Kind::Webp
    } else if at(0, b"II*\0") && at(8, b"CR\x02") {
        Kind::Cr2
    } else if at(0, b"FUJIFILMCCD-RAW ") {
        Kind::Raf
    } else if at(0, b"IIU\0") {
        Kind::Rw2
    } else if at(0, b"II*\0") || at(0, b"MM\0*") {
        match ext.as_str() {
            _ if has_dng_version(data) => Kind::Dng,
            "nef" => Kind::Nef,
            "arw" => Kind::Arw,
            "pef" => Kind::Pef,
            _ => Kind::Tiff,
        }
    } else if at(0, b"II+\0") || at(0, b"MM\0+") {
        Kind::Tiff
    } else if at(0, b"\xff\x0a") || at(0, b"\0\0\0\x0cJXL \r\n\x87\n") {
        Kind::Jxl
    } else if let Some(kind) = bmff(head) {
        kind
    } else if is_svg(head) {
        Kind::Svg
    } else if at(0, b"\x1f\x8b") {
        Kind::Gzip
    } else if at(0, b"\x01\0\0\0") && at(40, b" EMF") {
        Kind::Emf
    } else if at(0, b"\xd7\xcd\xc6\x9a") || at(0, b"\x01\0\x09\0") || at(0, b"\x02\0\x09\0") {
        Kind::Wmf
    } else {
        return Err(out_of_scope(head));
    };
    Ok(kind)
}

/// The refusal for a file no matrix row claimed.
fn out_of_scope(head: &[u8]) -> Error {
    let at = |offset: usize, sig: &[u8]| head.get(offset..offset + sig.len()) == Some(sig);
    let format = if at(0, b"8BPS") {
        "PSD"
    } else if at(0, b"\0\0\0\x0cjP  ") || at(0, b"\xff\x4f\xff\x51") {
        "JPEG 2000"
    } else if at(0, b"%PDF-") {
        "PDF"
    } else if head.len() > 2
        && head[0] == b'P'
        && (b'1'..=b'7').contains(&head[1])
        && head[2].is_ascii_whitespace()
    {
        "PNM"
    } else if at(0, b"qoif") {
        "QOI"
    } else if at(0, b"DDS ") {
        "DDS"
    } else if at(0, b"\x76\x2f\x31\x01") {
        "OpenEXR"
    } else if at(0, b"#?RADIANCE") || at(0, b"#?RGBE") {
        "Radiance HDR"
    } else if at(0, b"\0\0\x02\0") {
        "Cursor (CUR)"
    } else if at(128, b"DICM") {
        "DICOM"
    } else {
        let raw = if at(0, b"II\x1a\0\0\0HEAPCCDR") {
            "Canon CRW"
        } else if at(0, b"FOVb") {
            "Sigma X3F"
        } else if at(0, b"\0MRM") {
            "Minolta MRW"
        } else if at(0, b"IIRO") || at(0, b"IIRS") || at(0, b"MMOR") {
            "Olympus ORF"
        } else {
            return Error::unknown_format();
        };
        return Error::unsupported_raw(raw);
    };
    Error::unsupported_format(format)
}

/// Whether IFD0 of a TIFF-structured file has the DNGVersion tag (0xC612).
fn has_dng_version(data: &[u8]) -> bool {
    Tiff::new(data)
        .and_then(|(tiff, first)| tiff.ifd(first))
        .is_some_and(|(ifd, _)| ifd.get(0xC612).is_some())
}

/// HEIC, AVIF or CR3 from an ISO-BMFF `ftyp` box's brands; the primary item decides HEIC/AVIF later.
fn bmff(head: &[u8]) -> Option<Kind> {
    if head.get(4..8) != Some(b"ftyp") {
        return None;
    }
    let size = u32::from_be_bytes(head[..4].try_into().unwrap()) as usize;
    // The major brand, the minor version (skipped), then the compatible brands.
    let (words, _) = head.get(8..size.min(head.len()).max(12))?.as_chunks::<4>();
    let has = |set: &[&[u8; 4]]| {
        words
            .iter()
            .enumerate()
            .any(|(i, b)| i != 1 && set.contains(&b))
    };
    if has(&[b"crx "]) {
        Some(Kind::Cr3)
    } else if has(&[b"avif", b"avis"]) {
        Some(Kind::Avif)
    } else if has(&HEIF_BRANDS) {
        Some(Kind::Heic)
    } else {
        None
    }
}

/// An optional BOM, whitespace, XML declaration, comments and DOCTYPE, then `<svg`. UTF-16 text
/// is read with its NUL bytes dropped, so it is detected here and refused by the SVG reader.
fn is_svg(head: &[u8]) -> bool {
    let boms: [&[u8]; 3] = [b"\xef\xbb\xbf", b"\xff\xfe", b"\xfe\xff"];
    let head = boms
        .iter()
        .find_map(|bom| head.strip_prefix(*bom))
        .unwrap_or(head);
    let text: Vec<u8> = head.iter().copied().filter(|&b| b != 0).collect();
    let mut rest = &text[..];
    loop {
        rest = rest.trim_ascii_start();
        let end = if rest.starts_with(b"<?") {
            find(rest, b"?>").map(|i| i + 2)
        } else if rest.starts_with(b"<!--") {
            find(rest, b"-->").map(|i| i + 3)
        } else if rest.starts_with(b"<!DOCTYPE") {
            let close = find(rest, b">");
            match find(rest, b"[") {
                Some(open) if close.is_none_or(|c| open < c) => {
                    find(rest, b"]").and_then(|i| find(&rest[i..], b">").map(|j| i + j + 1))
                }
                _ => close.map(|i| i + 1),
            }
        } else {
            return rest.starts_with(b"<svg")
                && rest
                    .get(4)
                    .is_none_or(|&b| b.is_ascii_whitespace() || b == b'>' || b == b'/');
        };
        match end {
            Some(end) => rest = &rest[end..],
            None => return false,
        }
    }
}

pub(crate) fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kind(data: &[u8], name: &str) -> Result<Kind, Error> {
        detect(data, Path::new(name))
    }

    fn padded(prefix: &[u8]) -> Vec<u8> {
        let mut data = prefix.to_vec();
        data.resize(256, 0);
        data
    }

    fn tiff_ifd0(big: bool, tags: &[u16]) -> Vec<u8> {
        let u16b = |v: u16| {
            if big {
                v.to_be_bytes()
            } else {
                v.to_le_bytes()
            }
        };
        let mut data = if big {
            b"MM\0*".to_vec()
        } else {
            b"II*\0".to_vec()
        };
        data.extend(if big {
            8u32.to_be_bytes()
        } else {
            8u32.to_le_bytes()
        });
        data.extend(u16b(tags.len() as u16));
        for &tag in tags {
            data.extend(u16b(tag));
            data.extend([0; 10]);
        }
        data.extend([0; 4]);
        data
    }

    fn ftyp(major: &[u8; 4], compatible: &[&[u8; 4]]) -> Vec<u8> {
        let mut data = ((16 + 4 * compatible.len()) as u32).to_be_bytes().to_vec();
        data.extend(b"ftyp");
        data.extend(major);
        data.extend([0; 4]);
        compatible.iter().for_each(|b| data.extend(*b));
        data.extend(b"\0\0\0\x08meta");
        data
    }

    #[test]
    fn signatures_pick_the_matrix_row() {
        let cases: [(&[u8], Kind); 15] = [
            (b"\x89PNG\r\n\x1a\n", Kind::Png),
            (b"\xff\xd8\xff\xe0", Kind::Jpeg),
            (b"GIF87a", Kind::Gif),
            (b"GIF89a", Kind::Gif),
            (b"BM", Kind::Bmp),
            (b"\0\0\x01\0\x01\0", Kind::Ico),
            (b"RIFF\0\0\0\0WEBPVP8 ", Kind::Webp),
            (b"II*\0\x10\0\0\0CR\x02\0", Kind::Cr2),
            (b"FUJIFILMCCD-RAW 0201", Kind::Raf),
            (b"IIU\0\x08\0\0\0", Kind::Rw2),
            (b"II+\0\x08\0\0\0", Kind::Tiff),
            (b"\xff\x0a\xfa", Kind::Jxl),
            (b"\0\0\0\x0cJXL \r\n\x87\n", Kind::Jxl),
            (b"\x1f\x8b\x08", Kind::Gzip),
            (b"\xd7\xcd\xc6\x9a", Kind::Wmf),
        ];
        for (prefix, expected) in cases {
            assert_eq!(kind(&padded(prefix), "x"), Ok(expected), "{prefix:?}");
        }
        let mut emf = padded(b"\x01\0\0\0");
        emf[40..44].copy_from_slice(b" EMF");
        assert_eq!(kind(&emf, "a.emf"), Ok(Kind::Emf));
        assert_eq!(kind(&padded(b"\x01\0\x09\0"), "a.wmf"), Ok(Kind::Wmf));
    }

    #[test]
    fn tiff_structures_split_by_dng_version_and_extension() {
        for big in [false, true] {
            assert_eq!(
                kind(&tiff_ifd0(big, &[256, 0xC612]), "a.tif"),
                Ok(Kind::Dng)
            );
            assert_eq!(
                kind(&tiff_ifd0(big, &[256, 0xC612]), "a.NEF"),
                Ok(Kind::Dng)
            );
            assert_eq!(kind(&tiff_ifd0(big, &[256, 257]), "a.tif"), Ok(Kind::Tiff));
            assert_eq!(kind(&tiff_ifd0(big, &[256]), "a.NEF"), Ok(Kind::Nef));
            assert_eq!(kind(&tiff_ifd0(big, &[256]), "a.Arw"), Ok(Kind::Arw));
            assert_eq!(kind(&tiff_ifd0(big, &[256]), "a.pef"), Ok(Kind::Pef));
        }
        let mut cut = tiff_ifd0(false, &[256, 0xC612]);
        cut.truncate(16);
        assert_eq!(kind(&cut, "a.tif"), Ok(Kind::Tiff));
    }

    #[test]
    fn bmff_brands_pick_heic_avif_or_cr3() {
        assert_eq!(
            kind(&ftyp(b"heic", &[b"mif1", b"heic"]), "x"),
            Ok(Kind::Heic)
        );
        assert_eq!(
            kind(&ftyp(b"mif1", &[b"mif1", b"miaf"]), "x"),
            Ok(Kind::Heic)
        );
        assert_eq!(
            kind(&ftyp(b"avif", &[b"avif", b"mif1"]), "x"),
            Ok(Kind::Avif)
        );
        assert_eq!(kind(&ftyp(b"mif1", &[b"avis"]), "x"), Ok(Kind::Avif));
        assert_eq!(
            kind(&ftyp(b"crx ", &[b"crx ", b"isom"]), "x"),
            Ok(Kind::Cr3)
        );
        assert_eq!(
            kind(&ftyp(b"isom", &[b"mp41"]), "x").unwrap_err().message,
            "This isn't an image format image-convert reads."
        );
        assert!(kind(b"\0\0\0\x18ftyphe", "x").is_err());
    }

    #[test]
    fn svg_text_after_any_prolog() {
        let svgs: [&[u8]; 7] = [
            b"<svg xmlns='http://www.w3.org/2000/svg'/>",
            b"\xef\xbb\xbf  \n<svg>",
            b"<?xml version='1.0'?>\n<!-- made by hand -->\n<svg\nwidth='1'>",
            b"<!DOCTYPE svg PUBLIC '-//W3C//DTD SVG 1.1//EN' 'x.dtd'><svg/>",
            b"<!DOCTYPE svg [ <!ENTITY a 'b>c'> ]>\n<svg>",
            b"\xff\xfe<\0s\0v\0g\0>\0",
            b"<\0?\0x\0m\0l\0?\0>\0<\0s\0v\0g\0 \0",
        ];
        for svg in svgs {
            assert_eq!(
                kind(svg, "x"),
                Ok(Kind::Svg),
                "{}",
                String::from_utf8_lossy(svg)
            );
        }
        for not_svg in [&b"<svgx>"[..], b"<html><svg>", b"<!-- open", b"hello <svg>"] {
            assert!(kind(not_svg, "x").is_err());
        }
        let mut late = vec![b' '; 4096];
        late.extend(b"<svg>");
        assert!(kind(&late, "x").is_err());
    }

    #[test]
    fn out_of_scope_formats_are_named() {
        let named: [(&[u8], &str); 13] = [
            (b"8BPS", "PSD files aren't supported."),
            (
                b"\0\0\0\x0cjP  \r\n\x87\n",
                "JPEG 2000 files aren't supported.",
            ),
            (b"\xff\x4f\xff\x51", "JPEG 2000 files aren't supported."),
            (b"%PDF-1.7", "PDF files aren't supported."),
            (b"P6\n1 1\n255\n", "PNM files aren't supported."),
            (b"qoif", "QOI files aren't supported."),
            (b"DDS ", "DDS files aren't supported."),
            (b"\x76\x2f\x31\x01", "OpenEXR files aren't supported."),
            (b"#?RADIANCE\n", "Radiance HDR files aren't supported."),
            (b"\0\0\x02\0\x01\0", "Cursor (CUR) files aren't supported."),
            (
                b"II\x1a\0\0\0HEAPCCDR",
                "Canon CRW RAW files aren't supported yet.",
            ),
            (b"FOVb", "Sigma X3F RAW files aren't supported yet."),
            (
                b"IIRO\x08\0\0\0",
                "Olympus ORF RAW files aren't supported yet.",
            ),
        ];
        for (prefix, message) in named {
            let err = kind(&padded(prefix), "x").unwrap_err();
            assert_eq!(
                (err.code, err.message.as_str()),
                ("unsupported_format", message)
            );
        }
        let mut dicom = vec![0; 128];
        dicom.extend(b"DICM");
        assert_eq!(
            kind(&dicom, "x").unwrap_err().message,
            "DICOM files aren't supported."
        );
        assert_eq!(
            kind(&padded(b"\0MRM"), "x").unwrap_err().message,
            "Minolta MRW RAW files aren't supported yet."
        );
        let pdf = kind(b"%PDF-1.7", "x").unwrap_err();
        assert_eq!(
            pdf.hint,
            Some("Render the pages to PNG with a PDF tool first.")
        );
        assert!(
            kind(b"P6x", "x")
                .unwrap_err()
                .message
                .starts_with("This isn't")
        );
    }

    #[test]
    fn refused_raw_extensions_win_over_content() {
        let tiff = tiff_ifd0(false, &[256]);
        let err = kind(&tiff, "DSC_1.NRW").unwrap_err();
        assert_eq!(err.message, "Nikon NRW RAW files aren't supported yet.");
        assert_eq!(
            err.hint,
            Some(
                "Export a JPEG from the camera's software, or use a RAW editor (darktable, RawTherapee)."
            )
        );
        assert_eq!(
            kind(b"\x89PNG\r\n\x1a\n", "x.sr2").unwrap_err().message,
            "Sony SR2 RAW files aren't supported yet."
        );
    }

    #[test]
    fn names_and_labels() {
        let kinds = [Kind::Png, Kind::Webp, Kind::Jxl, Kind::Heic, Kind::Rw2];
        let names: Vec<_> = kinds.iter().map(|k| (k.name(), k.label())).collect();
        let expected = [
            ("png", "PNG"),
            ("webp", "WebP"),
            ("jxl", "JPEG XL"),
            ("heic", "HEIF"),
            ("rw2", "RW2"),
        ];
        assert_eq!(names, expected.map(|(n, l)| (n.to_string(), l.to_string())));
    }

    #[test]
    fn empty_and_unknown_files() {
        assert_eq!(kind(b"", "a.png").unwrap_err().code, "empty_file");
        let err = kind(b"hello", "a.png").unwrap_err();
        assert_eq!(
            err.message,
            "This isn't an image format image-convert reads."
        );
        assert!(err.hint.unwrap().starts_with("Supported: PNG, JPEG"));
    }
}
