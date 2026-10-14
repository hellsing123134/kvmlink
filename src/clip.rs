//! Clipboard sync (text and images) over a second TCP connection on port + 1,
//! so a big copy can never delay mouse/keyboard input. Same key, same encryption.

use crate::proto::{key, pair, Opener, Sealer, SALT};
use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering::Relaxed};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::{GlobalFree, HWND};
use windows_sys::Win32::System::DataExchange::*;
use windows_sys::Win32::System::Memory::*;
use windows_sys::Win32::UI::WindowsAndMessaging::PostMessageW;

pub const WM_CLIP_IN: u32 = 0x8000 + 5; // WM_APP + 5

const MAX: usize = 16 * 1024 * 1024;
const PING_EVERY: Duration = Duration::from_secs(2);
const DEAD_AFTER: Duration = Duration::from_secs(7);
const HELLO: u8 = 0;
const TEXT: u8 = 1;
const DIB: u8 = 2;
const CF_UNICODETEXT: u32 = 13;
const CF_DIB: u32 = 8;

static RUN: AtomicU32 = AtomicU32::new(0);
static ENABLED: AtomicBool = AtomicBool::new(false);
static MAIN: AtomicUsize = AtomicUsize::new(0);
static LAST: AtomicU64 = AtomicU64::new(0);
static OUT: Mutex<Option<Sender<Vec<u8>>>> = Mutex::new(None);
static INBOX: Mutex<Vec<Vec<u8>>> = Mutex::new(Vec::new());
static APPLIED: Mutex<Option<Instant>> = Mutex::new(None);
/// Id of the connection that currently owns OUT; a newer connection takes over.
static SESSION: AtomicU32 = AtomicU32::new(0);

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

/// Start watching the local clipboard (call on the GUI thread).
pub fn init(hwnd: HWND) {
    MAIN.store(hwnd as usize, Relaxed);
    ENABLED.store(true, Relaxed);
    unsafe {
        AddClipboardFormatListener(hwnd);
    }
}

/// WM_CLIPBOARDUPDATE
pub fn on_local_change() {
    if !ENABLED.load(Relaxed) {
        return;
    }
    // Ignore the echo of what we just wrote ourselves.
    if APPLIED.lock().unwrap().map_or(false, |t| t.elapsed() < Duration::from_millis(800)) {
        return;
    }
    let Some(rec) = read_clipboard() else { return };
    let h = fnv(&rec);
    if LAST.swap(h, Relaxed) == h {
        return;
    }
    if let Some(tx) = OUT.lock().unwrap().as_ref() {
        let _ = tx.send(rec);
    }
}

/// WM_CLIP_IN: the peer sent new clipboard content.
pub fn apply_remote() {
    let rec = INBOX.lock().unwrap().drain(..).last();
    if let Some(rec) = rec {
        LAST.store(fnv(&rec), Relaxed);
        *APPLIED.lock().unwrap() = Some(Instant::now());
        write_clipboard(&rec);
    }
}

// ---------- network ----------

struct RecReader {
    hdr: [u8; 4],
    hfill: usize,
    buf: Vec<u8>,
    bfill: usize,
    len: Option<usize>,
}

impl RecReader {
    fn new() -> Self {
        RecReader { hdr: [0; 4], hfill: 0, buf: Vec::new(), bfill: 0, len: None }
    }

    /// Ok(None) on read timeout, Err on EOF / bad length.
    fn poll(&mut self, s: &mut TcpStream) -> io::Result<Option<Vec<u8>>> {
        loop {
            let (dst, want_hdr): (&mut [u8], bool) = match self.len {
                None => (&mut self.hdr[self.hfill..], true),
                Some(_) => (&mut self.buf[self.bfill..], false),
            };
            if !dst.is_empty() {
                match s.read(dst) {
                    Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
                    Ok(n) => {
                        if want_hdr {
                            self.hfill += n;
                        } else {
                            self.bfill += n;
                        }
                    }
                    Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => {
                        return Ok(None)
                    }
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(e) => return Err(e),
                }
            }
            if self.len.is_none() && self.hfill == 4 {
                let n = u32::from_le_bytes(self.hdr) as usize;
                if n < 16 || n > MAX + 64 {
                    return Err(io::Error::other("bad record length"));
                }
                self.len = Some(n);
                self.buf = vec![0; n];
                self.bfill = 0;
            }
            if let Some(n) = self.len {
                if self.bfill == n {
                    self.len = None;
                    self.hfill = 0;
                    return Ok(Some(std::mem::take(&mut self.buf)));
                }
            }
        }
    }
}

fn write_rec(s: &mut TcpStream, sealer: &mut Sealer, plain: &[u8]) -> io::Result<()> {
    let ct = sealer.seal_vec(plain);
    let mut out = Vec::with_capacity(4 + ct.len());
    out.extend_from_slice(&(ct.len() as u32).to_le_bytes());
    out.extend_from_slice(&ct);
    s.write_all(&out)
}

