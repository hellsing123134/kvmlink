//! A hidden Win32 window on its own thread: tray icon, clipboard notifications, and the
//! "show yourself" message a second launch sends. It keeps working while the egui window is
//! hidden, and its class name is what `sys::show_existing` looks for.

use crate::{clip, net};
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering::Relaxed};
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Shell::{Shell_NotifyIconW, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

const ID_OPEN: usize = 201;
const ID_EXIT: usize = 202;
const WM_TRAY: u32 = WM_APP + 2;
const WM_EXIT: u32 = WM_APP + 3;
pub const WM_SHOWME: u32 = WM_APP + 4; // also sent by sys::show_existing

static HWND_: AtomicUsize = AtomicUsize::new(0);
/// The egui window, once it exists.
static APP: AtomicUsize = AtomicUsize::new(0);
static TASKBAR_CREATED: AtomicU32 = AtomicU32::new(0);

fn w(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

pub fn app_icon(size: i32) -> HICON {
    unsafe {
        let h = LoadImageW(GetModuleHandleW(std::ptr::null()), 1 as _, IMAGE_ICON, size, size, 0);
        if h.is_null() {
            LoadIconW(0 as _, IDI_APPLICATION)
        } else {
            h as HICON
        }
    }
}

fn tray_icon(h: HWND, op: u32) {
    unsafe {
        let mut nid: NOTIFYICONDATAW = std::mem::zeroed();
        nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        nid.hWnd = h;
        nid.uID = 1;
        nid.uFlags = 1 | 2 | 4; // NIF_MESSAGE | NIF_ICON | NIF_TIP
        nid.uCallbackMessage = WM_TRAY;
        nid.hIcon = app_icon(GetSystemMetrics(SM_CXSMICON));
        for (i, c) in "KVM Link".encode_utf16().enumerate() {
            nid.szTip[i] = c;
        }
        Shell_NotifyIconW(op, &nid);
    }
}

/// Remembers the egui window so the tray can show it.
pub fn set_app_window(h: isize) {
    APP.store(h as usize, Relaxed);
    unsafe {
        let big = app_icon(GetSystemMetrics(SM_CXICON));
        let small = app_icon(GetSystemMetrics(SM_CXSMICON));
        SendMessageW(h as HWND, WM_SETICON, ICON_BIG as WPARAM, big as LPARAM);
        SendMessageW(h as HWND, WM_SETICON, ICON_SMALL as WPARAM, small as LPARAM);
    }
}

pub fn show_app() {
    let h = APP.load(Relaxed) as HWND;
    if h.is_null() {
        return;
    }
    unsafe {
        ShowWindow(h, SW_SHOW);
        if IsIconic(h) != 0 {
            ShowWindow(h, SW_RESTORE);
        }
        SetForegroundWindow(h);
    }
    crate::ui::notify();
}

pub fn hide_app() {
    let h = APP.load(Relaxed) as HWND;
    if !h.is_null() {
        unsafe {
            ShowWindow(h, SW_HIDE);
        }
    }
}

/// Hides the window and moves it to the middle of the screen (used after an off-screen start).
pub fn hide_app_centered() {
    let h = APP.load(Relaxed) as HWND;
    if h.is_null() {
        return;
    }
    unsafe {
        ShowWindow(h, SW_HIDE);
        let mut r: RECT = std::mem::zeroed();
        GetWindowRect(h, &mut r);
        let mut work: RECT = std::mem::zeroed();
        SystemParametersInfoW(SPI_GETWORKAREA, 0, &mut work as *mut RECT as _, 0);
        let (w, ht) = (r.right - r.left, r.bottom - r.top);
        let x = work.left + ((work.right - work.left) - w) / 2;
        let y = work.top + ((work.bottom - work.top) - ht) / 2;
        SetWindowPos(h, std::ptr::null_mut(), x, y, 0, 0, SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE);
    }
}

pub fn app_visible() -> bool {
    let h = APP.load(Relaxed) as HWND;
    !h.is_null() && unsafe { IsWindowVisible(h) != 0 && IsIconic(h) == 0 }
}

/// Stops everything and ends the process.
pub fn exit() {
    let h = HWND_.load(Relaxed);
    if h != 0 {
        unsafe {
            PostMessageW(h as HWND, WM_EXIT, 0, 0);
        }
    } else {
        net::stop();
        std::process::exit(0);
    }
}

fn menu(h: HWND) {
    unsafe {
        let m = CreatePopupMenu();
        AppendMenuW(m, 0, ID_OPEN, w("Open KVM Link").as_ptr());
        AppendMenuW(m, MF_SEPARATOR, 0, std::ptr::null());
        AppendMenuW(m, 0, ID_EXIT, w("Exit").as_ptr());
        SetMenuDefaultItem(m, ID_OPEN as u32, 0);
        let mut p = std::mem::zeroed();
        GetCursorPos(&mut p);
        SetForegroundWindow(h);
        TrackPopupMenu(m, TPM_RIGHTBUTTON, p.x, p.y, 0, h, std::ptr::null());
        PostMessageW(h, WM_NULL, 0, 0);
        DestroyMenu(m);
    }
}

unsafe extern "system" fn wndproc(h: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_COMMAND => {
            match wp & 0xFFFF {
                ID_OPEN => show_app(),
                ID_EXIT => {
                    PostMessageW(h, WM_EXIT, 0, 0);
                }
                _ => {}
            }
            0
        }
        WM_CLIPBOARDUPDATE => {
            clip::on_local_change();
            0
        }
        clip::WM_CLIP_IN => {
            clip::apply_remote();
            0
        }
        WM_SHOWME => {
            show_app();
            0
        }
        WM_TRAY => {
            match (lp & 0xFFFF) as u32 {
                WM_LBUTTONUP | WM_LBUTTONDBLCLK => show_app(),
                WM_RBUTTONUP => menu(h),
                _ => {}
            }
            0
        }
        WM_EXIT => {
            net::stop();
            tray_icon(h, NIM_DELETE);
            std::process::exit(0);
        }
        m if m != 0 && m == TASKBAR_CREATED.load(Relaxed) => {
            tray_icon(h, NIM_ADD); // explorer restarted
            0
        }
        _ => DefWindowProcW(h, msg, wp, lp),
    }
}

/// Creates the hidden window and the tray icon on a thread of their own.
pub fn start() {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || unsafe {
        let hinst = GetModuleHandleW(std::ptr::null());
        let class = w("kvmlink_main");
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: 0,
            lpfnWndProc: Some(wndproc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: hinst,
            hIcon: app_icon(GetSystemMetrics(SM_CXICON)),
            hCursor: std::ptr::null_mut(),
            hbrBackground: std::ptr::null_mut(),
            lpszMenuName: std::ptr::null(),
            lpszClassName: class.as_ptr(),
            hIconSm: app_icon(GetSystemMetrics(SM_CXSMICON)),
        };
        RegisterClassExW(&wc);
        let h = CreateWindowExW(
            WS_EX_TOOLWINDOW,
            class.as_ptr(),
            w("KVM Link").as_ptr(),
            WS_POPUP,
            0,
            0,
            0,
            0,
            0 as _,
            0 as _,
            hinst,
            std::ptr::null(),
        );
        HWND_.store(h as usize, Relaxed);
        // An elevated/SYSTEM window ignores messages from the (non-elevated) shell unless allowed.
        let tbc = RegisterWindowMessageW(w("TaskbarCreated").as_ptr());
        TASKBAR_CREATED.store(tbc, Relaxed);
        for m in [WM_SHOWME, WM_TRAY, tbc] {
            ChangeWindowMessageFilterEx(h, m, MSGFLT_ALLOW, std::ptr::null_mut());
        }
        tray_icon(h, NIM_ADD);
        clip::init(h);
        let _ = tx.send(());
        crate::sys::message_loop();
    });
    let _ = rx.recv();
}
