//! Damaged inputs: a seeded corpus of 1,000 mutated files of every input type, none of which may
//! panic, hang over 10 s or take the process past 2 GiB. Slow, so ignored by default:
//! `cargo test --release --test corpus -- --ignored` (CI runs it on Linux).

mod common;

use std::fs;
use std::path::Path;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use common::*;
use image_convert::Options;
use image_convert::engine::convert;

/// One small, valid file of each kind the fixtures don't cover, named so that detection by
/// extension (NEF, ARW, PEF) applies.
fn generated() -> Vec<(String, Vec<u8>)> {
    let rgb = gradient(24, 16, 3);
    let jpg = jpeg(24, 16, jpeg_encoder::ColorType::Rgb, &rgb, |_| {});
    let preview = |len: usize| vec![(0x201, 4, 1, V::Blob(0)), (0x202, 4, 1, V::N(len as u32))];
    let mut raf = b"FUJIFILMCCD-RAW 0201FF383501".to_vec();
    raf.resize(100, 0);
    raf[84..88].copy_from_slice(&100u32.to_be_bytes());
    raf[88..92].copy_from_slice(&(jpg.len() as u32).to_be_bytes());
    raf.extend(&jpg);
    let mut dng = preview(jpg.len());
    dng.push((0xC612, 1, 4, V::N(0x0104)));
    let svg = br##"<svg xmlns="http://www.w3.org/2000/svg" width="40" height="30"><defs><linearGradient id="g"><stop offset="0" stop-color="#f00"/><stop offset="1" stop-color="#00f"/></linearGradient></defs><rect width="40" height="30" fill="url(#g)"/><circle cx="20" cy="15" r="9" fill="#0f0" opacity=".5"/></svg>"##;
    let svgz = {
        use std::io::Write;
        let mut e = flate2::write::GzEncoder::new(vec![], flate2::Compression::fast());
        e.write_all(svg).unwrap();
        e.finish().unwrap()
    };
    let indices = (0..24 * 16).map(|i| (i % 6) as u8).collect();
    let png_rgb = png(
        24,
        16,
        png::ColorType::Rgb,
        png::BitDepth::Eight,
        &rgb,
        |_| {},
    );
    vec![
        ("a.png".into(), png_rgb.clone()),
        (
            "a.gif".into(),
            gif((24, 16), Some(5), &[(0, 0, 24, 16, indices)]),
        ),
        (
            "a.bmp".into(),
            bmp(
                40,
                (24, 16),
                24,
                0,
                [0; 4],
                &[],
                &bmp_rows(
                    &rgb.chunks(72).map(<[u8]>::to_vec).collect::<Vec<_>>(),
                    false,
                ),
            ),
        ),
        ("a.ico".into(), ico(&[(24, 16, 32, png_rgb)])),
        ("a.webp".into(), webp_lossless(24, 16, &gradient(24, 16, 4))),
        ("a.jpg".into(), jpg.clone()),
        ("a.svg".into(), svg.to_vec()),
        ("a.svgz".into(), svgz),
        ("a.emf".into(), plain_emf()),
        ("a.wmf".into(), placeable_wmf()),
        (
            "a.cr2".into(),
            raw_tiff(
                42,
                &[b"CR\x02\0", &jpg],
                &[
                    (0x111, 4, 1, V::Blob(1)),
                    (0x117, 4, 1, V::N(jpg.len() as u32)),
                    (259, 3, 1, V::N(6)),
                ],
                &[],
            ),
        ),
        (
            "a.nef".into(),
            raw_tiff(42, &[&jpg], &preview(jpg.len()), &[]),
        ),
        (
            "a.arw".into(),
            raw_tiff(42, &[&jpg], &[], &[preview(jpg.len())]),
        ),
        (
            "a.pef".into(),
            raw_tiff(42, &[&jpg], &preview(jpg.len()), &[]),
        ),
        ("a.dng".into(), raw_tiff(42, &[&jpg], &dng, &[])),
        (
            "a.rw2".into(),
            raw_tiff(
                0x55,
                &[&jpg],
                &[(0x2E, 7, jpg.len() as u32, V::Blob(0))],
                &[],
            ),
        ),
        ("a.raf".into(), raf),
        ("a.cr3".into(), cr3(&jpg, &jpg, &exif_orientation(6))),
    ]
}

