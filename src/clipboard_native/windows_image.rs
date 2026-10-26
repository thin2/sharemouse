use super::*;
use ::windows::Win32::{Graphics::Imaging::*, System::Com::*};
use lan_mouse_data::clipboard_image::{MAX_IMAGE, bounded_dimensions, inspect};

// COM objects are created and destroyed on this worker thread.
pub(super) fn preview(image: &Image) -> io::Result<Vec<u8>> {
    let (_, expected_w, expected_h) = inspect(&image.bytes)?;
    unsafe {
        let result = CoInitializeEx(None, COINIT_MULTITHREADED);
        // RPC_E_CHANGED_MODE means this thread already has an apartment.
        if result.is_err() && result.0 != 0x80010106_u32 as i32 {
            return Err(io::Error::other(result.to_string()));
        }
        struct Apartment(bool);
        impl Drop for Apartment {
            fn drop(&mut self) {
                if self.0 {
                    unsafe {
                        CoUninitialize();
                    }
                }
            }
        }
        let _apartment = Apartment(result.is_ok());
        let decode = || -> ::windows::core::Result<_> {
            let factory: IWICImagingFactory =
                CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)?;
            let stream = factory.CreateStream()?;
            stream.InitializeFromMemory(&image.bytes)?;
            let decoder = factory.CreateDecoderFromStream(
                &stream,
                ptr::null(),
                WICDecodeMetadataCacheOnLoad,
            )?;
            let frame = decoder.GetFrame(0)?;
            let (mut w, mut h) = (0, 0);
            frame.GetSize(&mut w, &mut h)?;
            if bounded_dimensions(w, h).is_err() || (w, h) != (expected_w, expected_h) {
                return Err(::windows::core::Error::from_hresult(
                    ::windows::core::HRESULT(0x8007000d_u32 as i32),
                ));
            }
            let converter = factory.CreateFormatConverter()?;
            converter.Initialize(
                &frame,
                &GUID_WICPixelFormat32bppRGBA,
                WICBitmapDitherTypeNone,
                None,
                0.0,
                WICBitmapPaletteTypeCustom,
            )?;
            let mut rgba = vec![0; w as usize * h as usize * 4];
            converter.CopyPixels(ptr::null(), w * 4, &mut rgba)?;
            Ok((w, h, rgba))
        };
        let (w, h, rgba) = decode().map_err(io::Error::other)?;
        let bytes = super::super::raster::encode(w, h, &rgba)?;
        if bytes.len() > MAX_IMAGE {
            return Err(io::ErrorKind::InvalidData.into());
        }
        Ok(bytes)
    }
}
