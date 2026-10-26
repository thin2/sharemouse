use super::*;
use lan_mouse_data::clipboard_image::{MAX_IMAGE, MAX_PIXELS};
use std::{ffi::c_void, ptr};
#[path = "windows_image.rs"]
mod images;
type Handle = *mut c_void;
#[repr(C)]
struct WindowMessage {
    window: Handle,
    message: u32,
    wparam: usize,
    lparam: isize,
    time: u32,
    point: [i32; 2],
    private: u32,
}
#[link(name = "user32")]
unsafe extern "system" {
    fn OpenClipboard(window: Handle) -> i32;
    fn CloseClipboard() -> i32;
    fn EmptyClipboard() -> i32;
    fn GetClipboardSequenceNumber() -> u32;
    fn IsClipboardFormatAvailable(format: u32) -> i32;
    fn GetClipboardData(format: u32) -> Handle;
    fn SetClipboardData(format: u32, data: Handle) -> Handle;
    fn RegisterClipboardFormatW(name: *const u16) -> u32;
    fn CreateWindowExW(
        ex: u32,
        class: *const u16,
        name: *const u16,
        style: u32,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        parent: Handle,
        menu: Handle,
        instance: Handle,
        param: Handle,
    ) -> Handle;
    fn DestroyWindow(window: Handle) -> i32;
    fn PeekMessageW(
        message: *mut WindowMessage,
        window: Handle,
        min: u32,
        max: u32,
        remove: u32,
    ) -> i32;
    fn DispatchMessageW(message: *const WindowMessage) -> isize;
}
#[link(name = "shell32")]
unsafe extern "system" {
    fn DragQueryFileW(drop: Handle, index: u32, name: *mut u16, size: u32) -> u32;
}
#[link(name = "kernel32")]
unsafe extern "system" {
    fn GlobalAlloc(flags: u32, bytes: usize) -> Handle;
    fn GlobalLock(handle: Handle) -> *mut c_void;
    fn GlobalUnlock(handle: Handle) -> i32;
    fn GlobalFree(handle: Handle) -> Handle;
    fn GlobalSize(handle: Handle) -> usize;
}
struct Clipboard;
struct Owner(Handle);
impl Drop for Owner {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                DestroyWindow(self.0);
            }
        }
    }
}
thread_local! {
    // A hidden built-in window owned by this thread, never another process.
    static OWNER: Owner = Owner(unsafe {
        let class: Vec<u16> = "STATIC\0".encode_utf16().collect();
        CreateWindowExW(0, class.as_ptr(), ptr::null(), 0, 0, 0, 0, 0,
            ptr::null_mut(), ptr::null_mut(), ptr::null_mut(), ptr::null_mut())
    });
}
impl Clipboard {
    fn open(writing: bool) -> io::Result<Self> {
        // A non-null owner is required by SetClipboardData after EmptyClipboard.
        let owner = if writing {
            OWNER.with(|owner| owner.0)
        } else {
            ptr::null_mut()
        };
        if writing && owner.is_null() {
            return Err(io::Error::last_os_error());
        }
        // This thread runs Tokio rather than a Win32 message loop. Dispatch
        // owner messages (including WM_DESTROYCLIPBOARD) on every poll so
        // other applications do not stall waiting for our hidden window.
        if writing {
            pump_owner();
        }
        if unsafe { OpenClipboard(owner) } == 0 {
            Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "clipboard is busy",
            ))
        } else {
            Ok(Self)
        }
    }
}
fn pump_owner() {
    // Worker reads use OpenClipboard(NULL) and own no window.
    OWNER.with(|owner| {
        if owner.0.is_null() {
            return;
        }
        let mut message: WindowMessage = unsafe { std::mem::zeroed() };
        for _ in 0..64 {
            if unsafe { PeekMessageW(&mut message, owner.0, 0, 0, 1) } == 0 {
                break;
            }
            unsafe {
                DispatchMessageW(&message);
            }
        }
    });
}
pub(super) fn counter() -> io::Result<u64> {
    pump_owner();
    Ok(unsafe { GetClipboardSequenceNumber() } as u64)
}
fn registered(name: &str) -> io::Result<u32> {
    let units: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
    let format = unsafe { RegisterClipboardFormatW(units.as_ptr()) };
    if format == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(format)
    }
}
unsafe fn data(format: u32, limit: usize) -> io::Result<Vec<u8>> {
    let handle = unsafe { GetClipboardData(format) };
    if handle.is_null() {
        return Err(io::Error::last_os_error());
    }
    let size = unsafe { GlobalSize(handle) };
    if size == 0 || size > limit {
        return Err(io::ErrorKind::InvalidData.into());
    }
    let pointer = unsafe { GlobalLock(handle) } as *const u8;
    if pointer.is_null() {
        return Err(io::Error::last_os_error());
    }
    let bytes = unsafe { std::slice::from_raw_parts(pointer, size) }.to_vec();
    unsafe {
        GlobalUnlock(handle);
    }
    Ok(bytes)
}
unsafe fn image_snapshot() -> io::Result<(bool, Option<Image>)> {
    if unsafe { IsClipboardFormatAvailable(15) } != 0 {
        let drop = unsafe { GetClipboardData(15) };
        if drop.is_null() {
            return Err(io::Error::last_os_error());
        }
        if unsafe { DragQueryFileW(drop, u32::MAX, ptr::null_mut(), 0) } != 1 {
            return Ok((true, None));
        }
        let size = unsafe { DragQueryFileW(drop, 0, ptr::null_mut(), 0) };
        if size == 0 || size > 32767 {
            return Ok((true, None));
        }
        let mut path = vec![0_u16; size as usize + 1];
        if unsafe { DragQueryFileW(drop, 0, path.as_mut_ptr(), size + 1) } != size {
            return Err(io::ErrorKind::InvalidData.into());
        }
        use std::os::windows::ffi::OsStringExt;
        let path = std::ffi::OsString::from_wide(&path[..size as usize]);
        return Ok((true, Image::from_file(std::path::Path::new(&path)).ok()));
    }
    for name in ["PNG", "GIF", "JFIF"] {
        let format = registered(name)?;
        if unsafe { IsClipboardFormatAvailable(format) } != 0 {
            let image = unsafe { data(format, MAX_IMAGE) }
                .and_then(|bytes| Image::new(bytes, None))
                .ok();
            return Ok((true, image));
        }
    }
    for format in [17, 8] {
        if unsafe { IsClipboardFormatAvailable(format) } != 0 {
            let image = unsafe { data(format, MAX_PIXELS as usize * 4 + 4096) }
                .and_then(|bytes| super::raster::from_dib(&bytes))
                .and_then(|bytes| Image::new(bytes, None))
                .ok();
            return Ok((true, image));
        }
    }
    Ok((unsafe { IsClipboardFormatAvailable(2) } != 0, None))
}
impl Drop for Clipboard {
    fn drop(&mut self) {
        unsafe {
            CloseClipboard();
        }
    }
}
unsafe fn snapshot(previous: Option<u64>) -> io::Result<RawSnapshot> {
    let raw = unsafe { GetClipboardSequenceNumber() } as u64;
    if previous == Some(raw) {
        return Ok(RawSnapshot {
            raw,
            text: None,
            image: None,
        });
    }
    let mut text = None;
    let (unsupported, image) = unsafe { image_snapshot()? };
    if !unsupported && unsafe { IsClipboardFormatAvailable(13) } != 0 {
        let handle = unsafe { GetClipboardData(13) };
        if handle.is_null() {
            return Err(io::Error::last_os_error());
        }
        let size = unsafe { GlobalSize(handle) };
        if size <= (lan_mouse_data::clipboard_sync::MAX_TEXT + 1) * 2 {
            let pointer = unsafe { GlobalLock(handle) } as *const u16;
            if pointer.is_null() {
                return Err(io::Error::last_os_error());
            }
            let units = unsafe { std::slice::from_raw_parts(pointer, size / 2) };
            if let Some(end) = units.iter().position(|u| *u == 0) {
                text = String::from_utf16(&units[..end])
                    .ok()
                    .filter(|t| t.len() <= lan_mouse_data::clipboard_sync::MAX_TEXT);
            }
            unsafe {
                GlobalUnlock(handle);
            }
        }
    }
    if unsafe { GetClipboardSequenceNumber() } as u64 != raw {
        return Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "clipboard changed while rendering",
        ));
    }
    Ok(RawSnapshot { raw, text, image })
}
pub(super) fn read(previous: Option<u64>) -> io::Result<RawSnapshot> {
    let _clipboard = Clipboard::open(false)?;
    unsafe { snapshot(previous) }
}
pub(super) fn write(expected: u64, text: &str) -> io::Result<Option<RawSnapshot>> {
    // CF_UNICODETEXT cannot preserve embedded NULs. Reject before clearing.
    if text.contains('\0') {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "embedded NUL in clipboard text",
        ));
    }
    let _clipboard = Clipboard::open(true)?;
    unsafe {
        if GetClipboardSequenceNumber() as u64 != expected {
            return Ok(None);
        }
        let units: Vec<_> = text.encode_utf16().chain(Some(0)).collect();
        let memory = GlobalAlloc(2, units.len() * 2);
        if memory.is_null() {
            return Err(io::Error::last_os_error());
        }
        let target = GlobalLock(memory) as *mut u16;
        if target.is_null() {
            GlobalFree(memory);
            return Err(io::Error::last_os_error());
        }
        ptr::copy_nonoverlapping(units.as_ptr(), target, units.len());
        GlobalUnlock(memory);
        if EmptyClipboard() == 0 || SetClipboardData(13, memory).is_null() {
            GlobalFree(memory);
            return Err(io::Error::last_os_error());
        }
        // Record while still owning the clipboard. Reopening after releasing
        // it could accidentally suppress a genuine intervening local copy.
        Ok(Some(RawSnapshot {
            raw: GetClipboardSequenceNumber() as u64,
            text: Some(text.to_owned()),
            image: None,
        }))
    }
}

