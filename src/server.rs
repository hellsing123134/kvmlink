//! Runs on the machine whose keyboard/mouse you want to use (the PC).

use crate::proto::*;
use crate::sys;
use std::io::Write;
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU64, AtomicUsize, Ordering::Relaxed};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

static REMOTE: AtomicBool = AtomicBool::new(false);
static TX: Mutex<Option<Sender<Msg>>> = Mutex::new(None);
static SIDE_LEFT: AtomicBool = AtomicBool::new(false);
static W: AtomicI32 = AtomicI32::new(0);
static H: AtomicI32 = AtomicI32::new(0);
static MODS: AtomicU32 = AtomicU32::new(0); // bit0 ctrl, bit1 alt
static LAST_LEAVE_MS: AtomicU64 = AtomicU64::new(0);
static START: Mutex<Option<Instant>> = Mutex::new(None);
static RUN: AtomicU32 = AtomicU32::new(0);
static HOOK_M: AtomicUsize = AtomicUsize::new(0);
static HOOK_K: AtomicUsize = AtomicUsize::new(0);
static LISTENER: Mutex<Option<std::thread::JoinHandle<()>>> = Mutex::new(None);
/// Id of the input session that currently owns TX; a newer connection takes over.
static SESSION: AtomicU32 = AtomicU32::new(0);

fn now_ms() -> u64 {
    START.lock().unwrap().unwrap().elapsed().as_millis() as u64
}

fn connected() -> bool {
    TX.lock().unwrap().is_some()
}

fn send(m: Msg) {
    if let Some(tx) = TX.lock().unwrap().as_ref() {
        let _ = tx.send(m);
    }
}

fn enter_remote(y_milli: i32) {
    REMOTE.store(true, Relaxed);
    send(Msg::new(ENTER, y_milli, SIDE_LEFT.load(Relaxed) as i32));
    sys::set_cursor(W.load(Relaxed) / 2, H.load(Relaxed) / 2);
}

/// `y_milli`: where to put the cursor on the PC edge (None keeps it where it is).
fn leave_remote(y_milli: Option<i32>) {
    if !REMOTE.swap(false, Relaxed) {
        return;
    }
    LAST_LEAVE_MS.store(now_ms(), Relaxed);
    send(Msg::new(RELEASE, 0, 0));
    if let Some(y) = y_milli {
        let (w, h) = (W.load(Relaxed), H.load(Relaxed));
        let x = if SIDE_LEFT.load(Relaxed) { 3 } else { w - 4 };
        sys::set_cursor(x, (y.clamp(0, 1000) as i64 * h as i64 / 1000) as i32);
    }
}

unsafe extern "system" fn mouse_proc(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if code < 0 {
        return CallNextHookEx(0 as _, code, wp, lp);
    }
    let ms = &*(lp as *const MSLLHOOKSTRUCT);
    if ms.flags & 1 != 0 {
        return CallNextHookEx(0 as _, code, wp, lp);
    }
    let msg = wp as u32;
    let (w, h) = (W.load(Relaxed), H.load(Relaxed));

    if !REMOTE.load(Relaxed) {
        if msg == WM_MOUSEMOVE && now_ms().saturating_sub(LAST_LEAVE_MS.load(Relaxed)) > 300 && connected() {
            let hit = if SIDE_LEFT.load(Relaxed) { ms.pt.x <= 0 } else { ms.pt.x >= w - 1 };
            if hit {
                enter_remote((ms.pt.y.clamp(0, h - 1) as i64 * 1000 / h as i64) as i32);
                // Swallow this event, otherwise Windows moves the cursor back to the edge
                // and the next delta looks like a half-screen jump.
                return 1;
            }
        }
        return CallNextHookEx(0 as _, code, wp, lp);
    }

    let hi = (ms.mouseData >> 16) as u16;
    match msg {
        WM_MOUSEMOVE => {
            let (dx, dy) = (ms.pt.x - w / 2, ms.pt.y - h / 2);
            if dx != 0 || dy != 0 {
                send(Msg::new(MOVE, dx, dy));
                // Re-centre after every event so the next delta is measured from a known point.
                sys::set_cursor(w / 2, h / 2);
            }
        }
        WM_LBUTTONDOWN => send(Msg::new(BUTTON, 0, 1)),
        WM_LBUTTONUP => send(Msg::new(BUTTON, 0, 0)),
        WM_RBUTTONDOWN => send(Msg::new(BUTTON, 1, 1)),
        WM_RBUTTONUP => send(Msg::new(BUTTON, 1, 0)),
        WM_MBUTTONDOWN => send(Msg::new(BUTTON, 2, 1)),
        WM_MBUTTONUP => send(Msg::new(BUTTON, 2, 0)),
        WM_XBUTTONDOWN => send(Msg::new(BUTTON, 2 + hi as i32, 1)),
        WM_XBUTTONUP => send(Msg::new(BUTTON, 2 + hi as i32, 0)),
        WM_MOUSEWHEEL => send(Msg::new(WHEEL, hi as i16 as i32, 0)),
        WM_MOUSEHWHEEL => send(Msg::new(WHEEL, hi as i16 as i32, 1)),
        _ => {}
    }
    1
}

