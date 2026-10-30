//! ISO-BMFF: the HEIF/AVIF primary item with the properties the OS decoders need, and the CR3
//! boxes that hold RAW previews. Bounded: 10,000 boxes, 16 levels, 4,096 items, 64 extents.

use crate::check_size;
use std::cell::Cell;

use crate::color::{Cicp, Colour};
use crate::detect::Kind;
use crate::error::Error;

/// A box: its type and where its body lies in the file.
#[derive(Debug, Clone, Copy)]
struct Bx {
    kind: [u8; 4],
    start: usize,
    end: usize,
}

/// Reads boxes from one file, counting them against the limits.
struct Reader<'a> {
    file: &'a [u8],
    format: &'a str,
    count: Cell<usize>,
}

/// What a HEIF or AVIF file's primary image is, read before any OS decoder sees the file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Heif {
    /// `Heic` or `Avif`, from the primary item's coding.
    pub kind: Kind,
    pub width: u32,
    pub height: u32,
    pub colour: Option<Colour>,
    /// `irot`: anticlockwise quarter turns.
    pub rotation: u8,
    /// `imir`: 0 mirrors left and right, 1 top and bottom.
    pub mirror: Option<u8>,
    pub alpha: bool,
    /// The `Exif` item from its TIFF header on.
    pub exif: Option<Vec<u8>>,
    /// The file also holds an image sequence.
    pub sequence: bool,
}

impl Heif {
    /// `irot` then `imir` as an EXIF orientation (1–8).
    pub fn orientation(&self) -> u8 {
        match (self.rotation, self.mirror) {
            (0, None) => 1,
            (1, None) => 8,
            (2, None) => 3,
            (3, None) => 6,
            (0, Some(0)) | (2, Some(1)) => 2,
            (0, Some(_)) | (2, Some(_)) => 4,
            (1, Some(0)) | (3, Some(1)) => 7,
            _ => 5,
        }
    }
}

const ALPHA: [&[u8]; 2] = [
    b"urn:mpeg:mpegB:cicp:systems:auxiliary:alpha",
    b"urn:mpeg:hevc:2015:auxid:1",
];

impl<'a> Reader<'a> {
    fn corrupt(&self, reason: &str) -> Error {
        Error::corrupt(self.format, reason)
    }

    fn get(&self, at: usize, len: usize) -> Result<&'a [u8], Error> {
        at.checked_add(len)
            .and_then(|end| self.file.get(at..end))
            .ok_or_else(|| self.corrupt("a box is cut short"))
    }

    fn uint(&self, at: usize, len: usize) -> Result<u64, Error> {
        let bytes = self.get(at, len)?;
        Ok(bytes.iter().fold(0, |n, &b| n << 8 | u64::from(b)))
    }

    /// The boxes between `start` and `end`. Callers descend fixed paths at most 5 deep, so the
    /// 16-level limit holds by construction.
    fn boxes(&self, start: usize, end: usize) -> Result<Vec<Bx>, Error> {
        let mut found = Vec::new();
        let mut at = start;
        while at + 8 <= end {
            self.count.set(self.count.get() + 1);
            if self.count.get() > 10_000 {
                return Err(self.corrupt("it has more than 10,000 boxes"));
            }
            let (size, header) = match self.uint(at, 4)? {
                0 => ((end - at) as u64, 8),
                1 => (self.uint(at + 8, 8)?, 16),
                size => (size, 8),
            };
            let kind = self.get(at + 4, 4)?.try_into().unwrap();
            let next = usize::try_from(size)
                .ok()
                .and_then(|s| at.checked_add(s))
                .filter(|&next| size >= header && next <= end)
                .ok_or_else(|| self.corrupt("a box runs past its end"))?;
            found.push(Bx {
                kind,
                start: at + header as usize,
                end: next,
            });
            at = next;
        }
        Ok(found)
    }

    /// The children of `parent`, whose own fields take `skip` bytes first.
    fn children(&self, parent: Bx, skip: usize) -> Result<Vec<Bx>, Error> {
        self.boxes(parent.start + skip, parent.end)
    }
}

