//! HEIC and AVIF through the system's own libheif (1.13 or newer, with its decoder plugins),
//! opened at run time so that nothing of it is linked or shipped (spec §3.3). Only calls that
//! libheif 1.17.6 has are used.

use std::ffi::{CStr, c_char, c_int, c_void};
use std::sync::OnceLock;

use libloading::Library;

use crate::bmff::Heif;
use crate::detect::Kind;
use crate::error::{Codec, Error};
use crate::{Layout, Pixels, Samples};

#[repr(C)]
struct HeifError {
    code: c_int,
    subcode: c_int,
    message: *const c_char,
}

const COMPRESSION_HEVC: c_int = 1;
const COMPRESSION_AV1: c_int = 4;
const COLORSPACE_RGB: c_int = 1;
const CHROMA_INTERLEAVED_RGBA: c_int = 11;
const CHANNEL_INTERLEAVED: c_int = 10;

/// The library and its version, loaded and initialised once; `None` when it isn't installed.
fn library() -> Option<&'static (Library, u8, u8)> {
    static LIBRARY: OnceLock<Option<(Library, u8, u8)>> = OnceLock::new();
    LIBRARY
        .get_or_init(|| unsafe {
            // A bare name: the dynamic loader searches the system's paths, never this folder.
            let lib = Library::new("libheif.so.1").ok()?;
            let version = *lib
                .get::<unsafe extern "C" fn() -> u32>(b"heif_get_version_number")
                .ok()?;
            let version = version();
            let (major, minor) = ((version >> 24) as u8, (version >> 16) as u8);
            if (major, minor) >= (1, 13) {
                // Loads the decoder plugins; kept for the life of the process.
                let init = *lib
                    .get::<unsafe extern "C" fn(Object) -> HeifError>(b"heif_init")
                    .ok()?;
                init(std::ptr::null_mut());
            }
            Some((lib, major, minor))
        })
        .as_ref()
}

/// Whether the system's libheif can decode `kind`.
pub fn check(kind: Kind) -> Result<(), Error> {
    let heic = kind == Kind::Heic;
    let missing = |codec| Error::codec_missing(codec, heic);
    let (_, major, minor) = library().ok_or_else(|| missing(Codec::LinuxNoLibheif))?;
    if (*major, *minor) < (1, 13) {
        return Err(missing(Codec::LinuxOldLibheif(*major, *minor)));
    }
    let format = if heic {
        COMPRESSION_HEVC
    } else {
        COMPRESSION_AV1
    };
    let have =
        unsafe { get::<unsafe extern "C" fn(c_int) -> c_int>(b"heif_have_decoder_for_format") };
    if have.is_ok_and(|have| unsafe { have(format) } != 0) {
        Ok(())
    } else {
        Err(missing(Codec::LinuxNoPlugin))
    }
}

/// Decodes the primary image as upright RGBA (libheif applies irot, imir and clap) in its own
/// colour space.
pub fn decode(data: &[u8], heif: &Heif) -> Result<Pixels, Error> {
    check(heif.kind)?;
    let (width, height, rgba) = unsafe { primary_rgba(data) }
        .map_err(|reason| Error::corrupt(&heif.kind.label(), reason))?;
    Ok(Pixels {
        width,
        height,
        layout: Layout::Rgba,
        samples: Samples::U8(rgba),
        bit_depth: 8,
        colour: None,
        exif: None,
        orientation: 1,
        frames: 1,
        warnings: Vec::new(),
    })
}

type Object = *mut c_void;

/// Releases a libheif object when it drops.
struct Guard(Object, unsafe extern "C" fn(Object));

impl Drop for Guard {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { (self.1)(self.0) };
        }
    }
}

/// The primary image's size and RGBA rows, or libheif's message.
unsafe fn primary_rgba(data: &[u8]) -> Result<(u32, u32, Vec<u8>), String> {
    let call = |e: HeifError| match e.code {
        0 => Ok(()),
        _ => Err(unsafe { CStr::from_ptr(e.message) }
            .to_string_lossy()
            .into_owned()),
    };
    unsafe {
        let alloc: unsafe extern "C" fn() -> Object = get(b"heif_context_alloc")?;
        let read: unsafe extern "C" fn(Object, *const u8, usize, Object) -> HeifError =
            get(b"heif_context_read_from_memory_without_copy")?;
        let primary: unsafe extern "C" fn(Object, *mut Object) -> HeifError =
            get(b"heif_context_get_primary_image_handle")?;
        let decode: unsafe extern "C" fn(Object, *mut Object, c_int, c_int, Object) -> HeifError =
            get(b"heif_decode_image")?;
        let plane: unsafe extern "C" fn(Object, c_int, *mut c_int) -> *const u8 =
            get(b"heif_image_get_plane_readonly")?;
        let width: unsafe extern "C" fn(Object, c_int) -> c_int = get(b"heif_image_get_width")?;
        let height: unsafe extern "C" fn(Object, c_int) -> c_int = get(b"heif_image_get_height")?;

        // Declared in this order, the guards release the image, the handle, then the context.
        let context = Guard(alloc(), get(b"heif_context_free")?);
        call(read(
            context.0,
            data.as_ptr(),
            data.len(),
            std::ptr::null_mut(),
        ))?;
        let mut handle = Guard(std::ptr::null_mut(), get(b"heif_image_handle_release")?);
        call(primary(context.0, &mut handle.0))?;
        let mut image = Guard(std::ptr::null_mut(), get(b"heif_image_release")?);
        let (rgb, rgba) = (COLORSPACE_RGB, CHROMA_INTERLEAVED_RGBA);
        call(decode(
            handle.0,
            &mut image.0,
            rgb,
            rgba,
            std::ptr::null_mut(),
        ))?;
        let mut stride = 0;
        let pixels = plane(image.0, CHANNEL_INTERLEAVED, &mut stride);
        let [w, h] = [width, height].map(|f| f(image.0, CHANNEL_INTERLEAVED));
        let [w, h, stride] = [w, h, stride].map(|v| usize::try_from(v).unwrap_or(0));
        if pixels.is_null() || w == 0 || h == 0 || stride < w * 4 {
            return Err("libheif gave no pixels".into());
        }
        let rows = (0..h).flat_map(|y| std::slice::from_raw_parts(pixels.add(y * stride), w * 4));
        Ok((w as u32, h as u32, rows.copied().collect()))
    }
}

/// A function of the loaded library, which stays loaded, so the pointer stays valid.
unsafe fn get<T: Copy>(name: &[u8]) -> Result<T, String> {
    let (lib, ..) = library().ok_or("libheif isn't loaded")?;
    unsafe { lib.get::<T>(name) }
        .map(|f| *f)
        .map_err(|e| e.to_string())
}