fn writer(mut s: TcpStream, mut sealer: Sealer, rx: Receiver<Vec<u8>>, gen: u32) {
    let mut last_tx = Instant::now();
    loop {
        let rec = match rx.recv_timeout(Duration::from_millis(500)) {
            Ok(rec) => rec,
            Err(RecvTimeoutError::Timeout) => {
                if RUN.load(Relaxed) != gen {
                    return;
                }
                if last_tx.elapsed() < PING_EVERY {
                    continue;
                }
                vec![HELLO] // keep-alive, ignored by the reader
            }
            Err(RecvTimeoutError::Disconnected) => return,
        };
        if write_rec(&mut s, &mut sealer, &rec).is_err() {
            let _ = s.shutdown(std::net::Shutdown::Both);
            return;
        }
        last_tx = Instant::now();
    }
}

fn session(mut s: TcpStream, is_server: bool, gen: u32) {
    let secret = key();
    if secret.is_empty() {
        return;
    }
    let _ = s.set_nodelay(true);
    let _ = s.set_read_timeout(Some(Duration::from_millis(500)));
    let mut salt = [0u8; SALT];
    let started = Instant::now();
    let ok_time = |st: &Instant| st.elapsed() < Duration::from_secs(5) && RUN.load(Relaxed) == gen;
    if is_server {
        if getrandom::getrandom(&mut salt).is_err() || s.write_all(&salt).is_err() {
            return;
        }
    } else {
        let mut got = 0;
        while got < SALT {
            match s.read(&mut salt[got..]) {
                Ok(0) => return,
                Ok(n) => got += n,
                Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) && ok_time(&started) => {}
                Err(_) => return,
            }
        }
    }
    let (mut sealer, mut opener): (Sealer, Opener) = pair(&secret, &salt, is_server);
    let mut rr = RecReader::new();
    if !is_server && write_rec(&mut s, &mut sealer, &[HELLO]).is_err() {
        return;
    }
    if is_server {
        let hello = loop {
            match rr.poll(&mut s) {
                Ok(Some(ct)) => break opener.open_vec(&ct),
                Ok(None) if ok_time(&started) => {}
                _ => return,
            }
        };
        if hello.as_deref() != Some(&[HELLO][..]) {
            return;
        }
    }

    let (tx, rx) = channel();
    let ws = match s.try_clone() {
        Ok(c) => c,
        Err(_) => return,
    };
    let wt = std::thread::spawn(move || writer(ws, sealer, rx, gen));
    let me = SESSION.fetch_add(1, Relaxed) + 1;
    *OUT.lock().unwrap() = Some(tx);

    let mut last_rx = Instant::now();
    loop {
        if RUN.load(Relaxed) != gen || SESSION.load(Relaxed) != me || last_rx.elapsed() > DEAD_AFTER {
            break;
        }
        let got = rr.poll(&mut s);
        if matches!(got, Ok(Some(_))) {
            last_rx = Instant::now();
        }
        match got {
            Ok(Some(ct)) => match opener.open_vec(&ct) {
                Some(rec) if rec.len() > 1 => {
                    INBOX.lock().unwrap().push(rec);
                    unsafe {
                        PostMessageW(MAIN.load(Relaxed) as HWND, WM_CLIP_IN, 0, 0);
                    }
                }
                Some(_) => {}
                None => break,
            },
            Ok(None) => {}
            Err(_) => break,
        }
    }
    let _ = s.shutdown(std::net::Shutdown::Both);
    if SESSION.load(Relaxed) == me {
        *OUT.lock().unwrap() = None;
        let _ = wt.join();
    }
}

pub fn start_server(port: u16) {
    let gen = RUN.fetch_add(1, Relaxed) + 1;
    std::thread::spawn(move || {
        // The previous listener may still be closing after a quick stop/start.
        let mut listener = None;
        for _ in 0..20 {
            match TcpListener::bind(("0.0.0.0", port)) {
                Ok(l) => {
                    listener = Some(l);
                    break;
                }
                Err(_) => std::thread::sleep(Duration::from_millis(100)),
            }
            if RUN.load(Relaxed) != gen {
                return;
            }
        }
        let Some(l) = listener else {
            crate::ui::log(format!("Буфер обмена: не удалось занять порт {port}"));
            return;
        };
        let _ = l.set_nonblocking(true);
        while RUN.load(Relaxed) == gen {
            match l.accept() {
                Ok((s, _)) => {
                    let _ = s.set_nonblocking(false);
                    std::thread::spawn(move || session(s, true, gen));
                }
                Err(_) => std::thread::sleep(Duration::from_millis(20)),
            }
        }
    });
}

pub fn start_client(host: String, port: u16) {
    let gen = RUN.fetch_add(1, Relaxed) + 1;
    std::thread::spawn(move || {
        while RUN.load(Relaxed) == gen {
            if let Some(addr) = (host.as_str(), port).to_socket_addrs().ok().and_then(|mut a| a.next()) {
                if let Ok(s) = TcpStream::connect_timeout(&addr, Duration::from_secs(2)) {
                    session(s, false, gen);
                }
            }
            for _ in 0..10 {
                if RUN.load(Relaxed) != gen {
                    return;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    });
}

pub fn stop(hwnd: HWND) {
    RUN.fetch_add(1, Relaxed);
    ENABLED.store(false, Relaxed);
    *OUT.lock().unwrap() = None;
    unsafe {
        RemoveClipboardFormatListener(hwnd);
    }
}
