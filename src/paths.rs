//! Files on disk: the inputs that paths and folders name, in order, loaded within the size
//! limit, and the path each output is written to.

use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use crate::error::Error;

/// The largest input read, stdin included.
pub const MAX_FILE: u64 = 512 << 20;

/// The extensions a folder is converted for; a file named on its own is read whatever its name.
const EXTENSIONS: [&str; 31] = [
    "png", "jpg", "jpeg", "jpe", "jfif", "gif", "bmp", "dib", "ico", "webp", "tif", "tiff", "jxl",
    "heic", "heif", "hif", "avif", "svg", "svgz", "emf", "emz", "wmf", "wmz", "cr2", "cr3", "nef",
    "arw", "dng", "raf", "rw2", "pef",
];

/// One file to convert.
#[derive(Debug)]
pub struct Input {
    /// The path as given, a folder entry joined to its folder, or `-` for stdin.
    pub path: PathBuf,
    /// The folder given for a folder entry.
    pub folder: Option<PathBuf>,
    /// Why a folder couldn't be listed.
    error: Option<Error>,
}

/// Where outputs go (`-o`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// Beside each file, or in a folder's `converted` subfolder.
    Beside,
    Dir(PathBuf),
    File(PathBuf),
    Stdout,
}

impl Target {
    /// `-o` read as stdout (`-`), a folder (an existing one, or a path ending in `/` or `\`) or
    /// a file.
    pub fn new(output: Option<&Path>) -> Self {
        match output {
            None => Target::Beside,
            Some(p) if p.as_os_str() == "-" => Target::Stdout,
            Some(p)
                if p.is_dir()
                    || matches!(p.as_os_str().as_encoded_bytes().last(), Some(b'/' | b'\\')) =>
            {
                Target::Dir(p.into())
            }
            Some(p) => Target::File(p.into()),
        }
    }
}

/// The files `paths` name, in order: `-` is stdin; a folder gives its files with an image
/// extension (any case), sorted by name in byte order, skipping hidden names, its own
/// `converted` subfolder and `skip` (an output folder inside it); with `recursive` its
/// subfolders come in place, depth-first, without following symlinked folders. A path given
/// twice is converted twice.
pub fn expand(paths: &[PathBuf], recursive: bool, skip: Option<&Path>) -> Vec<Input> {
    let skip = skip.and_then(|s| fs::canonicalize(s).ok());
    let mut inputs = vec![];
    for path in paths {
        if path.as_os_str() != "-" && path.is_dir() {
            folder(path, recursive, skip.as_deref(), &mut inputs);
        } else {
            inputs.push(Input::new(path.clone(), None, None));
        }
    }
    inputs
}

/// One path read as a file, never as a folder or stream (see [`Input::file_only`]).
pub fn file(path: PathBuf) -> Input {
    Input::new(path, None, None).file_only()
}

/// Walks with a stack of open listings rather than recursion, so no folder tree is too deep.
fn folder(root: &Path, recursive: bool, skip: Option<&Path>, inputs: &mut Vec<Input>) {
    let mut open = vec![list(root, root, inputs)];
    while let Some(listing) = open.last_mut() {
        let Some(entry) = listing.next() else {
            open.pop();
            continue;
        };
        let name = entry.file_name();
        let path = entry.path();
        if name.as_encoded_bytes().starts_with(b".") {
            continue;
        }
        if path.is_dir() {
            let own_output = open.len() == 1 && name.eq_ignore_ascii_case("converted");
            let is_link = entry.file_type().is_ok_and(|t| t.is_symlink());
            let skipped = skip.is_some_and(|s| fs::canonicalize(&path).is_ok_and(|p| p == s));
            if recursive && !is_link && !own_output && !skipped {
                open.push(list(&path, root, inputs));
            }
        } else if Path::new(&name)
            .extension()
            .is_some_and(|ext| EXTENSIONS.iter().any(|e| ext.eq_ignore_ascii_case(e)))
        {
            inputs.push(Input::new(path, Some(root.into()), None));
        }
    }
}

/// A folder's entries sorted by name; one that can't be listed is an `unreadable` input.
fn list(dir: &Path, root: &Path, inputs: &mut Vec<Input>) -> std::vec::IntoIter<fs::DirEntry> {
    let entries = fs::read_dir(dir).and_then(|list| list.collect::<io::Result<Vec<_>>>());
    match entries {
        Ok(mut entries) => {
            entries.sort_by_key(fs::DirEntry::file_name);
            entries.into_iter()
        }
        Err(e) => {
            let error = Error::unreadable(&dir.to_string_lossy(), e);
            let folder = (dir != root).then(|| root.into());
            inputs.push(Input::new(dir.into(), folder, Some(error)));
            vec![].into_iter()
        }
    }
}

impl Input {
    fn new(path: PathBuf, folder: Option<PathBuf>, error: Option<Error>) -> Self {
        Self {
            path,
            folder,
            error,
        }
    }

