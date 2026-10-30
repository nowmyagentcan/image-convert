//! Camera RAW: the largest embedded JPEG preview, found per kind and decoded as any JPEG is.

use crate::detect::Kind;
use crate::error::{Error, Warning};
use crate::tiff::Tiff;
use crate::{Pixels, bmff, exif_orientation, jpeg};

/// The make and kind messages name a RAW file by.
fn name(kind: Kind) -> &'static str {
    match kind {
        Kind::Cr2 => "Canon CR2",
        Kind::Cr3 => "Canon CR3",
        Kind::Nef => "Nikon NEF",
        Kind::Arw => "Sony ARW",
        Kind::Dng => "Adobe DNG",
        Kind::Raf => "Fujifilm RAF",
        Kind::Rw2 => "Panasonic RW2",
        _ => "Pentax PEF",
    }
}

/// Decodes the largest JPEG preview (SOF0/1/2, at least 1 KB) of a RAW file, upright by the
/// preview's own EXIF, else the RAW's IFD0 (CR3: CMT1), else as stored.
pub fn decode(data: &[u8], kind: Kind) -> Result<Pixels, Error> {
    // The bytes at an (offset, length) pair, when both are given and lie inside the file.
    let part = |pair: Option<(u32, u32)>| {
        let (offset, len) = pair?;
        data.get(offset as usize..(offset as usize).checked_add(len as usize)?)
    };
    let (candidates, orientation) = match kind {
        Kind::Cr3 => {
            let cr3 = bmff::cr3(data)?;
            (cr3.previews, cr3.cmt1.and_then(exif_orientation))
        }
        Kind::Raf => {
            let be = |o: usize| Some(u32::from_be_bytes(data.get(o..o + 4)?.try_into().ok()?));
            (part(be(84).zip(be(88))).into_iter().collect(), None)
        }
        _ => {
            let (tiff, first) = Tiff::new(data)
                .ok_or_else(|| Error::corrupt(&kind.label(), "its header is cut short"))?;
            let ifds = tiff.ifds(first, true);
            let ifd0 = ifds.first().ok_or_else(|| {
                Error::corrupt(&kind.label(), "its first directory lies outside the file")
            })?;
            let mut found = Vec::new();
            for ifd in &ifds {
                found.extend(part(ifd.value(0x201).zip(ifd.value(0x202))));
                let one_strip = ifd.get(0x111).is_some_and(|e| e.count == 1);
                if one_strip && matches!(ifd.value(259), Some(6 | 7)) {
                    found.extend(part(ifd.value(0x111).zip(ifd.value(0x117))));
                }
            }
            if kind == Kind::Rw2 {
                found.extend(ifd0.bytes(0x2E));
            }
            let orientation = ifd0.value(0x112).and_then(|o| u8::try_from(o).ok());
            (found, orientation)
        }
    };
    let mut best = None;
    let mut area = 0;
    for jpeg in candidates {
        if jpeg.len() < 1024 || !jpeg.starts_with(b"\xff\xd8\xff") {
            continue;
        }
        if let Ok(frame) = jpeg::frame(jpeg)
            && matches!(frame.marker, 0xc0..=0xc2)
            && u64::from(frame.width) * u64::from(frame.height) > area
        {
            area = u64::from(frame.width) * u64::from(frame.height);
            best = Some(jpeg);
        }
    }
    let preview = best.ok_or_else(|| Error::raw_no_preview(name(kind)))?;
    let mut px = jpeg::decode(preview)?;
    if px.exif.as_deref().and_then(exif_orientation).is_none() {
        px.orientation = orientation.filter(|o| (1..=8).contains(o)).unwrap_or(1);
    }
    px.warnings.push(Warning::raw_preview(px.width, px.height));
    if px.width.max(px.height) < 1024 {
        px.warnings
            .push(Warning::small_preview(px.width, px.height));
    }
    Ok(px)
}
