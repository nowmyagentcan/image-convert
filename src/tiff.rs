//! TIFF: the IFD walker that TIFF, RAW and EXIF share, the checks that refuse every TIFF variant
//! outside the matrix before a decoder sees the file, and strip decoding through the tiff crate.

use std::io::Cursor;

use ::tiff::decoder::{Decoder, DecodingResult, Limits};

use crate::error::{Error, Warning};
use crate::jpeg::ycc_to_rgb;
use crate::{Layout, Pixels, Samples, check_size};

/// A TIFF header's byte order and the file it lies in.
#[derive(Debug, Clone, Copy)]
pub struct Tiff<'a> {
    pub data: &'a [u8],
    big: bool,
}

/// One IFD entry: its tag, type, value count and where its value bytes start.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Entry {
    pub tag: u16,
    pub kind: u16,
    pub count: u32,
    /// The offset of the value: inside the entry when it fits in 4 bytes.
    pub at: usize,
}

/// An image file directory and how deep it lies (0 for the main chain, 1 for its SubIFDs, …).
#[derive(Debug, Clone)]
pub struct Ifd<'a> {
    tiff: Tiff<'a>,
    pub offset: usize,
    pub depth: u8,
    pub entries: Vec<Entry>,
}

const MAX_IFDS: usize = 64;
const MAX_ENTRIES: usize = 4096;
const MAX_DEPTH: u8 = 4;

impl<'a> Tiff<'a> {
    /// The header of `data` with TIFF's magic 42, or RW2's 0x55, and the offset of IFD0.
    pub fn new(data: &'a [u8]) -> Option<(Self, usize)> {
        let big = match data.get(..2)? {
            b"II" => false,
            b"MM" => true,
            _ => return None,
        };
        let tiff = Tiff { data, big };
        matches!(tiff.u16(2)?, 42 | 0x55).then_some(())?;
        Some((tiff, tiff.u32(4)? as usize))
    }

    pub fn u16(&self, at: usize) -> Option<u16> {
        let b = self.data.get(at..at.checked_add(2)?)?;
        Some(if self.big {
            u16::from_be_bytes([b[0], b[1]])
        } else {
            u16::from_le_bytes([b[0], b[1]])
        })
    }

    /// `v` as two bytes in this file's byte order.
    pub fn u16_bytes(&self, v: u16) -> [u8; 2] {
        if self.big {
            v.to_be_bytes()
        } else {
            v.to_le_bytes()
        }
    }

    pub fn u32(&self, at: usize) -> Option<u32> {
        let b: [u8; 4] = self.data.get(at..at.checked_add(4)?)?.try_into().ok()?;
        Some(if self.big {
            u32::from_be_bytes(b)
        } else {
            u32::from_le_bytes(b)
        })
    }

    /// The IFD at `offset` and the offset of the next one (0 at the end of the chain or of the
    /// file); `None` when it lies outside the file or has more than 4,096 entries.
    pub fn ifd(&self, offset: usize, depth: u8) -> Option<(Ifd<'a>, usize)> {
        let count = usize::from(self.u16(offset)?);
        if count > MAX_ENTRIES {
            return None;
        }
        let mut entries = Vec::with_capacity(count);
        for i in 0..count {
            let at = offset + 2 + 12 * i;
            let (tag, kind, n) = (self.u16(at)?, self.u16(at + 2)?, self.u32(at + 4)?);
            let len = u64::from(n) * size_of_type(kind);
            let value = if len <= 4 {
                at + 8
            } else {
                self.u32(at + 8)? as usize
            };
            // Entries whose values lie outside the file are skipped; unknown types keep a tag only.
            if (value as u64).saturating_add(len) <= self.data.len() as u64 {
                entries.push(Entry {
                    tag,
                    kind,
                    count: n,
                    at: value,
                });
            }
        }
        let next = self.u32(offset + 2 + 12 * count).unwrap_or(0) as usize;
        let ifd = Ifd {
            tiff: *self,
            offset,
            depth,
            entries,
        };
        Some((ifd, next))
    }