    /// This input, made `unreadable` unless it is a regular file or missing (which `load`
    /// reports): a server whose stdin carries its messages can't read `-`, and pipes and
    /// devices could block it.
    pub fn file_only(mut self) -> Self {
        let regular = fs::metadata(&self.path).map_or(true, |m| m.is_file());
        if self.error.is_none() && (self.path.as_os_str() == "-" || !regular) {
            self.error = Some(Error::unreadable(&self.name(), "it isn't a regular file"));
        }
        self
    }

    /// The path as results show it (non-UTF-8 parts as U+FFFD).
    pub fn name(&self) -> String {
        self.path.to_string_lossy().into_owned()
    }

    /// The whole file or stdin, refused from its size before reading when over 512 MiB.
    pub fn load(&self) -> Result<Vec<u8>, Error> {
        if let Some(e) = &self.error {
            return Err(e.clone());
        }
        let name = self.name();
        let unreadable = |e| Error::unreadable(&name, e);
        let (reader, size): (Box<dyn Read>, u64) = if self.path.as_os_str() == "-" {
            (Box::new(io::stdin().lock()), 0)
        } else {
            let file = File::open(&self.path).map_err(|e| match e.kind() {
                _ if fs::symlink_metadata(&self.path).is_ok() => unreadable(e),
                _ if name.contains(['*', '?']) => Error::wildcard(&name),
                io::ErrorKind::NotFound => Error::not_found(&name),
                _ => unreadable(e),
            })?;
            let size = file.metadata().map_err(unreadable)?.len();
            if size > MAX_FILE {
                return Err(Error::too_large());
            }
            (Box::new(file), size)
        };
        let mut bytes = Vec::with_capacity(size as usize);
        reader
            .take(MAX_FILE + 1)
            .read_to_end(&mut bytes)
            .map_err(unreadable)?;
        if bytes.len() as u64 > MAX_FILE {
            return Err(Error::too_large());
        }
        Ok(bytes)
    }

    /// Where this input's output with extension `ext` goes: `None` for stdout. Beside the input
    /// (folder entries under `<folder>/converted/`) or under `-o DIR`, the first of
    /// `<stem>.<ext>`, `<stem>-1.<ext>` … `<stem>-999.<ext>` that doesn't exist; `-o FILE` as
    /// given, unless it is the input or exists without `overwrite`.
    pub fn output(
        &self,
        target: &Target,
        ext: &str,
        overwrite: bool,
    ) -> Result<Option<PathBuf>, Error> {
        let rel_dir = || {
            let folder = self.folder.as_deref()?;
            Some(self.path.strip_prefix(folder).ok()?.parent()?.to_path_buf())
        };
        let dir = match (target, &self.folder) {
            (Target::Stdout, _) => return Ok(None),
            (Target::File(file), _) => {
                let same = |a: &Path, b: &Path| {
                    fs::canonicalize(a).is_ok_and(|a| fs::canonicalize(b).is_ok_and(|b| a == b))
                };
                return if same(file, &self.path) {
                    Err(Error::output_is_input())
                } else if exists(file) && !overwrite {
                    Err(Error::output_exists(&file.to_string_lossy()))
                } else {
                    Ok(Some(file.clone()))
                };
            }
            (Target::Beside, Some(folder)) => {
                folder.join("converted").join(rel_dir().unwrap_or_default())
            }
            (Target::Beside, None) => self
                .path
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_default(),
            (Target::Dir(dir), _) => dir.join(rel_dir().unwrap_or_default()),
        };
        let stem = match self.path.file_stem() {
            _ if self.path.as_os_str() == "-" => "stdin".as_ref(),
            Some(stem) => stem,
            None => "image".as_ref(),
        };
        // Built as an OsString, so a name that isn't UTF-8 is written as it is.
        let name = |suffix: String| {
            let mut name = stem.to_os_string();
            name.push(suffix);
            dir.join(name)
        };
        let first = name(format!(".{ext}"));
        (0..1000)
            .map(|n| match n {
                0 => first.clone(),
                n => name(format!("-{n}.{ext}")),
            })
            .find(|p| !exists(p))
            .map(Some)
            .ok_or_else(|| Error::output_exists(&first.to_string_lossy()))
    }
}

