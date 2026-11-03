//! HEIC and AVIF through Windows Imaging Component: the HEIF Image Extension's decoder with the
//! HEVC or AV1 Video Extension's Media Foundation decoder behind it (spec §3.3).

use windows::Win32::Foundation::WINCODEC_ERR_COMPONENTNOTFOUND;
use windows::Win32::Graphics::Imaging::*;
use windows::Win32::Media::MediaFoundation::{
    IMFActivate, MF_E_TOPO_CODEC_NOT_FOUND, MFMediaType_Video, MFT_CATEGORY_VIDEO_DECODER,
    MFT_ENUM_FLAG_ALL, MFT_REGISTER_TYPE_INFO, MFTEnumEx, MFVideoFormat_AV1, MFVideoFormat_HEVC,
};
use windows::Win32::System::Com::StructuredStorage::{PROPVARIANT, PropVariantClear};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree,
    CoUninitialize,
};
use windows::Win32::System::Variant::VT_UI2;
use windows::core::{Interface, w};

use crate::bmff::Heif;
use crate::color::{Colour, describe};
use crate::detect::Kind;
use crate::error::{Codec, Error, Warning};
use crate::{Layout, Pixels, Samples};

/// COM on this thread for as long as it lives (multithreaded, as a worker thread wants).
struct Com(bool);

impl Com {
    fn init() -> Com {
        Com(unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_ok())
    }
}

impl Drop for Com {
    fn drop(&mut self) {
        if self.0 {
            unsafe { CoUninitialize() };
        }
    }
}

fn factory() -> windows::core::Result<IWICImagingFactory> {
    unsafe { CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER) }
}

/// Whether Windows has what it takes to decode `kind`: the HEIF extension, then a Media
/// Foundation decoder for its coding.
pub fn check(kind: Kind) -> Result<(), Error> {
    let _com = Com::init();
    let heic = kind == Kind::Heic;
    let heif = factory().and_then(|f| unsafe { f.CreateComponentInfo(&CLSID_WICHeifDecoder) });
    if heif.is_err() {
        return Err(Error::codec_missing(Codec::WindowsHeifExtension, heic));
    }
    let input = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: if heic {
            MFVideoFormat_HEVC
        } else {
            MFVideoFormat_AV1
        },
    };
    let mut found: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count = 0;
    let listed = unsafe {
        MFTEnumEx(
            MFT_CATEGORY_VIDEO_DECODER,
            MFT_ENUM_FLAG_ALL,
            Some(&input),
            None,
            &mut found,
            &mut count,
        )
    };
    if !found.is_null() {
        // Each activation object is released as the slice drops, then the array is freed.
        unsafe {
            std::ptr::drop_in_place(std::ptr::slice_from_raw_parts_mut(found, count as usize));
            CoTaskMemFree(Some(found.cast()));
        }
    }
    if listed.is_err() || count == 0 {
        return Err(Error::codec_missing(coding(heic), heic));
    }
    Ok(())
}

/// The video extension a HEIC (HEVC) or AVIF (AV1) file needs.
fn coding(heic: bool) -> Codec {
    if heic {
        Codec::WindowsHevc
    } else {
        Codec::WindowsAv1
    }
}

/// Decodes the primary image as RGBA in its own colour space and orientation.
pub fn decode(data: &[u8], heif: &Heif) -> Result<Pixels, Error> {
    check(heif.kind)?;
    let _com = Com::init();
    let heic = heif.kind == Kind::Heic;
    let failed = |e: windows::core::Error| match e.code() {
        WINCODEC_ERR_COMPONENTNOTFOUND => Error::codec_missing(Codec::WindowsHeifExtension, heic),
        MF_E_TOPO_CODEC_NOT_FOUND => Error::codec_missing(coding(heic), heic),
        code => Error::corrupt(
            &heif.kind.label(),
            format!("Windows error 0x{:08X}", code.0 as u32),
        ),
    };
    unsafe {
        let factory = factory().map_err(failed)?;
        let stream = factory.CreateStream().map_err(failed)?;
        stream.InitializeFromMemory(data).map_err(failed)?;
        let decoder = factory
            .CreateDecoderFromStream(&stream, std::ptr::null(), WICDecodeMetadataCacheOnDemand)
            .map_err(failed)?;
        let frame = decoder.GetFrame(0).map_err(failed)?;
        let (mut width, mut height) = (0, 0);
        frame.GetSize(&mut width, &mut height).map_err(failed)?;
        let mut rgba = vec![0; width as usize * height as usize * 4];
        WICConvertBitmapSource(&GUID_WICPixelFormat32bppRGBA, &frame)
            .and_then(|s| s.CopyPixels(std::ptr::null(), width * 4, &mut rgba))
            .map_err(failed)?;

        let mut warnings = Vec::new();
        // The decoder reports alpha AVIF as Bgr32; its alpha plane comes separately.
        if heif.alpha && rgba.iter().skip(3).step_by(4).all(|&a| a == 255) {
            let mut alpha = vec![0; width as usize * height as usize];
            let copied = frame.cast::<IWICBitmapSourceTransform>().and_then(|t| {
                t.CopyPixels(
                    std::ptr::null(),
                    width,
                    height,
                    &GUID_WICPixelFormat8bppAlpha,
                    WICBitmapTransformRotate0,
                    width,
                    &mut alpha,
                )
            });
            match copied {
                Ok(()) => {
                    for (px, a) in rgba.as_chunks_mut::<4>().0.iter_mut().zip(alpha) {
                        px[3] = a;
                    }
                }
                Err(_) => warnings.push(Warning::alpha_dropped()),
            }
        }

        // The decoder doesn't rotate; the orientation is its WICHeifOrientation, else irot/imir.
        let orientation = frame
            .GetMetadataQueryReader()
            .ok()
            .and_then(|reader| {
                let mut value = PROPVARIANT::default();
                reader
                    .GetMetadataByName(w!("/heifProps/{ushort=1}"), &mut value)
                    .ok()?;
                let inner = &value.Anonymous.Anonymous;
                let orientation = (inner.vt == VT_UI2).then(|| inner.Anonymous.uiVal);
                PropVariantClear(&mut value).ok()?;
                orientation
            })
            .filter(|o| (1..=8).contains(o))
            .map_or_else(|| heif.orientation(), |o| o as u8);

        // Media Foundation turns YCbCr described by code points into sRGB itself (HDR tone
        // mapped); RGB (matrix 0) and profile-described images come out in their own colour.
        let colour = match &heif.colour {
            Some(Colour::Cicp(cicp)) if cicp.matrix != 0 => {
                Some(Colour::Converted(describe(heif.colour.as_ref())))
            }
            _ => None,
        };
        Ok(Pixels {
            width,
            height,
            layout: Layout::Rgba,
            samples: Samples::U8(rgba),
            bit_depth: 8,
            colour,
            exif: None,
            orientation,
            frames: 1,
            warnings,
        })
    }
}