    /// The IFD chain from `first`; with `nested`, also each IFD's SubIFDs (0x014A) and EXIF IFD
    /// (0x8769) down to depth 4. At most 64 IFDs; a visited offset or an unreadable IFD ends its
    /// chain.
    pub fn ifds(&self, first: usize, nested: bool) -> Vec<Ifd<'a>> {
        let mut found: Vec<Ifd<'a>> = Vec::new();
        let mut chains = vec![(first, 0)];
        let mut i = 0;
        while let Some(&(mut offset, depth)) = chains.get(i) {
            i += 1;
            while offset != 0 && found.len() < MAX_IFDS && found.iter().all(|f| f.offset != offset)
            {
                let Some((ifd, next)) = self.ifd(offset, depth) else {
                    break;
                };
                if nested && depth < MAX_DEPTH {
                    for tag in [0x014A, 0x8769] {
                        chains.extend(ifd.values(tag).iter().map(|&o| (o as usize, depth + 1)));
                    }
                }
                found.push(ifd);
                offset = next;
            }
        }
        found
    }
}

impl<'a> Ifd<'a> {
    pub fn get(&self, tag: u16) -> Option<&Entry> {
        self.entries.iter().find(|e| e.tag == tag)
    }

    /// Up to the first 64 values of an integer tag (BYTE, SHORT, LONG or IFD).
    pub fn values(&self, tag: u16) -> Vec<u32> {
        let Some(e) = self.get(tag) else {
            return Vec::new();
        };
        let t = &self.tiff;
        (0..e.count.min(64) as usize)
            .filter_map(|i| match e.kind {
                1 => t.data.get(e.at + i).map(|&b| u32::from(b)),
                3 => t.u16(e.at + 2 * i).map(u32::from),
                4 | 13 => t.u32(e.at + 4 * i),
                _ => None,
            })
            .collect()
    }

    /// The first value of an integer tag.
    pub fn value(&self, tag: u16) -> Option<u32> {
        self.values(tag).first().copied()
    }

    /// The raw value bytes of a tag.
    pub fn bytes(&self, tag: u16) -> Option<&'a [u8]> {
        let e = self.get(tag)?;
        let len = u64::from(e.count) * size_of_type(e.kind);
        self.tiff.data.get(e.at..e.at + len as usize)
    }
}

/// Bytes per value of a TIFF field type, 0 for types classic TIFF doesn't define.
fn size_of_type(kind: u16) -> u64 {
    match kind {
        1 | 2 | 6 | 7 => 1,
        3 | 8 => 2,
        4 | 9 | 11 | 13 => 4,
        5 | 10 | 12 => 8,
        _ => 0,
    }
}

/// How a page that passed [`check`] stores its pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Page {
    pub width: u32,
    pub height: u32,
    pub photometric: u32,
    pub bits: u32,
    /// Colour samples per pixel: 1, 3 or 4.
    pub colours: usize,
    /// One extra sample follows the colours: alpha when `alpha`, else it is dropped.
    pub extra: bool,
    pub alpha: bool,
}

