//! A native notification icon with its own Win32 message loop.
use crate::{
    preferences::{UiPreferences, tr},
    window::Window,
};
use adw::prelude::*;
use gtk::glib;
use std::{
    cell::RefCell,
    sync::{OnceLock, mpsc},
    time::Duration,
};
use windows::{
    Win32::{
        Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM},
        System::LibraryLoader::GetModuleHandleW,
        UI::{
            Shell::{
                NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW,
                Shell_NotifyIconW,
            },
            WindowsAndMessaging::*,
        },
    },
    core::{PCWSTR, w},
};

const CALLBACK: u32 = WM_APP + 1;
static TASKBAR_CREATED: OnceLock<u32> = OnceLock::new();
thread_local! {
    static EVENTS: RefCell<Option<mpsc::Sender<u32>>> = const { RefCell::new(None) };
}

pub fn setup(app: &adw::Application, window: &Window) -> Result<(), String> {
    let (events_tx, events_rx) = mpsc::channel();
    let (ready_tx, ready_rx) = mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name("notification-icon".into())
        .spawn(move || unsafe {
            EVENTS.with(|slot| slot.replace(Some(events_tx)));
            let result = create_icon_window();
            let hwnd = match result {
                Ok(hwnd) => {
                    let _ = ready_tx.send(Ok(hwnd.0 as usize));
                    hwnd
                }
                Err(e) => {
                    let _ = ready_tx.send(Err(e.to_string()));
                    return;
                }
            };
            let mut message = MSG::default();
            while GetMessageW(&mut message, None, 0, 0).0 > 0 {
                let _ = TranslateMessage(&message);
                DispatchMessageW(&message);
            }
            let _ = Shell_NotifyIconW(NIM_DELETE, &icon_data(hwnd));
        })
        .map_err(|e| e.to_string())?;
    let hwnd = ready_rx
        .recv_timeout(Duration::from_secs(5))
        .map_err(|e| e.to_string())??;
    let hold = app.hold();
    app.connect_shutdown(move |_| unsafe {
        let _ = &hold;
        let _ = PostMessageW(Some(HWND(hwnd as *mut _)), WM_CLOSE, WPARAM(0), LPARAM(0));
    });
    let app = app.downgrade();
    let window = window.downgrade();
    glib::timeout_add_local(Duration::from_millis(100), move || {
        let Some(app) = app.upgrade() else {
            return glib::ControlFlow::Break;
        };
        while let Ok(action) = events_rx.try_recv() {
            match action {
                1 => {
                    if let Some(window) = window.upgrade() {
                        window.present();
                    }
                }
                2 => app.activate_action("stop", None),
                3 => app.activate_action("quit", None),
                _ => {}
            }
        }
        glib::ControlFlow::Continue
    });
    Ok(())
}

unsafe fn create_icon_window() -> windows::core::Result<HWND> {
    let instance = GetModuleHandleW(None)?;
    let class = WNDCLASSW {
        hInstance: instance.into(),
        lpszClassName: w!("SharemouseNotificationWindow"),
        lpfnWndProc: Some(window_proc),
        ..Default::default()
    };
    if RegisterClassW(&class) == 0 {
        return Err(windows::core::Error::from_win32());
    }
    let hwnd = CreateWindowExW(
        WINDOW_EX_STYLE::default(),
        class.lpszClassName,
        w!("Lan Mouse"),
        WINDOW_STYLE::default(),
        0,
        0,
        0,
        0,
        None,
        None,
        Some(instance.into()),
        None,
    )?;
    TASKBAR_CREATED.get_or_init(|| RegisterWindowMessageW(w!("TaskbarCreated")));
    if !Shell_NotifyIconW(NIM_ADD, &icon_data(hwnd)).as_bool() {
        let _ = DestroyWindow(hwnd);
        return Err(windows::core::Error::from_win32());
    }
    Ok(hwnd)
}

unsafe fn icon_data(hwnd: HWND) -> NOTIFYICONDATAW {
    let mut data = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: 1,
        uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP,
        uCallbackMessage: CALLBACK,
        hIcon: LoadIconW(None, IDI_APPLICATION).unwrap_or_default(),
        ..Default::default()
    };
    for (dest, value) in data.szTip.iter_mut().zip("Lan Mouse".encode_utf16()) {
        *dest = value;
    }
    data
}

unsafe extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if TASKBAR_CREATED.get().copied() == Some(message) {
        let _ = Shell_NotifyIconW(NIM_ADD, &icon_data(hwnd));
        return LRESULT(0);
    }
    match message {
        CALLBACK => {
            let action = match lparam.0 as u32 {
                WM_LBUTTONUP | WM_LBUTTONDBLCLK => 1,
                WM_RBUTTONUP | WM_CONTEXTMENU => popup(hwnd),
                _ => 0,
            };
            EVENTS.with(|slot| {
                if let Some(tx) = slot.borrow().as_ref() {
                    let _ = tx.send(action);
                }
            });
            LRESULT(0)
        }
        WM_CLOSE => {
            let _ = Shell_NotifyIconW(NIM_DELETE, &icon_data(hwnd));
            let _ = DestroyWindow(hwnd);
            LRESULT(0)
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, message, wparam, lparam),
    }
}

unsafe fn popup(hwnd: HWND) -> u32 {
    let Ok(menu) = CreatePopupMenu() else {
        return 0;
    };
    let language = UiPreferences::load().language;
    for (id, key) in [(1, "app.open"), (2, "app.stop"), (3, "app.quit")] {
        let label: Vec<u16> = tr(language, key).encode_utf16().chain(Some(0)).collect();
        let _ = AppendMenuW(menu, MF_STRING, id, PCWSTR(label.as_ptr()));
    }
    let mut point = POINT::default();
    let _ = GetCursorPos(&mut point);
    let _ = SetForegroundWindow(hwnd);
    let selected = TrackPopupMenu(
        menu,
        TPM_RETURNCMD | TPM_RIGHTBUTTON,
        point.x,
        point.y,
        None,
        hwnd,
        None,
    )
    .0 as u32;
    let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
    let _ = DestroyMenu(menu);
    selected
}