unsafe extern "system" fn key_proc(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if code < 0 {
        return CallNextHookEx(0 as _, code, wp, lp);
    }
    let k = &*(lp as *const KBDLLHOOKSTRUCT);
    if k.flags & 0x10 != 0 {
        return CallNextHookEx(0 as _, code, wp, lp);
    }
    let up = k.flags & 0x80 != 0;
    let bit = match k.vkCode {
        0xA2 | 0xA3 => 1,
        0xA4 | 0xA5 => 2,
        _ => 0,
    };
    if bit != 0 {
        if up {
            MODS.fetch_and(!bit, Relaxed);
        } else {
            MODS.fetch_or(bit, Relaxed);
        }
    }

    // Ctrl+Alt+F12 flips the keyboard/mouse between the PC and the laptop.
    if !up && k.vkCode == 0x7B && MODS.load(Relaxed) == 3 && connected() {
        if REMOTE.load(Relaxed) {
            leave_remote(None);
        } else {
            sys::release_local_modifiers();
            enter_remote(500);
        }
        return 1;
    }

    if REMOTE.load(Relaxed) {
        let mut scan = k.scanCode as i32;
        if scan == 0 {
            scan = sys::scan_from_vk(k.vkCode) as i32;
        }
        let flags = (up as i32) | (((k.flags & 1) as i32) << 1);
        send(Msg::new(KEY, scan, flags));
        return 1;
    }
    CallNextHookEx(0 as _, code, wp, lp)
}

fn writer(mut s: TcpStream, mut sealer: Sealer, rx: Receiver<Msg>) {
    sys::boost_thread();
    let mut batch: Vec<Msg> = Vec::with_capacity(64);
    let mut out: Vec<u8> = Vec::with_capacity(64 * FRAME);
    loop {
        match rx.recv_timeout(Duration::from_secs(1)) {
            Ok(m) => batch.push(m),
            Err(RecvTimeoutError::Timeout) => batch.push(Msg::new(PING, 0, 0)),
            Err(RecvTimeoutError::Disconnected) => return,
        }
        while let Ok(m) = rx.try_recv() {
            batch.push(m);
        }
        out.clear();
        let mut pending: Option<Msg> = None; // coalesced mouse motion
        for m in batch.drain(..) {
            if m.t == MOVE {
                match pending.as_mut() {
                    Some(p) => {
                        p.a += m.a;
                        p.b += m.b;
                    }
                    None => pending = Some(m),
                }
                continue;
            }
            if let Some(p) = pending.take() {
                out.extend_from_slice(&sealer.seal(p));
            }
            out.extend_from_slice(&sealer.seal(m));
        }
        if let Some(p) = pending.take() {
            out.extend_from_slice(&sealer.seal(p));
        }
        if s.write_all(&out).is_err() {
            let _ = s.shutdown(std::net::Shutdown::Both);
            return;
        }
    }
}

fn read_exact_timeout(s: &mut TcpStream, buf: &mut [u8]) -> bool {
    let _ = s.set_read_timeout(Some(Duration::from_secs(5)));
    std::io::Read::read_exact(s, buf).is_ok()
}

