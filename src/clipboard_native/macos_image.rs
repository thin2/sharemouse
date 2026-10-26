use super::*;
use lan_mouse_data::clipboard_image::{Image, MAX_IMAGE, bounded_dimensions};

#[link(name = "ImageIO", kind = "framework")]
unsafe extern "C" {
    fn CGImageSourceCreateWithData(data: Id, options: Id) -> Id;
    fn CGImageSourceCopyPropertiesAtIndex(source: Id, index: usize, options: Id) -> Id;
    fn CGImageSourceCreateImageAtIndex(source: Id, index: usize, options: Id) -> Id;
    fn CGImageDestinationCreateWithData(data: Id, kind: Id, count: usize, options: Id) -> Id;
    fn CGImageDestinationAddImage(destination: Id, image: Id, properties: Id);
    fn CGImageDestinationFinalize(destination: Id) -> bool;
    static kCGImagePropertyPixelWidth: Id;
    static kCGImagePropertyPixelHeight: Id;
}
#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFDataCreate(allocator: Id, bytes: *const u8, size: isize) -> Id;
    fn CFDataCreateMutable(allocator: Id, capacity: isize) -> Id;
    fn CFDataGetLength(data: Id) -> isize;
    fn CFDataGetBytePtr(data: Id) -> *const u8;
    fn CFDictionaryGetValue(dictionary: Id, key: Id) -> Id;
    fn CFNumberGetValue(number: Id, kind: i32, result: *mut i64) -> bool;
    fn CFRelease(value: Id);
}
#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGImageGetWidth(image: Id) -> usize;
    fn CGImageGetHeight(image: Id) -> usize;
}
struct Owned(Id);
impl Owned {
    fn new(value: Id) -> io::Result<Self> {
        if value.is_null() {
            Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "image decoding failed",
            ))
        } else {
            Ok(Self(value))
        }
    }
}
impl Drop for Owned {
    fn drop(&mut self) {
        unsafe {
            CFRelease(self.0);
        }
    }
}

/// ImageIO decodes only frame zero. The original GIF bytes remain untouched.
pub(super) fn png(bytes: &[u8]) -> io::Result<Vec<u8>> {
    if bytes.len() > MAX_IMAGE {
        return Err(io::ErrorKind::InvalidData.into());
    }
    let _pool = Pool::new();
    unsafe {
        let data = Owned::new(CFDataCreate(
            std::ptr::null_mut(),
            bytes.as_ptr(),
            bytes.len() as isize,
        ))?;
        let source = Owned::new(CGImageSourceCreateWithData(data.0, std::ptr::null_mut()))?;
        let properties = Owned::new(CGImageSourceCopyPropertiesAtIndex(
            source.0,
            0,
            std::ptr::null_mut(),
        ))?;
        let dimension = |key| {
            let number = CFDictionaryGetValue(properties.0, key);
            let mut value = 0_i64;
            if number.is_null() || !CFNumberGetValue(number, 4, &mut value) {
                return Err(io::ErrorKind::InvalidData.into());
            }
            u32::try_from(value).map_err(|_| io::Error::from(io::ErrorKind::InvalidData))
        };
        let (width, height) = bounded_dimensions(
            dimension(kCGImagePropertyPixelWidth)?,
            dimension(kCGImagePropertyPixelHeight)?,
        )?;
        let image = Owned::new(CGImageSourceCreateImageAtIndex(
            source.0,
            0,
            std::ptr::null_mut(),
        ))?;
        if CGImageGetWidth(image.0) != width as usize
            || CGImageGetHeight(image.0) != height as usize
        {
            return Err(io::ErrorKind::InvalidData.into());
        }
        let output = Owned::new(CFDataCreateMutable(std::ptr::null_mut(), 0))?;
        let destination = Owned::new(CGImageDestinationCreateWithData(
            output.0,
            nsstring("public.png"),
            1,
            std::ptr::null_mut(),
        ))?;
        CGImageDestinationAddImage(destination.0, image.0, std::ptr::null_mut());
        if !CGImageDestinationFinalize(destination.0) {
            return Err(io::ErrorKind::InvalidData.into());
        }
        let length =
            usize::try_from(CFDataGetLength(output.0)).map_err(|_| io::ErrorKind::InvalidData)?;
        if length == 0 || length > MAX_IMAGE {
            return Err(io::ErrorKind::InvalidData.into());
        }
        Ok(std::slice::from_raw_parts(CFDataGetBytePtr(output.0), length).to_vec())
    }
}
pub(super) fn prepare(image: &Image) -> io::Result<Vec<u8>> {
    png(&image.bytes)
}