pub(super) fn prepare_image(image: &Image) -> io::Result<Vec<u8>> {
    images::preview(image)
}
struct Memory(Handle);
impl Memory {
    fn new(bytes: &[u8]) -> io::Result<Self> {
        unsafe {
            let memory = Self(GlobalAlloc(2, bytes.len()));
            if memory.0.is_null() {
                return Err(io::Error::last_os_error());
            }
            let target = GlobalLock(memory.0) as *mut u8;
            if target.is_null() {
                return Err(io::Error::last_os_error());
            }
            ptr::copy_nonoverlapping(bytes.as_ptr(), target, bytes.len());
            GlobalUnlock(memory.0);
            Ok(memory)
        }
    }
    fn publish(mut self, format: u32) -> io::Result<()> {
        if unsafe { SetClipboardData(format, self.0) }.is_null() {
            return Err(io::Error::last_os_error());
        }
        self.0 = ptr::null_mut(); // transferred to the system
        Ok(())
    }
}
impl Drop for Memory {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                GlobalFree(self.0);
            }
        }
    }
}
pub(super) fn write_image(
    expected: u64,
    prepared: &PreparedImage,
) -> io::Result<Option<RawSnapshot>> {
    use std::os::windows::ffi::OsStrExt;
    let path: Vec<u16> = prepared
        .path
        .as_os_str()
        .encode_wide()
        .chain([0, 0])
        .collect();
    if path.len() > 32769 {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    // DROPFILES, UTF-16 and a double terminator for exactly one file.
    let mut drop = vec![0_u8; 20];
    drop[..4].copy_from_slice(&20_u32.to_le_bytes());
    drop[16..20].copy_from_slice(&1_u32.to_le_bytes());
    for unit in path {
        drop.extend_from_slice(&unit.to_le_bytes());
    }
    let png_format = registered("PNG")?;
    let png = Memory::new(&prepared.preview)?;
    let dib = Memory::new(&prepared.dib)?;
    let file = Memory::new(&drop)?;
    let _clipboard = Clipboard::open(true)?;
    if unsafe { GetClipboardSequenceNumber() } as u64 != expected {
        return Ok(None);
    }
    if unsafe { EmptyClipboard() } == 0 {
        return Err(io::Error::last_os_error());
    }
    png.publish(png_format)?;
    dib.publish(17)?;
    file.publish(15)?;
    Ok(Some(RawSnapshot {
        raw: unsafe { GetClipboardSequenceNumber() } as u64,
        text: None,
        image: Some(prepared.image.clone()),
    }))
}