/// Checks IFD0 against the supported TIFF variants (spec §2.2 row 7), giving `unsupported_variant`
/// with the variant's name for the first one it hits.
pub fn check(data: &[u8]) -> Result<Page, Error> {
    if data.starts_with(b"II+\0") || data.starts_with(b"MM\0+") {
        return Err(Error::unsupported_variant("TIFF", "BigTIFF"));
    }
    let ifd = Tiff::new(data)
        .and_then(|(tiff, first)| tiff.ifd(first, 0))
        .ok_or_else(|| Error::corrupt("TIFF", "its first directory lies outside the file"))?
        .0;
    let refuse = |variant: String| Err(Error::unsupported_variant("TIFF", variant));
    let tag = |t: u16, default: u32| ifd.value(t).unwrap_or(default);
    let samples = tag(277, 1);
    if ifd.get(322).is_some() || ifd.get(324).is_some() {
        return refuse("tiles".into());
    }
    if tag(284, 1) == 2 && samples > 1 {
        return refuse("planar layout".into());
    }
    let compression = tag(259, 1);
    let name = match compression {
        1 | 4 | 5 | 7 | 8 | 32946 | 32773 => None,
        2 => Some("CCITT 1D".to_string()),
        3 => Some("Group 3 fax".into()),
        6 => Some("old-style JPEG".into()),
        34712 => Some("JPEG 2000".into()),
        34925 => Some("LZMA".into()),
        50000 => Some("Zstandard".into()),
        50001 => Some("WebP".into()),
        n => Some(n.to_string()),
    };
    if let Some(name) = name {
        return refuse(format!("compression {name}"));
    }
    for format in ifd.values(339) {
        match format {
            1 => {}
            3 | 6 => return refuse("floating-point samples".into()),
            2 | 5 => return refuse("signed samples".into()),
            n => return refuse(format!("sample format {n}")),
        }
    }
    let depths = ifd.values(258);
    let bits = depths.first().copied().unwrap_or(1);
    if depths.iter().any(|&b| b != bits) {
        let list: Vec<_> = depths.iter().map(u32::to_string).collect();
        return refuse(format!("{}-bit samples", list.join("/")));
    }
    let photometric = ifd
        .value(262)
        .ok_or_else(|| Error::corrupt("TIFF", "its PhotometricInterpretation tag is missing"))?;
    let colours = match photometric {
        0 | 1 => 1,
        2 | 6 => 3,
        5 => 4,
        3 => return refuse("palette colour".into()),
        4 => return refuse("transparency mask".into()),
        8..=10 => return refuse("CIELab colour".into()),
        n => return refuse(format!("photometric interpretation {n}")),
    };
    if tag(266, 1) == 2 {
        return refuse("FillOrder 2".into());
    }
    let extras = samples.checked_sub(colours).ok_or_else(|| {
        Error::corrupt(
            "TIFF",
            format!("it has {samples} samples per pixel for {colours} colours"),
        )
    })?;
    let kind = ifd.value(338).unwrap_or(0);
    if kind == 1 {
        return refuse("premultiplied alpha".into());
    }
    if extras > 1 {
        return refuse(format!("{extras} extra samples"));
    }
    match tag(317, 1) {
        1 | 2 => {}
        n => return refuse(format!("predictor {n}")),
    }
    let (extra, alpha) = (extras == 1, extras == 1 && kind == 2);
    match photometric {
        6 if compression != 7 => return refuse("YCbCr without JPEG compression".into()),
        5 if bits != 8 => return refuse(format!("CMYK with {bits}-bit samples")),
        5 if extra => return refuse("CMYK with alpha".into()),
        5 if tag(332, 1) != 1 => return refuse("inks other than CMYK".into()),
        0 if extra => return refuse("WhiteIsZero with alpha".into()),
        0 | 1 if bits == 1 && extra => return refuse("1-bit samples with an extra sample".into()),
        0 | 1 if !matches!(bits, 1 | 8 | 16) => return refuse(format!("{bits}-bit samples")),
        2 | 6 if !matches!(bits, 8 | 16) => return refuse(format!("{bits}-bit samples")),
        _ => {}
    }
    if compression == 4 && bits != 1 {
        return refuse(format!("Group 4 fax at {bits} bits"));
    }
    if compression == 7 && bits != 8 {
        return refuse(format!("{bits}-bit JPEG compression"));
    }
    let size = |t| {
        ifd.value(t)
            .ok_or_else(|| Error::corrupt("TIFF", "its image size is missing"))
    };
    Ok(Page {
        width: size(256)?,
        height: size(257)?,
        photometric,
        bits,
        colours: colours as usize,
        extra,
        alpha,
    })
}

/// Decodes the first page of a TIFF that passes [`check`]: grey, grey + alpha, RGB, RGBA or CMYK,
/// with its orientation, ICC profile and page count.
pub fn decode(data: &[u8]) -> Result<Pixels, Error> {
    let page = check(data)?;
    check_size(page.width.into(), page.height.into(), "TIFF")?;
    let (tiff, first) = Tiff::new(data).expect("check read the header");
    let chain = tiff.ifds(first, false);
    let pages = chain.iter().filter(|i| i.value(254).unwrap_or(0) & 1 == 0);
    let pages = pages.count().max(1) as u32;
    let ifd0 = &chain[0];

    // The tiff crate misreads the strips when ExtraSamples is 0 (unspecified), so a copy labels
    // the sample as alpha; it is dropped below either way.
    let copy;
    let data = match ifd0.get(338).filter(|_| page.extra && !page.alpha) {
        Some(e) => {
            copy = [&data[..e.at], &tiff.u16_bytes(2), &data[e.at + 2..]].concat();
            &copy[..]
        }
        None => data,
    };
    let failed = |e: ::tiff::TiffError| Error::corrupt("TIFF", e);
    let mut limits = Limits::default();
    limits.decoding_buffer_size = 2 << 30;
    let mut decoder = Decoder::new(Cursor::new(data))
        .map_err(failed)?
        .with_limits(limits);
    let (w, h) = (page.width as usize, page.height as usize);
    let channels = page.colours + usize::from(page.extra);
    let samples = match decoder.read_image().map_err(failed)? {
        DecodingResult::U8(s) if page.bits == 1 && s.len() >= w.div_ceil(8) * h => {
            let row = w.div_ceil(8);
            let bit = |x: usize, y: usize| s[y * row + x / 8] >> (7 - x % 8) & 1;
            Samples::U8(
                (0..h)
                    .flat_map(|y| (0..w).map(move |x| bit(x, y) * 255))
                    .collect(),
            )
        }
        DecodingResult::U8(s) if s.len() == w * h * channels => Samples::U8(s),
        DecodingResult::U16(s) if s.len() == w * h * channels => Samples::U16(s),
        _ => {
            let reason = "its strips hold the wrong amount of data";
            return Err(Error::corrupt("TIFF", reason));
        }
    };
    let samples = match samples {
        Samples::U8(s) if page.extra && !page.alpha => Samples::U8(drop_extra(&s, channels)),
        Samples::U16(s) if page.extra && !page.alpha => Samples::U16(drop_extra(&s, channels)),
        Samples::U8(s) if page.photometric == 6 => Samples::U8(
            s.as_chunks::<3>()
                .0
                .iter()
                .flat_map(|&[y, cb, cr]| ycc_to_rgb(y, cb, cr))
                .collect(),
        ),
        samples => samples,
    };
    let layout = match (page.colours, page.alpha) {
        (1, false) => Layout::Grey,
        (1, true) => Layout::GreyAlpha,
        (3, false) => Layout::Rgb,
        (3, true) => Layout::Rgba,
        _ => Layout::Cmyk,
    };
    let orientation = ifd0.value(274).filter(|o| (1..=8).contains(o));
    Ok(Pixels {
        width: page.width,
        height: page.height,
        layout,
        samples,
        bit_depth: page.bits as u8,
        icc: ifd0.bytes(34675).map(<[u8]>::to_vec),
        exif: None,
        orientation: orientation.unwrap_or(1) as u8,
        frames: pages,
        warnings: (pages > 1)
            .then(|| Warning::first_page_only(pages))
            .into_iter()
            .collect(),
    })
}