fn find(boxes: &[Bx], kind: &[u8; 4]) -> Option<Bx> {
    boxes.iter().copied().find(|b| &b.kind == kind)
}

/// An item's data: (construction method, extents as (offset, length)).
type Location = (u16, Vec<(u64, u64)>);

/// Reads the primary item of a HEIF or AVIF file and refuses the variants the matrix leaves out
/// (spec §2.2 row 9); `kind` is what the brands said.
pub fn heif(data: &[u8], kind: Kind) -> Result<Heif, Error> {
    let label = kind.label();
    let r = Reader {
        file: data,
        format: &label,
        count: Cell::new(0),
    };
    let refuse = |variant: &str| Error::unsupported_variant(&label, variant);
    let top = r.boxes(0, data.len())?;
    let sequence = find(&top, b"moov").is_some();
    let no_primary = || refuse("no primary image");
    let meta = find(&top, b"meta").ok_or_else(no_primary)?;
    let meta = r.children(meta, 4)?;
    let handler = find(&meta, b"hdlr")
        .map(|h| r.get(h.start + 8, 4))
        .transpose()?;
    let pitm = find(&meta, b"pitm").ok_or_else(no_primary)?;
    if handler != Some(b"pict") {
        return Err(no_primary());
    }
    // Ids and counts are 2 bytes in version 0 boxes and 4 bytes from version 1.
    let wide = |b: Bx| Ok::<_, Error>(if r.uint(b.start, 1)? >= 1 { 4 } else { 2 });
    let id_len = wide(pitm)?;
    let primary = r.uint(pitm.start + 4, id_len)? as u32;

    // Item types from iinf.
    let mut types = Vec::new();
    if let Some(iinf) = find(&meta, b"iinf") {
        let count_len = wide(iinf)?;
        for infe in r.children(iinf, 4 + count_len)? {
            if &infe.kind != b"infe" {
                continue;
            }
            let version = r.uint(infe.start, 1)?;
            if version < 2 {
                continue;
            }
            let id_len = if version == 2 { 2 } else { 4 };
            let id = r.uint(infe.start + 4, id_len)? as u32;
            let ty: [u8; 4] = r.get(infe.start + 4 + id_len + 2, 4)?.try_into().unwrap();
            types.push((id, ty));
            if types.len() > 4096 {
                return Err(r.corrupt("it has more than 4,096 items"));
            }
        }
    }
    let type_of = |id: u32| types.iter().find(|(i, _)| *i == id).map(|(_, t)| *t);

    // References: (type, from, to).
    let mut refs = Vec::new();
    if let Some(iref) = find(&meta, b"iref") {
        let id_len = wide(iref)?;
        for reference in r.children(iref, 4)? {
            let from = r.uint(reference.start, id_len)? as u32;
            let count = r.uint(reference.start + id_len, 2)? as usize;
            for i in 0..count {
                let to = r.uint(reference.start + id_len + 2 + i * id_len, id_len)? as u32;
                refs.push((reference.kind, from, to));
            }
        }
    }
    let targets = |kind: &[u8; 4], from: u32| -> Vec<u32> {
        refs.iter()
            .filter(|(k, f, _)| k == kind && *f == from)
            .map(|r| r.2)
            .collect()
    };

    // The coded items that make up the primary image, and their coding.
    let mut coded = vec![primary];
    let mut ty = type_of(primary).ok_or_else(|| r.corrupt("its primary item has no type"))?;
    if &ty == b"tmap" {
        coded = targets(b"dimg", primary).into_iter().take(1).collect();
        ty = coded
            .first()
            .and_then(|&id| type_of(id))
            .unwrap_or(*b"tmap");
    }
    if &ty == b"grid" {
        let grid = coded[0];
        coded = targets(b"dimg", grid);
        let tiles: Vec<_> = coded.iter().map(|&id| type_of(id)).collect();
        ty = match tiles.first() {
            Some(Some(t)) if tiles.iter().all(|x| x == &Some(*t)) => *t,
            Some(Some(t)) if t == b"hvc1" || t == b"av01" => *b"mixd",
            Some(Some(t)) => *t,
            _ => return Err(r.corrupt("its grid has no tiles")),
        };
    }
    let kind = match &ty {
        b"hvc1" => Kind::Heic,
        b"av01" => Kind::Avif,
        b"iovl" => return Err(refuse("overlay images")),
        b"iden" => return Err(refuse("identity-derived images")),
        b"jpeg" => return Err(refuse("JPEG compression")),
        b"j2k1" => return Err(refuse("JPEG 2000 compression")),
        b"unci" => return Err(refuse("no compression (unci)")),
        b"vvc1" => return Err(refuse("VVC compression")),
        b"evc1" => return Err(refuse("EVC compression")),
        other => {
            let name = String::from_utf8_lossy(other).trim().to_string();
            return Err(refuse(&format!("'{name}' compression")));
        }
    };

    // Item locations from iloc.
    let mut locations: Vec<(u32, Location)> = Vec::new();
    if let Some(iloc) = find(&meta, b"iloc") {
        let version = r.uint(iloc.start, 1)?;
        if version > 2 {
            return Err(r.corrupt("its iloc box has an unknown version"));
        }
        let sizes = r.uint(iloc.start + 4, 2)?;
        let [offset_len, length_len, base_len, index_len] =
            [12, 8, 4, 0].map(|shift| (sizes >> shift & 15) as usize);
        let index_len = if version == 0 { 0 } else { index_len };
        if [offset_len, length_len, base_len, index_len]
            .iter()
            .any(|s| ![0, 4, 8].contains(s))
        {
            return Err(r.corrupt("its iloc box has an unknown field size"));
        }
        let (count_len, id_len) = if version < 2 { (2, 2) } else { (4, 4) };
        let mut at = iloc.start + 6;
        let mut read = |len: usize| {
            let v = r.uint(at, len);
            at += len;
            v
        };
        for _ in 0..read(count_len)? {
            let id = read(id_len)? as u32;
            let method = if version == 0 {
                0
            } else {
                read(2)? as u16 & 15
            };
            read(2)?;
            let base = read(base_len)?;
            let extents = read(2)?;
            if extents > 64 {
                return Err(r.corrupt("an item has more than 64 extents"));
            }
            let mut list = Vec::new();
            for _ in 0..extents {
                read(index_len)?;
                list.push((base.saturating_add(read(offset_len)?), read(length_len)?));
            }
            locations.push((id, (method, list)));
            if locations.len() > 4096 {
                return Err(r.corrupt("it has more than 4,096 items"));
            }
        }
    }
    let location = |id: u32| locations.iter().find(|(i, _)| *i == id).map(|(_, l)| l);
    let in_file =
        |&(offset, length): &(u64, u64)| offset.saturating_add(length) <= data.len() as u64;
    for &id in &coded {
        let (method, extents) =
            location(id).ok_or_else(|| r.corrupt("an image item has no location"))?;
        if *method != 0 {
            return Err(refuse("item data in idat or another item"));
        }
        if !extents.iter().all(in_file) {
            return Err(r.corrupt("an image item lies outside the file"));
        }
    }

    // Properties from iprp: ipco holds them, ipma assigns them (1-based) to items.
    let mut props = Vec::new();
    let mut assigned = Vec::new();
    if let Some(iprp) = find(&meta, b"iprp") {
        let iprp = r.children(iprp, 0)?;
        if let Some(ipco) = find(&iprp, b"ipco") {
            props = r.children(ipco, 0)?;
        }
        if let Some(ipma) = find(&iprp, b"ipma") {
            let version = r.uint(ipma.start, 1)?;
            let flags = r.uint(ipma.start + 1, 3)?;
            let (id_len, index_len) = (if version < 1 { 2 } else { 4 }, 1 + (flags & 1) as usize);
            let mut at = ipma.start + 8;
            for _ in 0..r.uint(ipma.start + 4, 4)? {
                let id = r.uint(at, id_len)? as u32;
                let count = r.uint(at + id_len, 1)? as usize;
                at += id_len + 1;
                for _ in 0..count {
                    let mask = if index_len == 1 { 0x7f } else { 0x7fff };
                    let index = r.uint(at, index_len)? as usize & mask;
                    at += index_len;
                    assigned.push((id, index));
                }
            }
        }
    }
    let property = |id: u32, kind: &[u8; 4]| {
        assigned
            .iter()
            .filter(|(i, _)| *i == id)
            .filter_map(|&(_, index)| props.get(index.checked_sub(1)?))
            .find(|p| &p.kind == kind)
            .copied()
    };
    let ispe = property(primary, b"ispe")
        .ok_or_else(|| r.corrupt("its primary image has no size (ispe)"))?;
    let (width, height) = (r.uint(ispe.start + 4, 4)?, r.uint(ispe.start + 8, 4)?);
    check_size(width, height, &label)?;
    let colour = match property(primary, b"colr") {
        Some(colr) => match r.get(colr.start, 4)? {
            b"nclx" => Some(Colour::Cicp(Cicp {
                primaries: r.uint(colr.start + 4, 2)? as u16,
                transfer: r.uint(colr.start + 6, 2)? as u16,
                matrix: r.uint(colr.start + 8, 2)? as u16,
                full_range: r.uint(colr.start + 10, 1)? & 0x80 != 0,
            })),

            b"prof" | b"rICC" => Some(Colour::Icc(data[colr.start + 4..colr.end].to_vec())),
            _ => None,
        },
        None => None,
    };
    let rotation = match property(primary, b"irot") {
        Some(irot) => r.uint(irot.start, 1)? as u8 & 3,
        None => 0,
    };
    let mirror = match property(primary, b"imir") {
        Some(imir) => Some(r.uint(imir.start, 1)? as u8 & 1),
        None => None,
    };
    let mut alpha = false;
    for &(kind, from, to) in &refs {
        if &kind == b"auxl"
            && to == primary
            && let Some(aux) = property(from, b"auxC")
        {
            let urn = r.get(aux.start + 4, aux.end - aux.start - 4)?;
            let urn = urn.split(|&b| b == 0).next().unwrap_or_default();
            alpha |= ALPHA.contains(&urn);
        }
    }
    let exif_item = types
        .iter()
        .filter(|(_, t)| t == b"Exif")
        .map(|(id, _)| *id)
        .find(|&id| targets(b"cdsc", id).contains(&primary));
    let exif = exif_item.and_then(location).and_then(|(method, extents)| {
        let mut block = Vec::new();
        for &(offset, length) in extents.iter().filter(|_| *method == 0) {
            let end = if length == 0 {
                data.len() as u64
            } else {
                offset.checked_add(length)?
            };
            block.extend(data.get(offset as usize..end as usize)?);
        }
        let skip = u32::from_be_bytes(block.get(..4)?.try_into().ok()?) as usize;
        block.get(4 + skip..).map(<[u8]>::to_vec)
    });
    Ok(Heif {
        kind,
        width: width as u32,
        height: height as u32,
        colour,
        rotation,
        mirror,
        alpha,
        exif,
        sequence,
    })
}

