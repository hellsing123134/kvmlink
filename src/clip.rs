//! Shared clipboard (text and images). Content travels over the same encrypted link as the
//! input, in chunks, so a big copy does not hold up the mouse. Every device forwards what it
//! receives to its other neighbours; a hash of the last content stops it from going round.

use crate::proto::CLIP_MAX;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::{GlobalFree, HWND};
use windows_sys::Win32::System::DataExchange::*;
use windows_sys::Win32::System::Memory::*;
use windows_sys::Win32::UI::WindowsAndMessaging::PostMessageW;

pub const WM_CLIP_IN: u32 = 0x8000 + 5; // WM_APP + 5

const MAX: usize = CLIP_MAX;
const TEXT: u8 = 1;
const DIB: u8 = 2;
const CF_UNICODETEXT: u32 = 13;
const CF_DIB: u32 = 8;

static ENABLED: AtomicBool = AtomicBool::new(false);
static MAIN: AtomicUsize = AtomicUsize::new(0);
static LAST: AtomicU64 = AtomicU64::new(0);
/// (sender, record) waiting for the window thread.
static INBOX: Mutex<Vec<(u64, Vec<u8>)>> = Mutex::new(Vec::new());
static APPLIED: Mutex<Option<Instant>> = Mutex::new(None);

fn fnv(b: &[u8]) -> u64 {
    let mut h = 0xcbf29ce484222325u64;
    for &x in b {
        h ^= x as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

// ---------- clipboard access (GUI thread only) ----------

unsafe fn open() -> bool {
    for _ in 0..10 {
        if OpenClipboard(MAIN.load(Relaxed) as HWND) != 0 {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    false
}

fn read_clipboard() -> Option<Vec<u8>> {
    unsafe {
        if !open() {
            return None;
        }
        let r = (|| {
            if IsClipboardFormatAvailable(CF_UNICODETEXT) != 0 {
                let h = GetClipboardData(CF_UNICODETEXT);
                if h.is_null() {
                    return None;
                }
                let p = GlobalLock(h) as *const u16;
                if p.is_null() {
                    return None;
                }
                let max = GlobalSize(h) / 2;
                let mut n = 0;
                while n < max && *p.add(n) != 0 {
                    n += 1;
                }
                let s = String::from_utf16_lossy(std::slice::from_raw_parts(p, n));
                GlobalUnlock(h);
                if s.is_empty() {
                    return None;
                }
                let mut v = vec![TEXT];
                v.extend_from_slice(s.as_bytes());
                return Some(v);
            }
            if IsClipboardFormatAvailable(CF_DIB) != 0 {
                let h = GetClipboardData(CF_DIB);
                if h.is_null() {
                    return None;
                }
                let size = GlobalSize(h);
                let p = GlobalLock(h) as *const u8;
                if p.is_null() {
                    return None;
                }
                let v = if size < MAX {
                    let mut v = Vec::with_capacity(size + 1);
                    v.push(DIB);
                    v.extend_from_slice(std::slice::from_raw_parts(p, size));
                    Some(v)
                } else {
                    None
                };
                GlobalUnlock(h);
                return v;
            }
            None
        })();
        CloseClipboard();
        r
    }
}

fn write_clipboard(rec: &[u8]) {
    let (fmt, bytes): (u32, Vec<u8>) = match rec.first() {
        Some(&TEXT) => {
            let s = String::from_utf8_lossy(&rec[1..]);
            let mut b = Vec::new();
            for u in s.encode_utf16().chain(Some(0)) {
                b.extend_from_slice(&u.to_le_bytes());
            }
            (CF_UNICODETEXT, b)
        }
        Some(&DIB) => (CF_DIB, rec[1..].to_vec()),
        _ => return,
    };
    unsafe {
        if !open() {
            return;
        }
        EmptyClipboard();
        let h = GlobalAlloc(GMEM_MOVEABLE, bytes.len());
        if !h.is_null() {
            let p = GlobalLock(h) as *mut u8;
            if !p.is_null() {
                std::ptr::copy_nonoverlapping(bytes.as_ptr(), p, bytes.len());
                GlobalUnlock(h);
                if SetClipboardData(fmt, h).is_null() {
                    GlobalFree(h);
                }
            } else {
                GlobalFree(h);
            }
        }
        CloseClipboard();
    }
}

/// Start watching the local clipboard (call on the thread that owns `hwnd`).
pub fn init(hwnd: HWND) {
    MAIN.store(hwnd as usize, Relaxed);
    unsafe {
        AddClipboardFormatListener(hwnd);
    }
}

pub fn set_enabled(on: bool) {
    ENABLED.store(on, Relaxed);
}

/// WM_CLIPBOARDUPDATE: something was copied on this computer.
pub fn on_local_change() {
    if !ENABLED.load(Relaxed) {
        return;
    }
    // Ignore the echo of what we just wrote ourselves.
    if APPLIED.lock().unwrap().is_some_and(|t| t.elapsed() < Duration::from_millis(800)) {
        return;
    }
    let Some(rec) = read_clipboard() else { return };
    let h = fnv(&rec);
    if LAST.swap(h, Relaxed) == h {
        return;
    }
    crate::net::broadcast_clip(Arc::new(rec), 0);
}

/// Called by a link thread: `from` sent new clipboard content.
pub fn incoming(from: u64, rec: Vec<u8>) {
    if !ENABLED.load(Relaxed) || rec.len() < 2 {
        return;
    }
    INBOX.lock().unwrap().push((from, rec));
    unsafe {
        PostMessageW(MAIN.load(Relaxed) as HWND, WM_CLIP_IN, 0, 0);
    }
}

/// WM_CLIP_IN: put the newest received content on the local clipboard and pass it on.
pub fn apply_remote() {
    let Some((from, rec)) = INBOX.lock().unwrap().drain(..).last() else { return };
    let h = fnv(&rec);
    if LAST.swap(h, Relaxed) == h {
        return; // already have it (it came round through another device)
    }
    *APPLIED.lock().unwrap() = Some(Instant::now());
    write_clipboard(&rec);
    crate::net::broadcast_clip(Arc::new(rec), from);
}