/// Each pixel's samples without the last one.
fn drop_extra<T: Copy>(samples: &[T], channels: usize) -> Vec<T> {
    samples
        .chunks_exact(channels)
        .flat_map(|p| &p[..channels - 1])
        .copied()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A TIFF with one IFD of SHORT/LONG tags (sorted by the caller) and `next` after it.
    fn tiff(big: bool, tags: &[(u16, &[u32])], next: u32) -> Vec<u8> {
        let mut d = if big {
            b"MM\0*".to_vec()
        } else {
            b"II*\0".to_vec()
        };
        let put16 = |d: &mut Vec<u8>, v: u16| {
            d.extend(if big {
                v.to_be_bytes()
            } else {
                v.to_le_bytes()
            })
        };
        let put32 = |d: &mut Vec<u8>, v: u32| {
            d.extend(if big {
                v.to_be_bytes()
            } else {
                v.to_le_bytes()
            })
        };
        put32(&mut d, 8);
        put16(&mut d, tags.len() as u16);
        let values_at = 8 + 2 + 12 * tags.len() as u32 + 4;
        let mut extra = Vec::new();
        for (tag, values) in tags {
            put16(&mut d, *tag);
            put16(&mut d, 4);
            put32(&mut d, values.len() as u32);
            if values.len() == 1 {
                put32(&mut d, values[0]);
            } else {
                put32(&mut d, values_at + extra.len() as u32);
                values.iter().for_each(|&v| put32(&mut extra, v));
            }
        }
        put32(&mut d, next);
        d.extend(extra);
        d
    }

    type Tags = &'static [(u16, &'static [u32])];

    const RGB: Tags = &[
        (256, &[4]),
        (257, &[2]),
        (258, &[8, 8, 8]),
        (262, &[2]),
        (277, &[3]),
    ];

    fn with(base: Tags, changes: Tags) -> Vec<u8> {
        let mut tags: Vec<_> = base
            .iter()
            .filter(|(t, _)| changes.iter().all(|(c, _)| c != t))
            .chain(changes)
            .copied()
            .collect();
        tags.sort_by_key(|(t, _)| *t);
        tiff(false, &tags, 0)
    }

    #[test]
    fn reads_values_in_both_byte_orders() {
        for big in [false, true] {
            let data = tiff(big, RGB, 0);
            let (t, first) = Tiff::new(&data).unwrap();
            let (ifd, next) = t.ifd(first, 0).unwrap();
            assert_eq!(
                (ifd.value(256), ifd.values(258), next),
                (Some(4), vec![8, 8, 8], 0)
            );
            assert_eq!(ifd.value(999), None);
        }
        assert!(Tiff::new(b"II*").is_none() && Tiff::new(b"XX*\0\x08\0\0\0").is_none());
    }

    #[test]
    fn walks_chains_and_sub_ifds_and_stops_at_cycles() {
        // IFD0 at 8 points back to itself; with a SubIFD tag naming IFD0 too.
        let data = tiff(false, &[(0x014A, &[8])], 8);
        let (t, first) = Tiff::new(&data).unwrap();
        assert_eq!(t.ifds(first, true).len(), 1);
        let far = tiff(false, &[(0x014A, &[1 << 30])], 1 << 30);
        assert_eq!(Tiff::new(&far).unwrap().0.ifds(8, true).len(), 1);
        let mut huge = b"II*\0\x08\0\0\0".to_vec();
        huge.extend(4097u16.to_le_bytes());
        huge.resize(8 + 2 + 12 * 4097 + 4, 0);
        assert!(Tiff::new(&huge).unwrap().0.ifd(8, 0).is_none());
    }

    #[test]
    fn supported_pages_pass() {
        let page = check(&tiff(true, RGB, 0)).unwrap();
        assert_eq!(
            (page.width, page.height, page.colours, page.extra),
            (4, 2, 3, false)
        );
        let rgba = check(&with(RGB, &[(277, &[4]), (338, &[2])])).unwrap();
        assert!(rgba.extra && rgba.alpha);
        let unspecified = check(&with(RGB, &[(277, &[4])])).unwrap();
        assert!(unspecified.extra && !unspecified.alpha);
        let grey: Tags = &[(256, &[1]), (257, &[1]), (262, &[1])];
        assert_eq!(check(&with(grey, &[(259, &[4])])).unwrap().bits, 1);
        check(&with(RGB, &[(259, &[7]), (262, &[6])])).unwrap();
        check(&with(RGB, &[(259, &[32946]), (317, &[2])])).unwrap();
    }

    #[test]
    fn every_unsupported_variant_is_named() {
        let cmyk: Tags = &[(256, &[1]), (257, &[1]), (262, &[5]), (277, &[4])];
        let grey: Tags = &[(256, &[1]), (257, &[1]), (262, &[1])];
        let cases: [(Tags, Tags, &str); 24] = [
            (RGB, &[(322, &[16])], "tiles"),
            (RGB, &[(284, &[2])], "planar layout"),
            (RGB, &[(259, &[2])], "compression CCITT 1D"),
            (RGB, &[(259, &[3])], "compression Group 3 fax"),
            (RGB, &[(259, &[6])], "compression old-style JPEG"),
            (RGB, &[(259, &[34712])], "compression JPEG 2000"),
            (RGB, &[(259, &[9])], "compression 9"),
            (RGB, &[(339, &[3, 3, 3])], "floating-point samples"),
            (RGB, &[(339, &[2])], "signed samples"),
            (RGB, &[(258, &[5, 6, 5])], "5/6/5-bit samples"),
            (RGB, &[(262, &[3])], "palette colour"),
            (RGB, &[(262, &[4])], "transparency mask"),
            (RGB, &[(262, &[8])], "CIELab colour"),
            (RGB, &[(266, &[2])], "FillOrder 2"),
            (RGB, &[(277, &[4]), (338, &[1])], "premultiplied alpha"),
            (RGB, &[(277, &[5])], "2 extra samples"),
            (RGB, &[(317, &[3])], "predictor 3"),
            (RGB, &[(262, &[6])], "YCbCr without JPEG compression"),
            (cmyk, &[(258, &[16])], "CMYK with 16-bit samples"),
            (cmyk, &[(258, &[8]), (277, &[5])], "CMYK with alpha"),
            (
                grey,
                &[(262, &[0]), (258, &[8]), (277, &[2])],
                "WhiteIsZero with alpha",
            ),
            (grey, &[(258, &[4])], "4-bit samples"),
            (RGB, &[(258, &[32])], "32-bit samples"),
            (grey, &[(258, &[8]), (259, &[4])], "Group 4 fax at 8 bits"),
        ];
        for (base, changes, variant) in cases {
            let err = check(&with(base, changes)).unwrap_err();
            assert_eq!(err.message, format!("TIFF with {variant} isn't supported."));
        }
        let big = b"II+\0\x08\0\0\0\0\0\0\0\x10\0\0\0\0\0\0\0";
        assert_eq!(
            check(big).unwrap_err().message,
            "TIFF with BigTIFF isn't supported."
        );
        assert_eq!(
            check(&with(RGB, &[(262, &[5])])).unwrap_err().code,
            "corrupt_image"
        );
        assert_eq!(check(b"II*\0\xff\0\0\0").unwrap_err().code, "corrupt_image");
    }
}