const CANON_PREVIEW: [u8; 16] = [
    0xea, 0xf4, 0x2b, 0x5e, 0x1c, 0x98, 0x4b, 0x88, 0xb9, 0xfb, 0xb7, 0xdc, 0x40, 0x6e, 0x4d, 0x16,
];
const CANON_METADATA: [u8; 16] = [
    0x85, 0xc0, 0xb6, 0x87, 0x82, 0x0f, 0x11, 0xe0, 0x81, 0x11, 0xf4, 0xce, 0x46, 0x2b, 0x6a, 0x48,
];

/// The parts of a CR3 a preview comes from.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Cr3<'a> {
    /// Track 1's first sample, then the `PRVW` JPEG, when present.
    pub previews: Vec<&'a [u8]>,
    /// The `CMT1` TIFF block, which holds IFD0.
    pub cmt1: Option<&'a [u8]>,
}

/// Finds a CR3's preview candidates and its `CMT1` block.
pub fn cr3(data: &[u8]) -> Result<Cr3<'_>, Error> {
    let r = Reader {
        file: data,
        format: "CR3",
        count: Cell::new(0),
    };
    let top = r.boxes(0, data.len())?;
    let mut previews = Vec::new();
    let mut cmt1 = None;
    if let Some(moov) = find(&top, b"moov") {
        let moov = r.children(moov, 0)?;
        let canon = moov.iter().find(|b| {
            &b.kind == b"uuid" && data.get(b.start..b.start + 16) == Some(&CANON_METADATA)
        });
        if let Some(&canon) = canon {
            let tags = r.children(canon, 16)?;
            cmt1 = find(&tags, b"CMT1").map(|b| &data[b.start..b.end]);
        }
        if let Some(trak) = find(&moov, b"trak") {
            let mut level = r.children(trak, 0)?;
            for kind in [b"mdia", b"minf", b"stbl"] {
                let Some(b) = find(&level, kind) else {
                    level = Vec::new();
                    break;
                };
                level = r.children(b, 0)?;
            }
            let offset = match (find(&level, b"stco"), find(&level, b"co64")) {
                (Some(stco), _) => Some(r.uint(stco.start + 8, 4)?),
                (None, Some(co64)) => Some(r.uint(co64.start + 8, 8)?),
                _ => None,
            };
            let size = match find(&level, b"stsz") {
                Some(stsz) => match r.uint(stsz.start + 4, 4)? {
                    0 => Some(r.uint(stsz.start + 12, 4)?),
                    size => Some(size),
                },
                None => None,
            };
            if let (Some(offset), Some(size)) = (offset, size) {
                let sample = (offset as usize).checked_add(size as usize);
                previews.extend(sample.and_then(|end| data.get(offset as usize..end)));
            }
        }
    }
    let uuid = top
        .iter()
        .find(|b| &b.kind == b"uuid" && data.get(b.start..b.start + 16) == Some(&CANON_PREVIEW));
    if let Some(&uuid) = uuid
        && let Some(prvw) = find(&r.children(uuid, 24)?, b"PRVW")
    {
        let body = &data[prvw.start..prvw.end];
        let jpeg = body.windows(3).take(32).position(|w| w == b"\xff\xd8\xff");
        previews.extend(jpeg.map(|at| &body[at..]));
    }
    Ok(Cr3 { previews, cmt1 })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resize::orient;
    use crate::{Image, Layout};

    #[test]
    fn irot_then_imir_match_their_exif_orientation() {
        let image = Image {
            width: 3,
            height: 2,
            layout: Layout::Grey,
            data: vec![1, 2, 3, 4, 5, 6],
        };
        for rotation in 0..4 {
            for mirror in [None, Some(0), Some(1)] {
                let mut step = orient(image.clone(), [1, 8, 3, 6][rotation as usize]);
                if let Some(axis) = mirror {
                    step = orient(step, [2, 4][axis as usize]);
                }
                let heif = Heif {
                    kind: Kind::Heic,
                    width: 3,
                    height: 2,
                    colour: None,
                    rotation,
                    mirror,
                    alpha: false,
                    exif: None,
                    sequence: false,
                };
                let once = orient(image.clone(), heif.orientation());
                assert_eq!(once, step, "irot {rotation} imir {mirror:?}");
            }
        }
    }
}