/// Whether anything, even a dangling symlink, is at `path`.
fn exists(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh folder holding empty files at `files` (subfolders made as needed).
    fn tree(test: &str, files: &[&str]) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("image-convert-{test}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        for file in files {
            let path = root.join(file);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            File::create(path).unwrap();
        }
        root
    }

    fn rel(path: &Path, root: &Path) -> String {
        path.strip_prefix(root)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/")
    }

    fn names(inputs: &[Input], root: &Path) -> Vec<String> {
        inputs.iter().map(|i| rel(&i.path, root)).collect()
    }

    #[test]
    fn a_folder_gives_its_images_in_byte_order() {
        let files = [
            "b.HEIC",
            "a.svgz",
            "B.jpg",
            "notes.txt",
            ".hidden.png",
            "converted/old.jpg",
            "out/x.png",
            "sub/c.dng",
            "sub/converted/d.png",
            "sub/.d/e.png",
            "x.Tif",
        ];
        let root = tree("folder", &files);
        let dir = std::slice::from_ref(&root);
        let flat = expand(dir, false, None);
        assert_eq!(names(&flat, &root), ["B.jpg", "a.svgz", "b.HEIC", "x.Tif"]);
        assert!(flat.iter().all(|i| i.folder.as_deref() == Some(&*root)));
        let deep = expand(dir, true, Some(&root.join("out")));
        assert_eq!(
            names(&deep, &root),
            [
                "B.jpg",
                "a.svgz",
                "b.HEIC",
                "sub/c.dng",
                "sub/converted/d.png",
                "x.Tif"
            ]
        );
        let twice = expand(
            &[root.join("x.Tif"), "-".into(), root.join("x.Tif")],
            false,
            None,
        );
        assert_eq!(twice[1].path, Path::new("-"));
        assert_eq!(twice[0].path, twice[2].path);
        assert!(twice.iter().all(|i| i.folder.is_none()));
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_folders_are_not_followed() {
        let root = tree("links", &["real/a.png", "dir/b.png"]);
        std::os::unix::fs::symlink(root.join("real"), root.join("dir/link")).unwrap();
        std::os::unix::fs::symlink(root.join("real/a.png"), root.join("dir/c.png")).unwrap();
        let found = expand(&[root.join("dir")], true, None);
        assert_eq!(names(&found, &root), ["dir/b.png", "dir/c.png"]);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn missing_files_are_not_found_or_a_wildcard_and_big_ones_too_large() {
        let load = |name: &str| expand(&[name.into()], false, None)[0].load().unwrap_err();
        assert_eq!(load("no-such.png"), Error::not_found("no-such.png"));
        assert_eq!(load("photos/*.png"), Error::wildcard("photos/*.png"));
        let root = tree("load", &["a.png", "big.png"]);
        assert_eq!(
            expand(&[root.join("a.png")], false, None)[0].load(),
            Ok(vec![])
        );
        File::options()
            .write(true)
            .open(root.join("big.png"))
            .unwrap()
            .set_len(MAX_FILE + 1)
            .unwrap();
        let big = expand(&[root.join("big.png")], false, None)[0].load();
        assert_eq!(big, Err(Error::too_large()));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn outputs_go_beside_into_converted_or_under_o_with_suffixes() {
        let root = tree(
            "outputs",
            &["a.heic", "a.jpg", "a-1.jpg", "p/sub/b.png", "o/b.png"],
        );
        let file = &expand(&[root.join("a.heic")], false, None)[0];
        let entry = &expand(&[root.join("p")], true, None)[0];
        let out = |input: &Input, target: &Target| {
            let path = input.output(target, "jpg", false).unwrap().unwrap();
            rel(&path, &root)
        };
        assert_eq!(out(file, &Target::Beside), "a-2.jpg");
        assert_eq!(out(entry, &Target::Beside), "p/converted/sub/b.jpg");
        let dir = Target::new(Some(&root.join("o")));
        assert_eq!(dir, Target::Dir(root.join("o")));
        assert_eq!(out(file, &dir), "o/a.jpg");
        assert_eq!(out(entry, &dir), "o/sub/b.jpg");
        let new_dir = Target::new(Some(&root.join("new/")));
        assert_eq!(out(file, &new_dir), "new/a.jpg");
        let stdin = &expand(&["-".into()], false, None)[0];
        assert_eq!(out(stdin, &dir), "o/stdin.jpg");
        assert_eq!(
            file.output(&Target::new(Some(Path::new("-"))), "jpg", false),
            Ok(None)
        );

        let to = |name: &str| Target::new(Some(&root.join(name)));
        assert_eq!(out(file, &to("c.jpg")), "c.jpg");
        let taken = file.output(&to("a.jpg"), "jpg", false);
        let a = root.join("a.jpg").to_string_lossy().into_owned();
        assert_eq!(taken, Err(Error::output_exists(&a)));
        assert_eq!(
            file.output(&to("a.jpg"), "jpg", true).unwrap().unwrap(),
            root.join("a.jpg")
        );
        let itself = file.output(&to("./a.heic"), "jpg", true);
        assert_eq!(itself, Err(Error::output_is_input()));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn suffixes_stop_at_999() {
        let mut files = vec!["x.png".to_string(), "x.jpg".to_string()];
        files.extend((1..1000).map(|n| format!("x-{n}.jpg")));
        let refs: Vec<_> = files.iter().map(String::as_str).collect();
        let root = tree("suffixes", &refs);
        let input = &expand(&[root.join("x.png")], false, None)[0];
        let first = root.join("x.jpg").to_string_lossy().into_owned();
        assert_eq!(
            input.output(&Target::Beside, "jpg", false),
            Err(Error::output_exists(&first))
        );
        fs::remove_dir_all(root).unwrap();
    }
}
