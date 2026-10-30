//! EMF, EMF+ and placeable WMF through GDI+, drawn anti-aliased onto a transparent bitmap at the
//! size asked for (spec §3.5).

use std::sync::OnceLock;

use windows::Win32::Graphics::GdiPlus::{
    BitmapData, EmfTypeEmfPlusDual, GdipBitmapLockBits, GdipBitmapUnlockBits, GdipConvertToEmfPlus,
    GdipCreateBitmapFromScan0, GdipCreateMetafileFromStream, GdipDeleteGraphics, GdipDisposeImage,
    GdipDrawImageRectI, GdipGetImageGraphicsContext, GdipGetMetafileHeaderFromMetafile,
    GdipGraphicsClear, GdipSetInterpolationMode, GdipSetPageUnit, GdipSetSmoothingMode,
    GdipSetTextRenderingHint, GdiplusStartup, GdiplusStartupInput, GpBitmap, GpGraphics,
    GpMetafile, ImageLockModeRead, InterpolationModeHighQualityBicubic, MetafileHeader,
    MetafileTypeEmfPlusDual, MetafileTypeEmfPlusOnly, Ok as DONE, Rect, SmoothingModeAntiAlias,
    Status, TextRenderingHintAntiAliasGridFit, UnitPixel,
};
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT, DPI_AWARENESS_CONTEXT_SYSTEM_AWARE, SetThreadDpiAwarenessContext,
};
use windows::Win32::UI::Shell::SHCreateMemStream;
use windows::core::PCWSTR;

use crate::detect::Kind;
use crate::error::Error;
use crate::{Layout, Options, Pixels, Samples, resize};

/// GDI+'s PixelFormat32bppARGB (straight alpha), which the bindings leave out.
const ARGB: i32 = 0x0026_200A;

/// A GDI+ object, disposed of when it drops.
struct Object<T>(*mut T, unsafe fn(*mut T) -> Status);

impl<T> Drop for Object<T> {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { (self.1)(self.0) };
        }
    }
}

unsafe fn delete(graphics: *mut GpGraphics) -> Status {
    unsafe { GdipDeleteGraphics(graphics) }
}

/// The drawing's own size in px at 96 DPI, from its header: the EMF frame (0.01 mm) or the
/// placeable WMF bounding box (in its units per inch).
fn own_size(data: &[u8], kind: Kind) -> Result<[u32; 2], Error> {
    let label = kind.label();
    let cut = || Error::corrupt(&label, "its header is cut short");
    let (extent, per_inch) = if kind == Kind::Wmf {
        if !data.starts_with(b"\xd7\xcd\xc6\x9a") {
            return Err(Error::unsupported_variant("WMF", "no placeable header"));
        }
        let word = |at: usize| {
            let b = data.get(at..at + 2).ok_or_else(cut)?;
            Ok::<_, Error>(i16::from_le_bytes([b[0], b[1]]))
        };
        let [left, top, right, bottom] = [6, 8, 10, 12].map(word);
        let inch = f64::from(word(14)? as u16);
        let size = [right? as i32 - left? as i32, bottom? as i32 - top? as i32];
        (size, inch)
    } else {
        let long = |at: usize| {
            let b = data.get(at..at + 4).ok_or_else(cut)?;
            Ok::<_, Error>(i32::from_le_bytes(b.try_into().unwrap()))
        };
        let [left, top, right, bottom] = [24, 28, 32, 36].map(long);
        let size = [right?.saturating_sub(left?), bottom?.saturating_sub(top?)];
        (size, 2540.0)
    };
    let px = extent.map(|e| (f64::from(e.unsigned_abs()) * 96.0 / per_inch).ceil());
    if per_inch == 0.0 || px.contains(&0.0) {
        return Err(Error::corrupt(
            &label,
            "its header gives a size of 0 pixels",
        ));
    }
    Ok(px.map(|p| p.min(f64::from(u32::MAX)) as u32))
}

