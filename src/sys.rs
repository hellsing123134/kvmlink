//! Thin wrappers over the Win32 calls both sides need.

use windows_sys::Win32::Media::timeBeginPeriod;
use windows_sys::Win32::System::Threading::*;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

pub fn init_process() {
    unsafe {
        SetProcessDPIAware();
        timeBeginPeriod(1);
        SetPriorityClass(GetCurrentProcess(), HIGH_PRIORITY_CLASS);
    }
}

pub fn boost_thread() {
    unsafe {
        SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_TIME_CRITICAL);
    }
}

/// Size of the primary monitor.
pub fn screen() -> (i32, i32) {
    unsafe { (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN)) }
}

/// The whole desktop across all monitors: (x, y, width, height).
pub fn desk() -> (i32, i32, i32, i32) {
    unsafe {
        let r = (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN),
            GetSystemMetrics(SM_CYVIRTUALSCREEN),
        );
        if r.2 > 0 && r.3 > 0 {
            r
        } else {
            let (w, h) = screen();
            (0, 0, w, h)
        }
    }
}

pub fn set_cursor(x: i32, y: i32) {
    unsafe {
        SetCursorPos(x, y);
    }
}

pub fn cursor() -> (i32, i32) {
    unsafe {
        let mut p = std::mem::zeroed();
        if GetCursorPos(&mut p) == 0 {
            sync_desktop();
            GetCursorPos(&mut p);
        }
        (p.x, p.y)
    }
}

pub fn scan_from_vk(vk: u32) -> u16 {
    unsafe { MapVirtualKeyW(vk, MAPVK_VK_TO_VSC) as u16 }
}

pub fn message_loop() {
    unsafe {
        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, 0 as _, 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

thread_local! {
    static DESK: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Moves this thread to the desktop that currently receives input (UAC prompt, lock screen).
/// Only a SYSTEM process is allowed to; for everyone else it fails harmlessly.
fn sync_desktop() {
    use windows_sys::Win32::System::StationsAndDesktops::{CloseDesktop, OpenInputDesktop, SetThreadDesktop};
    unsafe {
        let d = OpenInputDesktop(0, 0, 0x1000_0000 /*GENERIC_ALL*/);
        if d.is_null() {
            return;
        }
        if SetThreadDesktop(d) != 0 {
            let old = DESK.with(|c| c.replace(d as usize));
            if old != 0 {
                CloseDesktop(old as _);
            }
        } else {
            CloseDesktop(d);
        }
    }
}

fn send_input(i: INPUT) {
    unsafe {
        if SendInput(1, &i, std::mem::size_of::<INPUT>() as i32) == 0 {
            sync_desktop();
            SendInput(1, &i, std::mem::size_of::<INPUT>() as i32);
        }
    }
}

fn mouse(dx: i32, dy: i32, data: u32, flags: u32) {
    send_input(INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT { dx, dy, mouseData: data, dwFlags: flags, time: 0, dwExtraInfo: 0 },
        },
    });
}

/// Absolute move to a pixel of the desktop (any monitor).
pub fn move_abs(x: i32, y: i32) {
    let (vx, vy, w, h) = desk();
    let ax = (((x - vx) as i64 * 65536 + 32768) / w as i64) as i32;
    let ay = (((y - vy) as i64 * 65536 + 32768) / h as i64) as i32;
    mouse(ax, ay, 0, MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK);
}

/// idx: 0 L, 1 R, 2 M, 3 X1, 4 X2
pub fn button(idx: i32, down: bool) {
    let (d, u, data) = match idx {
        0 => (MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, 0),
        1 => (MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, 0),
        2 => (MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, 0),
        3 => (MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, 1),
        _ => (MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, 2),
    };
    mouse(0, 0, data, if down { d } else { u });
}

pub fn wheel(delta: i32, horizontal: bool) {
    mouse(0, 0, delta as u32, if horizontal { MOUSEEVENTF_HWHEEL } else { MOUSEEVENTF_WHEEL });
}

pub fn key(scan: u16, up: bool, extended: bool) {
    let mut f = KEYEVENTF_SCANCODE;
    if up {
        f |= KEYEVENTF_KEYUP;
    }
    if extended {
        f |= KEYEVENTF_EXTENDEDKEY;
    }
    send_input(INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT { wVk: 0, wScan: scan, dwFlags: f, time: 0, dwExtraInfo: 0 },
        },
    });
}

/// Lifts Ctrl/Alt on the local machine after the switch hotkey, so they do not stay stuck.
pub fn release_local_modifiers() {
    for vk in [VK_LCONTROL, VK_RCONTROL, VK_LMENU, VK_RMENU] {
        send_input(INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT { wVk: vk, wScan: 0, dwFlags: KEYEVENTF_KEYUP, time: 0, dwExtraInfo: 0 },
            },
        });
    }
}

/// Asks the running copy (maybe elevated) to show its window.
pub fn show_existing() {
    let class: Vec<u16> = "kvmlink_main".encode_utf16().chain(Some(0)).collect();
    unsafe {
        let h = FindWindowW(class.as_ptr(), std::ptr::null());
        if !h.is_null() {
            PostMessageW(h, WM_APP + 4, 0, 0);
        }
    }
}

/// True if another copy (possibly elevated) already holds the single-instance mutex.
pub fn already_running() -> bool {
    use windows_sys::Win32::Foundation::GetLastError;
    use windows_sys::Win32::System::Threading::CreateMutexW;
    // KVMLINK_INSTANCE lets a second, independent copy run (testing).
    let suffix = std::env::var("KVMLINK_INSTANCE").unwrap_or_default();
    let name: Vec<u16> = format!("Local\\kvmlink-single{suffix}").encode_utf16().chain(Some(0)).collect();
    unsafe {
        let h = CreateMutexW(std::ptr::null(), 0, name.as_ptr());
        let err = GetLastError();
        // 183 = already exists, 5 = exists but owned by an elevated copy
        h.is_null() || err == 183 || err == 5
    }
}