/// Every committed fixture image, and the generated files.
fn seeds() -> Vec<(String, Vec<u8>)> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let mut seeds: Vec<_> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| {
            let ext = p.extension().unwrap_or_default().to_string_lossy();
            ["png", "jpg", "tif", "jxl", "heic", "avif", "emf"].contains(&&*ext)
        })
        .map(|p| {
            (
                p.file_name().unwrap().to_string_lossy().into_owned(),
                fs::read(p).unwrap(),
            )
        })
        .collect();
    seeds.sort();
    seeds.extend(generated());
    seeds.retain(|(_, data)| !data.is_empty());
    seeds
}

/// xorshift64*: the same corpus on every run and OS.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
}

/// `data` damaged one of five ways: bytes flipped, cut short, a run repeated, a 32-bit field
/// set to an extreme, or a run zeroed. The first 16 bytes are left alone half the time, so most
/// mutants still reach their decoder.
fn mutate(data: &[u8], rng: &mut Rng) -> Vec<u8> {
    let mut out = data.to_vec();
    let from = if rng.below(2) == 0 {
        16.min(out.len())
    } else {
        0
    };
    let at = |rng: &mut Rng, len: usize| from + rng.below(len - from);
    match rng.below(5) {
        0 => {
            for _ in 0..1 + rng.below(8) {
                let i = at(rng, out.len());
                out[i] ^= 1 << rng.below(8);
            }
        }
        1 => out.truncate(at(rng, out.len())),
        2 => {
            let i = at(rng, out.len());
            let run = out[i..(i + 1 + rng.below(64)).min(out.len())].to_vec();
            out.splice(
                i..i,
                run.iter()
                    .cycle()
                    .take(run.len() * (1 + rng.below(8)))
                    .copied(),
            );
        }
        3 if out.len() >= from + 4 => {
            let i = at(rng, out.len() - 3);
            let v = [0xFFFF_FFFFu32, 0x7FFF_FFFF, 0x8000_0000, 0, 0x0001_0000][rng.below(5)];
            out[i..i + 4].copy_from_slice(&v.to_be_bytes());
        }
        _ => {
            let i = at(rng, out.len());
            let end = (i + 1 + rng.below(256)).min(out.len());
            out[i..end].fill(0);
        }
    }
    out
}

/// Peak resident memory of this process, where the OS reports it.
fn peak_rss() -> Option<u64> {
    let status = fs::read_to_string("/proc/self/status").ok()?;
    let line = status.lines().find(|l| l.starts_with("VmHWM:"))?;
    let kb: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
    Some(kb * 1024)
}

#[test]
#[ignore = "slow: 1,000 conversions"]
fn a_thousand_damaged_files_never_crash_hang_or_blow_up() {
    let seeds = seeds();
    let mut rng = Rng(0x1A2B_3C4D_5E6F_7081);
    let mut codes = std::collections::BTreeMap::<&str, usize>::new();
    for n in 0..1000 {
        let (name, seed) = &seeds[n % seeds.len()];
        let data = mutate(seed, &mut rng);
        let (done, finished) = mpsc::channel();
        let input = name.clone();
        let copy = data.clone();
        thread::spawn(move || {
            let _ = done.send(convert(input, &copy, &Options::default()).0);
        });
        let file = finished
            .recv_timeout(Duration::from_secs(10))
            .unwrap_or_else(|_| panic!("mutant {n} of {name} took over 10 s"));
        let code = file.error.as_ref().map_or("converted", |e| e.code);
        if code == "internal" {
            let at = std::env::temp_dir().join(format!("mutant-{n}-{name}"));
            fs::write(&at, &data).unwrap();
            panic!(
                "mutant {n} of {name} panicked: {:?} (saved as {})",
                file.error,
                at.display()
            );
        }
        *codes.entry(code).or_default() += 1;
    }
    println!("{codes:?}");
    if let Some(peak) = peak_rss() {
        assert!(peak < 2 << 30, "peak RSS {peak} bytes");
    }
}