fn session(mut s: TcpStream, gen: u32) {
    let _ = s.set_nodelay(true);
    let mut mode = [0u8];
    if !read_exact_timeout(&mut s, &mut mode) {
        return;
    }
    match mode[0] {
        MODE_CONNECT => {}
        MODE_PAIR => return pairing(s),
        _ => return,
    }
    let key = key();
    if key.is_empty() {
        crate::ui::log("Ноутбук пытается подключиться, но устройства ещё не сопряжены");
        return;
    }
    let mut salt = [0u8; SALT];
    if getrandom::getrandom(&mut salt).is_err() || s.write_all(&salt).is_err() {
        return;
    }
    let _ = s.set_read_timeout(Some(Duration::from_millis(500)));
    let (sealer, mut opener) = pair(&key, &salt, true);
    let mut fr = FrameReader::new();

    let started = Instant::now();
    let hello = loop {
        match fr.poll(&mut s) {
            Ok(Some(f)) => break opener.open(&f),
            Ok(None) if started.elapsed() < Duration::from_secs(5) && RUN.load(Relaxed) == gen => {}
            _ => return,
        }
    };
    if !matches!(hello, Some(m) if m.t == HELLO) {
        crate::ui::log("Отклонено подключение: ключ не совпадает (сопрягите устройства заново)");
        return;
    }
    let peer = s.peer_addr().map(|a| a.ip().to_string()).unwrap_or_default();
    crate::ui::log(format!("Ноутбук подключён: {peer}"));
    crate::ui::status(crate::ui::Status::Connected, &format!("Ноутбук подключён ({peer})"));

    let (tx, rx) = channel();
    let ws = match s.try_clone() {
        Ok(c) => c,
        Err(_) => return,
    };
    let wt = std::thread::spawn(move || writer(ws, sealer, rx));
    let _ = tx.send(Msg::new(PING, 0, 0)); // lets the laptop know the key was accepted
    // A reconnecting laptop replaces a session that has not timed out yet.
    let me = SESSION.fetch_add(1, Relaxed) + 1;
    *TX.lock().unwrap() = Some(tx);

    let mut last_rx = Instant::now();
    loop {
        if RUN.load(Relaxed) != gen || SESSION.load(Relaxed) != me {
            break;
        }
        match fr.poll(&mut s) {
            Ok(Some(f)) => {
                last_rx = Instant::now();
                match opener.open(&f) {
                    Some(m) if m.t == LEAVE => leave_remote(Some(m.a)),
                    Some(_) => {}
                    None => break,
                }
            }
            Ok(None) => {
                if last_rx.elapsed() > Duration::from_secs(5) {
                    break;
                }
            }
            Err(_) => break,
        }
    }
    let _ = s.shutdown(std::net::Shutdown::Both);
    if SESSION.load(Relaxed) != me {
        return; // replaced by a newer connection, which owns TX now
    }
    leave_remote(None);
    *TX.lock().unwrap() = None;
    let _ = wt.join();
    crate::ui::log("Ноутбук отключён");
    if RUN.load(Relaxed) == gen {
        crate::ui::status(crate::ui::Status::Waiting, "Ожидание ноутбука");
    }
}

// ---------- pairing ----------

struct PairWindow {
    pin: String,
    until: Instant,
    fails: u32,
}

static PAIRING: Mutex<Option<PairWindow>> = Mutex::new(None);
const PAIR_SECS: u64 = 120;
const PAIR_MAX_FAILS: u32 = 3;

/// Opens a 2-minute pairing window and returns the PIN to show.
pub fn open_pairing() -> String {
    let mut b = [0u8; 4];
    let pin = loop {
        let _ = getrandom::getrandom(&mut b);
        let v = u32::from_le_bytes(b);
        if v < 4_294_000_000 {
            break format!("{:06}", v % 1_000_000); // rejection sampling keeps all PINs equally likely
        }
    };
    *PAIRING.lock().unwrap() = Some(PairWindow {
        pin: pin.clone(),
        until: Instant::now() + Duration::from_secs(PAIR_SECS),
        fails: 0,
    });
    pin
}

pub fn close_pairing() {
    *PAIRING.lock().unwrap() = None;
}

/// (PIN, seconds left) while a pairing window is open.
pub fn pairing_state() -> Option<(String, u64)> {
    let mut g = PAIRING.lock().unwrap();
    match g.as_ref() {
        Some(w) if w.until > Instant::now() => Some((w.pin.clone(), (w.until - Instant::now()).as_secs())),
        Some(_) => {
            *g = None;
            None
        }
        None => None,
    }
}

