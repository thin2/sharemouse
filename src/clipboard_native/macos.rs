use super::*;
use std::ffi::{CStr, c_char, c_void};

type Id = *mut c_void;
type Sel = *mut c_void;
// NSPasteboard returns cached process-wide objects. Concurrent type-cache
// updates on different worker threads can use already released Cocoa objects.
static PASTEBOARD: std::sync::Mutex<()> = std::sync::Mutex::new(());
fn access() -> io::Result<std::sync::MutexGuard<'static, ()>> {
    PASTEBOARD
        .try_lock()
        .map_err(|_| io::ErrorKind::WouldBlock.into())
}
#[path = "macos_image.rs"]
mod images;
use lan_mouse_data::clipboard_image::{Image, MAX_IMAGE};

#[link(name = "AppKit", kind = "framework")]
unsafe extern "C" {}
#[link(name = "objc")]
unsafe extern "C" {
    fn objc_getClass(name: *const c_char) -> Id;
    fn sel_registerName(name: *const c_char) -> Sel;
    fn objc_msgSend();
}

// Every cast below names the exact Objective-C method ABI. All objects are
// used within one autorelease pool on the same thread.
unsafe fn selector(name: &CStr) -> Sel {
    unsafe { sel_registerName(name.as_ptr()) }
}
unsafe fn id0(object: Id, name: &CStr) -> Id {
    let call: unsafe extern "C" fn(Id, Sel) -> Id =
        unsafe { std::mem::transmute(objc_msgSend as *const ()) };
    unsafe { call(object, selector(name)) }
}
unsafe fn id1(object: Id, name: &CStr, value: Id) -> Id {
    let call: unsafe extern "C" fn(Id, Sel, Id) -> Id =
        unsafe { std::mem::transmute(objc_msgSend as *const ()) };
    unsafe { call(object, selector(name), value) }
}
unsafe fn number(object: Id, name: &CStr) -> usize {
    let call: unsafe extern "C" fn(Id, Sel) -> usize =
        unsafe { std::mem::transmute(objc_msgSend as *const ()) };
    unsafe { call(object, selector(name)) }
}
unsafe fn nsstring(text: &str) -> Id {
    let class = unsafe { objc_getClass(c"NSString".as_ptr()) };
    let allocated = unsafe { id0(class, c"alloc") };
    let call: unsafe extern "C" fn(Id, Sel, *const u8, usize, usize) -> Id =
        unsafe { std::mem::transmute(objc_msgSend as *const ()) };
    let string = unsafe {
        call(
            allocated,
            selector(c"initWithBytes:length:encoding:"),
            text.as_ptr(),
            text.len(),
            4,
        )
    };
    unsafe { id0(string, c"autorelease") }
}
struct Pool(Id);
impl Pool {
    fn new() -> Self {
        Self(unsafe { id0(objc_getClass(c"NSAutoreleasePool".as_ptr()), c"new") })
    }
}
impl Drop for Pool {
    fn drop(&mut self) {
        let call: unsafe extern "C" fn(Id, Sel) =
            unsafe { std::mem::transmute(objc_msgSend as *const ()) };
        unsafe {
            call(self.0, selector(c"drain"));
        }
    }
}
unsafe fn board() -> Id {
    let class = unsafe { objc_getClass(c"NSPasteboard".as_ptr()) };
    // Test processes use separate named NSPasteboards, never the user's board.
    match std::env::var("LAN_MOUSE_TEST_PASTEBOARD") {
        Ok(name) if !name.is_empty() => unsafe {
            id1(class, c"pasteboardWithName:", nsstring(&name))
        },
        _ => unsafe { id0(class, c"generalPasteboard") },
    }
}
unsafe fn snapshot(board: Id, previous: Option<u64>) -> io::Result<RawSnapshot> {
    if board.is_null() {
        return Err(io::Error::other("NSPasteboard unavailable"));
    }
    let before = unsafe { number(board, c"changeCount") } as u64;
    if previous == Some(before) {
        return Ok(RawSnapshot {
            raw: before,
            text: None,
            image: None,
        });
    }
    let types = unsafe { id0(board, c"types") };
    let contains: unsafe extern "C" fn(Id, Sel, Id) -> bool =
        unsafe { std::mem::transmute(objc_msgSend as *const ()) };
    let has = |kind| unsafe { contains(types, selector(c"containsObject:"), nsstring(kind)) };
    let data_bytes = |kind| -> io::Result<Vec<u8>> {
        let data = unsafe { id1(board, c"dataForType:", nsstring(kind)) };
        if data.is_null() {
            return Err(io::ErrorKind::InvalidData.into());
        }
        let size = unsafe { number(data, c"length") };
        if size == 0 || size > MAX_IMAGE {
            return Err(io::ErrorKind::InvalidData.into());
        }
        let pointer = unsafe { id0(data, c"bytes") } as *const u8;
        if pointer.is_null() {
            return Err(io::ErrorKind::InvalidData.into());
        }
        Ok(unsafe { std::slice::from_raw_parts(pointer, size) }.to_vec())
    };
    let mut image = None;
    let mut image_kind = false;
    if has("public.file-url") {
        image_kind = true;
        let urls = unsafe {
            let classes = id1(
                objc_getClass(c"NSArray".as_ptr()),
                c"arrayWithObject:",
                objc_getClass(c"NSURL".as_ptr()),
            );
            let call: unsafe extern "C" fn(Id, Sel, Id, Id) -> Id =
                std::mem::transmute(objc_msgSend as *const ());
            call(
                board,
                selector(c"readObjectsForClasses:options:"),
                classes,
                std::ptr::null_mut(),
            )
        };
        if !urls.is_null() && unsafe { number(urls, c"count") } == 1 {
            let url = unsafe { id0(urls, c"firstObject") };
            let is_file: unsafe extern "C" fn(Id, Sel) -> bool =
                unsafe { std::mem::transmute(objc_msgSend as *const ()) };
            if unsafe { is_file(url, selector(c"isFileURL")) } {
                let path = unsafe { id0(url, c"path") };
                let pointer = unsafe { id0(path, c"fileSystemRepresentation") } as *const c_char;
                if !pointer.is_null() {
                    use std::os::unix::ffi::OsStrExt;
                    let path = std::path::Path::new(std::ffi::OsStr::from_bytes(
                        unsafe { CStr::from_ptr(pointer) }.to_bytes(),
                    ));
                    image = Image::from_file(path).ok();
                }
            }
        }
    } else {
        for kind in [
            "com.compuserve.gif",
            "public.png",
            "public.jpeg",
            "public.tiff",
        ] {
            if has(kind) {
                image_kind = true;
                if let Ok(bytes) = data_bytes(kind) {
                    image = if kind == "public.tiff" {
                        images::png(&bytes).and_then(|b| Image::new(b, None)).ok()
                    } else {
                        Image::new(bytes, None).ok()
                    };
                }
                break;
            }
        }
    }
    if image_kind {
        let items = unsafe { id0(board, c"pasteboardItems") };
        if unsafe { number(items, c"count") } != 1 {
            image = None;
        }
    }
    let mut text = None;
    if !image_kind {
        let string = unsafe { id1(board, c"stringForType:", nsstring("public.utf8-plain-text")) };
        if !string.is_null()
            && unsafe { number(string, c"length") } <= lan_mouse_data::clipboard_sync::MAX_TEXT
        {
            let call: unsafe extern "C" fn(Id, Sel, usize) -> Id =
                unsafe { std::mem::transmute(objc_msgSend as *const ()) };
            let data = unsafe { call(string, selector(c"dataUsingEncoding:"), 4) };
            if !data.is_null() {
                let length = unsafe { number(data, c"length") };
                if length <= lan_mouse_data::clipboard_sync::MAX_TEXT {
                    let bytes = unsafe { id0(data, c"bytes") } as *const u8;
                    if length == 0 {
                        text = Some(String::new());
                    } else if !bytes.is_null() {
                        text = String::from_utf8(
                            unsafe { std::slice::from_raw_parts(bytes, length) }.to_vec(),
                        )
                        .ok();
                    }
                }
            }
        }
    }
    let after = unsafe { number(board, c"changeCount") } as u64;
    if before != after {
        return Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "clipboard changed while reading",
        ));
    }
    Ok(RawSnapshot {
        raw: after,
        text,
        image,
    })
}
pub(super) fn counter() -> io::Result<u64> {
    let _access = access()?;
    let _pool = Pool::new();
    unsafe {
        let board = board();
        if board.is_null() {
            return Err(io::Error::other("NSPasteboard unavailable"));
        }
        Ok(number(board, c"changeCount") as u64)
    }
}
pub(super) fn prepare_image(image: &Image) -> io::Result<Vec<u8>> {
    images::prepare(image)
}
pub(super) fn write_image(
    expected: u64,
    prepared: &PreparedImage,
) -> io::Result<Option<RawSnapshot>> {
    let _access = access()?;
    let _pool = Pool::new();
    unsafe {
        let board = board();
        if board.is_null() {
            return Err(io::Error::other("NSPasteboard unavailable"));
        }
        let item = id0(objc_getClass(c"NSPasteboardItem".as_ptr()), c"new");
        if item.is_null() {
            return Err(io::Error::other("NSPasteboardItem unavailable"));
        }
        let item = id0(item, c"autorelease");
        let data = |bytes: &[u8]| {
            let call: unsafe extern "C" fn(Id, Sel, *const u8, usize) -> Id =
                std::mem::transmute(objc_msgSend as *const ());
            call(
                objc_getClass(c"NSData".as_ptr()),
                selector(c"dataWithBytes:length:"),
                bytes.as_ptr(),
                bytes.len(),
            )
        };
        let set: unsafe extern "C" fn(Id, Sel, Id, Id) -> bool =
            std::mem::transmute(objc_msgSend as *const ());
        let file_url = id1(
            objc_getClass(c"NSURL".as_ptr()),
            c"fileURLWithPath:",
            nsstring(prepared.path.to_str().ok_or(io::ErrorKind::InvalidData)?),
        );
        if !set(
            item,
            selector(c"setData:forType:"),
            data(&prepared.preview),
            nsstring("public.png"),
        ) || !set(
            item,
            selector(c"setString:forType:"),
            id0(file_url, c"absoluteString"),
            nsstring("public.file-url"),
        ) || prepared.image.format != lan_mouse_data::clipboard_image::Format::Png
            && !set(
                item,
                selector(c"setData:forType:"),
                data(&prepared.image.bytes),
                nsstring(prepared.image.format.uti()),
            )
        {
            return Err(io::Error::other("could not prepare image clipboard"));
        }
        if number(board, c"changeCount") as u64 != expected {
            return Ok(None);
        }
        number(board, c"clearContents");
        let objects = id1(
            objc_getClass(c"NSArray".as_ptr()),
            c"arrayWithObject:",
            item,
        );
        let write: unsafe extern "C" fn(Id, Sel, Id) -> bool =
            std::mem::transmute(objc_msgSend as *const ());
        if !write(board, selector(c"writeObjects:"), objects) {
            return Err(io::Error::other("image clipboard write failed"));
        }
        let raw = number(board, c"changeCount") as u64;
        let actual_url = id1(board, c"stringForType:", nsstring("public.file-url"));
        let equal: unsafe extern "C" fn(Id, Sel, Id) -> bool =
            std::mem::transmute(objc_msgSend as *const ());
        if !equal(
            actual_url,
            selector(c"isEqualToString:"),
            id0(file_url, c"absoluteString"),
        ) || number(board, c"changeCount") as u64 != raw
        {
            return Ok(None);
        }
        Ok(Some(RawSnapshot {
            raw,
            text: None,
            image: Some(prepared.image.clone()),
        }))
    }
}
pub(super) fn read(previous: Option<u64>) -> io::Result<RawSnapshot> {
    let _access = access()?;
    let _pool = Pool::new();
    unsafe { snapshot(board(), previous) }
}
pub(super) fn write(expected: u64, text: &str) -> io::Result<Option<RawSnapshot>> {
    let _access = access()?;
    let _pool = Pool::new();
    unsafe {
        let board = board();
        if board.is_null() {
            return Err(io::Error::other("NSPasteboard unavailable"));
        }
        if number(board, c"changeCount") as u64 != expected {
            return Ok(None);
        }
        let string = nsstring(text);
        let kind = nsstring("public.utf8-plain-text");
        if number(board, c"changeCount") as u64 != expected {
            return Ok(None);
        }
        number(board, c"clearContents");
        let call: unsafe extern "C" fn(Id, Sel, Id, Id) -> bool =
            std::mem::transmute(objc_msgSend as *const ());
        if !call(board, selector(c"setString:forType:"), string, kind) {
            return Err(io::Error::other("NSPasteboard write failed"));
        }
        let raw = number(board, c"changeCount") as u64;
        let actual = id1(board, c"stringForType:", kind);
        let equal: unsafe extern "C" fn(Id, Sel, Id) -> bool =
            std::mem::transmute(objc_msgSend as *const ());
        if !equal(actual, selector(c"isEqualToString:"), string)
            || number(board, c"changeCount") as u64 != raw
        {
            return Ok(None);
        }
        Ok(Some(RawSnapshot {
            raw,
            text: Some(text.to_owned()),
            image: None,
        }))
    }
}