/// Renders the drawing at the size `options` gives it; returns the pixels and its own size.
pub fn render(data: &[u8], kind: Kind, options: &Options) -> Result<(Pixels, (u32, u32)), Error> {
    let own = own_size(data, kind)?;
    let (width, height, reduced) = resize::vector_target(own, options)?;
    let label = kind.label();
    let failed = |status: Status| Error::corrupt(&label, format!("GDI+ status {}", status.0));
    let check = |status: Status| {
        if status == DONE {
            Ok(())
        } else {
            Err(failed(status))
        }
    };
    static STARTED: OnceLock<Status> = OnceLock::new();
    check(*STARTED.get_or_init(|| {
        let input = GdiplusStartupInput {
            GdiplusVersion: 1,
            ..Default::default()
        };
        let mut token = 0;
        unsafe { GdiplusStartup(&mut token, &input, std::ptr::null_mut()) }
    }))?;

    let mut rgba = vec![0; width as usize * height as usize * 4];
    unsafe {
        // GDI+ mixes the logical and the physical resolution in a thread that isn't DPI-aware,
        // and EMF+ conversion then misplaces the drawing on a scaled display; so the thread is
        // DPI-aware while it draws.
        struct Restore(DPI_AWARENESS_CONTEXT);
        impl Drop for Restore {
            fn drop(&mut self) {
                unsafe { SetThreadDpiAwarenessContext(self.0) };
            }
        }
        let _restore = Restore(SetThreadDpiAwarenessContext(
            DPI_AWARENESS_CONTEXT_SYSTEM_AWARE,
        ));
        let stream = SHCreateMemStream(Some(data)).ok_or_else(|| failed(Status(3)))?;
        let mut metafile = Object(std::ptr::null_mut(), dispose_metafile);
        check(GdipCreateMetafileFromStream(&stream, &mut metafile.0))?;
        let mut header = MetafileHeader::default();
        check(GdipGetMetafileHeaderFromMetafile(metafile.0, &mut header))?;
        let mut bitmap = Object(std::ptr::null_mut(), dispose_bitmap);
        let (w, h) = (width as i32, height as i32);
        check(GdipCreateBitmapFromScan0(
            w,
            h,
            0,
            ARGB,
            None,
            &mut bitmap.0,
        ))?;
        let mut graphics = Object(std::ptr::null_mut(), delete);
        check(GdipGetImageGraphicsContext(
            bitmap.0.cast(),
            &mut graphics.0,
        ))?;
        check(GdipGraphicsClear(graphics.0, 0))?;
        // The default unit follows the screen's DPI; the target size is in pixels.
        check(GdipSetPageUnit(graphics.0, UnitPixel))?;
        check(GdipSetSmoothingMode(graphics.0, SmoothingModeAntiAlias))?;
        check(GdipSetTextRenderingHint(
            graphics.0,
            TextRenderingHintAntiAliasGridFit,
        ))?;
        check(GdipSetInterpolationMode(
            graphics.0,
            InterpolationModeHighQualityBicubic,
        ))?;
        // Plain EMF and WMF records draw without anti-aliasing unless converted to EMF+ first;
        // when the conversion fails the original is drawn.
        let mut plus = Object(std::ptr::null_mut(), dispose_metafile);
        let plain = ![MetafileTypeEmfPlusOnly, MetafileTypeEmfPlusDual].contains(&header.Type);
        if plain {
            let mut flag = 0;
            let converted = GdipConvertToEmfPlus(
                graphics.0,
                metafile.0,
                &mut flag,
                EmfTypeEmfPlusDual,
                PCWSTR::null(),
                &mut plus.0,
            );
            if converted != DONE {
                plus.0 = std::ptr::null_mut();
            }
        }
        let drawn = if plus.0.is_null() { &metafile } else { &plus };
        check(GdipDrawImageRectI(graphics.0, drawn.0.cast(), 0, 0, w, h))?;
        drop(graphics);

        let rect = Rect {
            X: 0,
            Y: 0,
            Width: w,
            Height: h,
        };
        let mut locked = BitmapData::default();
        let read = ImageLockModeRead.0 as u32;
        check(GdipBitmapLockBits(bitmap.0, &rect, read, ARGB, &mut locked))?;
        let row = width as usize * 4;
        for (y, out) in rgba.chunks_exact_mut(row).enumerate() {
            let start = locked
                .Scan0
                .cast::<u8>()
                .offset(y as isize * locked.Stride as isize);
            let bgra = std::slice::from_raw_parts(start, row);
            for (o, i) in out
                .as_chunks_mut::<4>()
                .0
                .iter_mut()
                .zip(bgra.as_chunks::<4>().0)
            {
                *o = [i[2], i[1], i[0], i[3]];
            }
        }
        check(GdipBitmapUnlockBits(bitmap.0, &mut locked))?;
    }
    let pixels = Pixels {
        width,
        height,
        layout: Layout::Rgba,
        samples: Samples::U8(rgba),
        bit_depth: 8,
        colour: None,
        exif: None,
        orientation: 1,
        frames: 1,
        warnings: reduced.into_iter().collect(),
    };
    Ok((pixels, (own[0], own[1])))
}

unsafe fn dispose_metafile(metafile: *mut GpMetafile) -> Status {
    unsafe { GdipDisposeImage(metafile.cast()) }
}

unsafe fn dispose_bitmap(bitmap: *mut GpBitmap) -> Status {
    unsafe { GdipDisposeImage(bitmap.cast()) }
}