/// SPAKE2 over the PIN: a wrong guess reveals nothing, and only 3 guesses are allowed per PIN.
fn pairing(mut s: TcpStream) {
    use spake2::{Ed25519Group, Identity, Password, Spake2};
    let Some((pin, _)) = pairing_state() else {
        let _ = s.write_all(&[0]); // no pairing window open
        return;
    };
    let mut len = [0u8];
    if !read_exact_timeout(&mut s, &mut len) {
        return;
    }
    let mut msg_a = vec![0u8; len[0] as usize];
    if !read_exact_timeout(&mut s, &mut msg_a) {
        return;
    }
    let (sp, msg_b) = Spake2::<Ed25519Group>::start_b(
        &Password::new(pin.as_bytes()),
        &Identity::new(SPAKE_ID_CLIENT),
        &Identity::new(SPAKE_ID_SERVER),
    );
    let mut out = vec![1u8, msg_b.len() as u8];
    out.extend_from_slice(&msg_b);
    if s.write_all(&out).is_err() {
        return;
    }
    let Ok(k) = sp.finish(&msg_a) else { return };

    let confirm = pair_confirm_secret(&k);
    let (mut sealer, mut opener) = pair(&confirm, &[0u8; SALT], true);
    let mut f = [0u8; FRAME];
    let ok = read_exact_timeout(&mut s, &mut f) && matches!(opener.open(&f), Some(m) if m.t == HELLO);
    if !ok {
        let mut g = PAIRING.lock().unwrap();
        if let Some(w) = g.as_mut() {
            w.fails += 1;
            if w.fails >= PAIR_MAX_FAILS {
                *g = None;
                crate::ui::log("Сопряжение отменено: 3 неверных PIN подряд");
            } else {
                crate::ui::log("Неверный PIN при сопряжении");
            }
        }
        return;
    }
    if s.write_all(&sealer.seal(Msg::new(HELLO, 0, 0))).is_err() {
        return;
    }
    close_pairing();
    let ltk = long_term_key(&k);
    set_key(&ltk);
    let peer = s.peer_addr().map(|a| a.ip().to_string()).unwrap_or_default();
    crate::ui::log(format!("Устройство {peer} сопряжено"));
    crate::ui::paired(&ltk);
}

// ---------- discovery ----------

fn discovery(port: u16, gen: u32) {
    let Ok(sock) = std::net::UdpSocket::bind(("0.0.0.0", port)) else { return };
    let _ = sock.set_read_timeout(Some(Duration::from_millis(500)));
    let name = std::env::var("COMPUTERNAME").unwrap_or_default();
    let mut buf = [0u8; 64];
    while RUN.load(Relaxed) == gen {
        if let Ok((n, from)) = sock.recv_from(&mut buf) {
            if &buf[..n] == DISCOVER_ASK {
                let mut reply = DISCOVER_REPLY.to_vec();
                reply.extend_from_slice(name.as_bytes());
                let _ = sock.send_to(&reply, from);
            }
        }
    }
}

pub fn start(port: u16, side_left: bool) -> Result<(), String> {
    *START.lock().unwrap() = Some(Instant::now());
    let (w, h) = sys::screen();
    W.store(w, Relaxed);
    H.store(h, Relaxed);
    SIDE_LEFT.store(side_left, Relaxed);
    REMOTE.store(false, Relaxed);
    MODS.store(0, Relaxed);

    let listener = TcpListener::bind(("0.0.0.0", port)).map_err(|e| format!("Не удалось занять порт {port}: {e}"))?;
    listener.set_nonblocking(true).map_err(|e| e.to_string())?;
    let gen = RUN.fetch_add(1, Relaxed) + 1;
    let t = std::thread::spawn(move || {
        while RUN.load(Relaxed) == gen {
            match listener.accept() {
                Ok((s, _)) => {
                    let _ = s.set_nonblocking(false);
                    std::thread::spawn(move || session(s, gen));
                }
                Err(_) => std::thread::sleep(Duration::from_millis(20)),
            }
        }
    });
    *LISTENER.lock().unwrap() = Some(t);
    std::thread::spawn(move || discovery(port, gen));

    unsafe {
        sys::boost_thread();
        let m = SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_proc), 0 as _, 0);
        let k = SetWindowsHookExW(WH_KEYBOARD_LL, Some(key_proc), 0 as _, 0);
        HOOK_M.store(m as usize, Relaxed);
        HOOK_K.store(k as usize, Relaxed);
    }
    Ok(())
}

pub fn stop() {
    RUN.fetch_add(1, Relaxed);
    close_pairing();
    unsafe {
        for h in [HOOK_M.swap(0, Relaxed), HOOK_K.swap(0, Relaxed)] {
            if h != 0 {
                UnhookWindowsHookEx(h as _);
            }
        }
    }
    leave_remote(None);
    if let Some(t) = LISTENER.lock().unwrap().take() {
        let _ = t.join();
    }
}
