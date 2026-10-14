//! Peer-to-peer links between paired computers. Every copy listens, dials, captures and
//! injects: there are no fixed roles. Pushing the cursor past an edge of this screen where a
//! connected device stands hands the keyboard and mouse to that device; the cursor comes back
//! through the opposite edge of the other screen.

use crate::config::{self, Side};
use crate::proto::*;
use crate::{clip, sys};
use std::collections::HashMap;
use std::io::Write;
use std::net::{TcpListener, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU64, Ordering::Relaxed};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::System::Threading::GetCurrentThreadId;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

/// How a paired device is doing, for the window.
#[derive(Clone, Copy, PartialEq)]
pub enum State {
    Offline,
    Online,
    /// Our keyboard and mouse are on that device.
    Controlling,
    /// That device's keyboard and mouse are on this one.
    ControlledBy,
}

enum Out {
    Msg(Msg),
    Clip(Arc<Vec<u8>>),
}

struct Link {
    peer: u64,
    serial: u32,
    tx: Sender<Out>,
}

static RUN: AtomicU32 = AtomicU32::new(0);
static RUNNING: AtomicBool = AtomicBool::new(false);
static LINKS: Mutex<Vec<Link>> = Mutex::new(Vec::new());
static SERIAL: AtomicU32 = AtomicU32::new(0);
static START: Mutex<Option<Instant>> = Mutex::new(None);
/// Peer id per side (Side code 0..4), 0 = nobody. Copied from the settings for the hooks.
static SIDES: Mutex<[u64; 4]> = Mutex::new([0; 4]);
static BLOCK: AtomicBool = AtomicBool::new(false);
static HOOK_THREAD: AtomicU32 = AtomicU32::new(0);

// Outgoing control: our input goes to REMOTE_PEER (0 = it stays here).
static REMOTE_PEER: AtomicU64 = AtomicU64::new(0);
static REMOTE_SIDE: AtomicI32 = AtomicI32::new(0);
static ENTER_POS: AtomicI32 = AtomicI32::new(500);
static LAST_LEAVE_MS: AtomicU64 = AtomicU64::new(0);
static MODS: AtomicU32 = AtomicU32::new(0); // bit0 ctrl, bit1 alt
/// Pixels next to each screen corner where the edges do not switch.
const CORNER: i32 = 12;

// Incoming control: CONTROLLER's input is injected here (0 = nobody).
static CONTROLLER: AtomicU64 = AtomicU64::new(0);
static INJ: Mutex<Injector> = Mutex::new(Injector { edge: Side::Left, keys: Vec::new(), buttons: [false; 5] });

fn now_ms() -> u64 {
    START.lock().unwrap().get_or_insert_with(Instant::now).elapsed().as_millis() as u64
}

/// Logs `text` unless the same line was logged during the last minute (retries repeat errors).
fn log_rarely(text: String) {
    static SEEN: Mutex<Vec<(String, u64)>> = Mutex::new(Vec::new());
    let now = now_ms();
    let mut seen = SEEN.lock().unwrap();
    seen.retain(|(_, t)| now - *t < 60_000);
    if !seen.iter().any(|(s, _)| *s == text) {
        seen.push((text.clone(), now));
        crate::ui::log(text);
    }
}

fn my_name() -> String {
    std::env::var("COMPUTERNAME").unwrap_or_else(|_| "Computer".into())
}

fn peer_name(id: u64) -> String {
    config::get().peer(id).map(|p| p.name.clone()).unwrap_or_else(|| format!("{id:016x}"))
}

pub fn running() -> bool {
    RUNNING.load(Relaxed)
}

pub fn state(id: u64) -> State {
    if REMOTE_PEER.load(Relaxed) == id {
        State::Controlling
    } else if CONTROLLER.load(Relaxed) == id {
        State::ControlledBy
    } else if connected(id) {
        State::Online
    } else {
        State::Offline
    }
}

fn connected(id: u64) -> bool {
    LINKS.lock().unwrap().iter().any(|l| l.peer == id)
}

fn any_connected() -> bool {
    !LINKS.lock().unwrap().is_empty()
}

fn send_to(id: u64, m: Msg) {
    if let Some(l) = LINKS.lock().unwrap().iter().find(|l| l.peer == id) {
        let _ = l.tx.send(Out::Msg(m));
    }
}

/// Sends clipboard content to every connected device except `except`.
pub fn broadcast_clip(rec: Arc<Vec<u8>>, except: u64) {
    for l in LINKS.lock().unwrap().iter().filter(|l| l.peer != except) {
        let _ = l.tx.send(Out::Clip(rec.clone()));
    }
}

/// Re-reads the layout and options from the settings. Call after every settings change.
pub fn refresh() {
    let c = config::get();
    let mut lay = [0u64; 4];
    for p in &c.peers {
        if let Some(s) = p.side {
            lay[Side::code(Some(s)) as usize] = p.id;
        }
    }
    *SIDES.lock().unwrap() = lay;
    BLOCK.store(c.block, Relaxed);
}

/// The user moved `id` to `side` in the window.
pub fn place(id: u64, side: Option<Side>) {
    let stamp = config::now_stamp();
    let changed = config::update(|c| c.place(id, side, stamp));
    refresh();
    let c = config::get();
    for pid in changed {
        if let Some(p) = c.peer(pid) {
            send_to(pid, Msg::new(LAYOUT, Side::code(p.side.map(Side::opposite)), p.stamp));
        }
        if REMOTE_PEER.load(Relaxed) == pid {
            leave_remote(None);
        }
    }
}

/// Removes a pairing on both sides (the other side only if it is connected right now).
pub fn forget(id: u64) {
    send_to(id, Msg::new(FORGET, 0, 0));
    std::thread::sleep(Duration::from_millis(50));
    drop_peer(id);
}

fn drop_peer(id: u64) {
    config::update(|c| c.peers.retain(|p| p.id != id));
    refresh();
    if REMOTE_PEER.load(Relaxed) == id {
        leave_remote(None);
    }
    LINKS.lock().unwrap().retain(|l| l.peer != id);
    crate::ui::notify();
}

/// The other side told us where it stands relative to us. The newer change wins.
fn on_layout(from: u64, side: Option<Side>, stamp: i32) {
    let me = config::get();
    let Some(p) = me.peer(from) else { return };
    let theirs_wins = stamp > p.stamp || (stamp == p.stamp && from < me.id);
    if p.side == side || !theirs_wins {
        return;
    }
    let changed = config::update(|c| c.place(from, side, stamp));
    refresh();
    let c = config::get();
    for pid in changed.into_iter().filter(|i| *i != from) {
        if let Some(p) = c.peer(pid) {
            send_to(pid, Msg::new(LAYOUT, Side::code(p.side.map(Side::opposite)), p.stamp));
        }
    }
    crate::ui::log(match side {
        Some(s) => format!("{} now stands on the {} of this screen", peer_name(from), s.name()),
        None => format!("{} was taken off the layout", peer_name(from)),
    });
    crate::ui::notify();
}

// ---------- outgoing control (input hooks) ----------

fn edge_pos(side: Side, x: i32, y: i32) -> i32 {
    let (vx, vy, vw, vh) = sys::desk();
    let v = match side {
        Side::Left | Side::Right => (y - vy).clamp(0, vh - 1) as i64 * 1000 / vh.max(1) as i64,
        Side::Top | Side::Bottom => (x - vx).clamp(0, vw - 1) as i64 * 1000 / vw.max(1) as i64,
    };
    v as i32
}

fn enter_remote(id: u64, side: Side, pos: i32) {
    drop_inbound_async(); // both pushed at the same moment: ours wins locally
    REMOTE_SIDE.store(Side::code(Some(side)), Relaxed);
    ENTER_POS.store(pos, Relaxed);
    REMOTE_PEER.store(id, Relaxed);
    send_to(id, Msg::new(ENTER, pos, Side::code(Some(side.opposite()))));
    let (w, h) = sys::screen();
    sys::set_cursor(w / 2, h / 2);
    crate::ui::notify();
}

/// `pos`: where to put the cursor on our edge; None leaves it where it is, -1 = where it left.
fn leave_remote(pos: Option<i32>) {
    let id = REMOTE_PEER.swap(0, Relaxed);
    if id == 0 {
        return;
    }
    LAST_LEAVE_MS.store(now_ms(), Relaxed);
    send_to(id, Msg::new(RELEASE, 0, 0));
    if let Some(mut p) = pos {
        if p < 0 {
            p = ENTER_POS.load(Relaxed);
        }
        let p = p.clamp(0, 1000) as i64;
        let (vx, vy, vw, vh) = sys::desk();
        let along_y = vy + (p * vh as i64 / 1000) as i32;
        let along_x = vx + (p * vw as i64 / 1000) as i32;
        let (x, y) = match Side::from_code(REMOTE_SIDE.load(Relaxed)) {
            Some(Side::Left) => (vx + 3, along_y),
            Some(Side::Right) => (vx + vw - 4, along_y),
            Some(Side::Top) => (along_x, vy + 3),
            _ => (along_x, vy + vh - 4),
        };
        sys::set_cursor(x, y);
    }
    crate::ui::notify();
}

/// Connected devices placed on the layout, in a fixed order (for the switch hotkey).
fn placed_online() -> Vec<(u64, Side)> {
    let lay = *SIDES.lock().unwrap();
    Side::ALL
        .into_iter()
        .filter_map(|s| {
            let id = lay[Side::code(Some(s)) as usize];
            (id != 0 && connected(id)).then_some((id, s))
        })
        .collect()
}

unsafe extern "system" fn mouse_proc(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if code < 0 {
        return CallNextHookEx(0 as _, code, wp, lp);
    }
    let ms = &*(lp as *const MSLLHOOKSTRUCT);
    if ms.flags & 1 != 0 {
        return CallNextHookEx(0 as _, code, wp, lp); // injected, by us or by another program
    }
    let msg = wp as u32;

    // Someone controls this computer and the local mouse moves: the local user takes over.
    let ctl = CONTROLLER.load(Relaxed);
    if ctl != 0 && REMOTE_PEER.load(Relaxed) == 0 {
        send_to(ctl, Msg::new(LEAVE, -1, 0));
        drop_inbound_async();
        LAST_LEAVE_MS.store(now_ms(), Relaxed);
        crate::ui::notify();
        return CallNextHookEx(0 as _, code, wp, lp);
    }

    if REMOTE_PEER.load(Relaxed) == 0 {
        if msg == WM_MOUSEMOVE && now_ms().saturating_sub(LAST_LEAVE_MS.load(Relaxed)) > 300 {
            let (x, y) = (ms.pt.x, ms.pt.y);
            let (vx, vy, vw, vh) = sys::desk();
            // Corners stay local: that is where the close button of a maximised window lives.
            let corner = (x <= vx + CORNER || x >= vx + vw - 1 - CORNER) && (y <= vy + CORNER || y >= vy + vh - 1 - CORNER);
            let hits = if corner { [(false, Side::Left); 4] } else { [
                (x <= vx, Side::Left),
                (x >= vx + vw - 1, Side::Right),
                (y <= vy, Side::Top),
                (y >= vy + vh - 1, Side::Bottom),
            ] };
            let lay = *SIDES.lock().unwrap();
            for (hit, side) in hits {
                let id = lay[Side::code(Some(side)) as usize];
                if hit && id != 0 && connected(id) {
                    enter_remote(id, side, edge_pos(side, x, y));
                    // Swallow this event, otherwise Windows moves the cursor back to the edge
                    // and the next delta looks like a half-screen jump.
                    return 1;
                }
            }
        }
        return CallNextHookEx(0 as _, code, wp, lp);
    }

    let id = REMOTE_PEER.load(Relaxed);
    let hi = (ms.mouseData >> 16) as u16;
    let m = match msg {
        WM_MOUSEMOVE => {
            let (w, h) = sys::screen();
            let (dx, dy) = (ms.pt.x - w / 2, ms.pt.y - h / 2);
            if dx == 0 && dy == 0 {
                return 1;
            }
            // Re-centre after every event so the next delta is measured from a known point.
            sys::set_cursor(w / 2, h / 2);
            Msg::new(MOVE, dx, dy)
        }
        WM_LBUTTONDOWN => Msg::new(BUTTON, 0, 1),
        WM_LBUTTONUP => Msg::new(BUTTON, 0, 0),
        WM_RBUTTONDOWN => Msg::new(BUTTON, 1, 1),
        WM_RBUTTONUP => Msg::new(BUTTON, 1, 0),
        WM_MBUTTONDOWN => Msg::new(BUTTON, 2, 1),
        WM_MBUTTONUP => Msg::new(BUTTON, 2, 0),
        WM_XBUTTONDOWN => Msg::new(BUTTON, 2 + hi as i32, 1),
        WM_XBUTTONUP => Msg::new(BUTTON, 2 + hi as i32, 0),
        WM_MOUSEWHEEL => Msg::new(WHEEL, hi as i16 as i32, 0),
        WM_MOUSEHWHEEL => Msg::new(WHEEL, hi as i16 as i32, 1),
        _ => return 1,
    };
    send_to(id, m);
    1
}

unsafe extern "system" fn key_proc(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if code < 0 {
        return CallNextHookEx(0 as _, code, wp, lp);
    }
    let k = &*(lp as *const KBDLLHOOKSTRUCT);
    if k.flags & 0x10 != 0 {
        return CallNextHookEx(0 as _, code, wp, lp); // injected
    }
    // A faulty built-in keyboard is ignored completely while another device is connected.
    if BLOCK.load(Relaxed) && any_connected() {
        return 1;
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

    // Ctrl+Alt+F12 moves the keyboard and mouse to the next connected device, then back home.
    if !up && k.vkCode == 0x7B && MODS.load(Relaxed) == 3 {
        let targets = placed_online();
        let cur = REMOTE_PEER.load(Relaxed);
        let next = match targets.iter().position(|(id, _)| *id == cur) {
            Some(i) => targets.get(i + 1).copied(),
            None if cur == 0 => targets.first().copied(),
            None => None,
        };
        if cur != 0 {
            leave_remote(None);
        }
        if let Some((id, side)) = next {
            sys::release_local_modifiers();
            enter_remote(id, side, 500);
        }
        return 1;
    }

    let id = REMOTE_PEER.load(Relaxed);
    if id != 0 {
        let mut scan = k.scanCode as i32;
        if scan == 0 {
            scan = sys::scan_from_vk(k.vkCode) as i32;
        }
        let flags = (up as i32) | (((k.flags & 1) as i32) << 1);
        send_to(id, Msg::new(KEY, scan, flags));
        return 1;
    }
    CallNextHookEx(0 as _, code, wp, lp)
}

fn start_hooks() {
    // Two test copies on one machine must not both grab the real mouse.
    if std::env::var_os("KVMLINK_NO_HOOKS").is_some() {
        return;
    }
    std::thread::spawn(|| unsafe {
        sys::boost_thread();
        HOOK_THREAD.store(GetCurrentThreadId(), Relaxed);
        let m = SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_proc), 0 as _, 0);
        let k = SetWindowsHookExW(WH_KEYBOARD_LL, Some(key_proc), 0 as _, 0);
        sys::message_loop();
        UnhookWindowsHookEx(m);
        UnhookWindowsHookEx(k);
    });
}

fn stop_hooks() {
    let t = HOOK_THREAD.swap(0, Relaxed);
    if t != 0 {
        unsafe {
            PostThreadMessageW(t, WM_QUIT, 0, 0);
        }
    }
}

// ---------- incoming control (injection) ----------

struct Injector {
    /// Edge of our screen the cursor came in through; leaving through it goes back.
    edge: Side,
    keys: Vec<(u16, bool)>,
    buttons: [bool; 5],
}

impl Injector {
    fn release_all(&mut self) {
        for (scan, ext) in self.keys.drain(..) {
            sys::key(scan, true, ext);
        }
        for (i, down) in self.buttons.iter_mut().enumerate() {
            if *down {
                sys::button(i as i32, false);
                *down = false;
            }
        }
    }
}

fn release_inbound() {
    INJ.lock().unwrap().release_all();
    CONTROLLER.store(0, Relaxed);
}

/// For the hook thread: injecting keys from there could wait on a session thread that is itself
/// waiting for the hook, so the stuck keys are released from a short-lived thread instead.
fn drop_inbound_async() {
    if CONTROLLER.swap(0, Relaxed) != 0 {
        std::thread::spawn(|| INJ.lock().unwrap().release_all());
    }
}

/// Applies one input event from `from`. Returns a reply for the sender, if any.
fn inject(from: u64, m: Msg) -> Option<Msg> {
    let mut inj = INJ.lock().unwrap();
    match m.t {
        ENTER => {
            if REMOTE_PEER.load(Relaxed) != 0 {
                leave_remote(None); // both pushed at the same moment: theirs wins here
            }
            inj.release_all();
            inj.edge = Side::from_code(m.b).unwrap_or(Side::Left);
            CONTROLLER.store(from, Relaxed);
            let (vx, vy, vw, vh) = sys::desk();
            let p = m.a.clamp(0, 1000) as i64;
            let (x, y) = match inj.edge {
                Side::Left => (vx, vy + (p * vh as i64 / 1000) as i32),
                Side::Right => (vx + vw - 1, vy + (p * vh as i64 / 1000) as i32),
                Side::Top => (vx + (p * vw as i64 / 1000) as i32, vy),
                Side::Bottom => (vx + (p * vw as i64 / 1000) as i32, vy + vh - 1),
            };
            sys::move_abs(x, y);
            crate::ui::notify();
            return None;
        }
        RELEASE => {
            if CONTROLLER.load(Relaxed) == from {
                inj.release_all();
                CONTROLLER.store(0, Relaxed);
                crate::ui::notify();
            }
            return None;
        }
        _ => {}
    }
    if CONTROLLER.load(Relaxed) != from {
        return None;
    }
    match m.t {
        MOVE => {
            let (vx, vy, vw, vh) = sys::desk();
            let (cx, cy) = sys::cursor();
            let (nx, ny) = (cx + m.a, cy + m.b);
            let out = match inj.edge {
                Side::Left => nx < vx && m.a < 0,
                Side::Right => nx > vx + vw - 1 && m.a > 0,
                Side::Top => ny < vy && m.b < 0,
                Side::Bottom => ny > vy + vh - 1 && m.b > 0,
            };
            if out {
                let pos = edge_pos(inj.edge, cx, cy);
                inj.release_all();
                CONTROLLER.store(0, Relaxed);
                crate::ui::notify();
                return Some(Msg::new(LEAVE, pos, 0));
            }
            sys::move_abs(nx.clamp(vx, vx + vw - 1), ny.clamp(vy, vy + vh - 1));
        }
        KEY => {
            let (scan, up, ext) = (m.a as u16, m.b & 1 != 0, m.b & 2 != 0);
            sys::key(scan, up, ext);
            inj.keys.retain(|k| *k != (scan, ext));
            if !up {
                inj.keys.push((scan, ext));
            }
        }
        BUTTON => {
            let i = (m.a as usize).min(4);
            inj.buttons[i] = m.b != 0;
            sys::button(i as i32, m.b != 0);
        }
        WHEEL => sys::wheel(m.a, m.b == 1),
        _ => {}
    }
    None
}

// ---------- a link to one paired device ----------

fn writer(mut s: TcpStream, mut sealer: Sealer, rx: Receiver<Out>) {
    sys::boost_thread();
    let mut batch: Vec<Msg> = Vec::with_capacity(64);
    let mut out: Vec<u8> = Vec::with_capacity(64 * 32);
    // Clipboard being sent, and how much of it has gone out.
    let mut clip: Option<(Arc<Vec<u8>>, usize)> = None;
    loop {
        // Block only when there is no clipboard data waiting to go out.
        let first = if clip.is_some() {
            match rx.try_recv() {
                Ok(o) => Some(o),
                Err(TryRecvError::Empty) => None,
                Err(TryRecvError::Disconnected) => return,
            }
        } else {
            match rx.recv_timeout(Duration::from_secs(1)) {
                Ok(o) => Some(o),
                Err(RecvTimeoutError::Timeout) => Some(Out::Msg(Msg::new(PING, 0, 0))),
                Err(RecvTimeoutError::Disconnected) => return,
            }
        };
        let mut take = |o: Out, clip: &mut Option<(Arc<Vec<u8>>, usize)>| match o {
            Out::Msg(m) => batch.push(m),
            Out::Clip(c) => *clip = Some((c, 0)), // only the newest copy matters
        };
        if let Some(o) = first {
            take(o, &mut clip);
        }
        while let Ok(o) = rx.try_recv() {
            take(o, &mut clip);
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
                sealer.seal_msg(p, &mut out);
            }
            sealer.seal_msg(m, &mut out);
        }
        if let Some(p) = pending.take() {
            sealer.seal_msg(p, &mut out);
        }
        if let Some((data, off)) = clip.as_mut() {
            let end = (*off + CLIP_CHUNK).min(data.len());
            let last = end == data.len();
            let mut rec = Vec::with_capacity(end - *off + 1);
            rec.push(if last { CLIP_END } else { CLIP_PART });
            rec.extend_from_slice(&data[*off..end]);
            sealer.seal_into(&rec, &mut out);
            *off = end;
            if last {
                clip = None;
            }
        }
        if s.write_all(&out).is_err() {
            let _ = s.shutdown(std::net::Shutdown::Both);
            return;
        }
    }
}

/// Runs an authenticated connection until it drops.
fn run_link(mut s: TcpStream, peer: u64, sealer: Sealer, mut opener: Opener, mut rr: RecReader, gen: u32) {
    let _ = s.set_read_timeout(Some(Duration::from_millis(500)));
    let (tx, rx) = channel();
    let Ok(ws) = s.try_clone() else { return };
    let wt = std::thread::spawn(move || writer(ws, sealer, rx));
    let serial = SERIAL.fetch_add(1, Relaxed) + 1;
    {
        // A reconnecting device replaces a link that has not timed out yet.
        let mut links = LINKS.lock().unwrap();
        links.retain(|l| l.peer != peer);
        links.push(Link { peer, serial, tx: tx.clone() });
    }
    let ip = s.peer_addr().map(|a| a.ip().to_string()).unwrap_or_default();
    config::update(|c| {
        if let Some(p) = c.peer_mut(peer) {
            if p.host != ip && !ip.is_empty() {
                p.host = ip.clone();
            }
        }
    });
    if let Some(p) = config::get().peer(peer) {
        let _ = tx.send(Out::Msg(Msg::new(LAYOUT, Side::code(p.side.map(Side::opposite)), p.stamp)));
    }
    crate::ui::log(format!("Connected to {} ({ip})", peer_name(peer)));
    crate::ui::notify();

    let alive = || RUN.load(Relaxed) == gen && LINKS.lock().unwrap().iter().any(|l| l.serial == serial);
    let mut clip_buf: Vec<u8> = Vec::new();
    let mut last_rx = Instant::now();
    let mut forgotten = false;
    while alive() {
        match rr.poll(&mut s) {
            Ok(Some(ct)) => {
                last_rx = Instant::now();
                let Some(rec) = opener.open(&ct) else { break };
                match rec.first() {
                    Some(&CLIP_PART) | Some(&CLIP_END) => {
                        if clip_buf.len() + rec.len() > CLIP_MAX + 1 {
                            clip_buf.clear(); // too big: drop it
                            continue;
                        }
                        clip_buf.extend_from_slice(&rec[1..]);
                        if rec[0] == CLIP_END {
                            clip::incoming(peer, std::mem::take(&mut clip_buf));
                        }
                    }
                    _ => {
                        let Some(m) = Msg::parse(&rec) else { break };
                        match m.t {
                            PING | HELLO => {}
                            LEAVE => {
                                if REMOTE_PEER.load(Relaxed) == peer {
                                    leave_remote(Some(m.a));
                                }
                            }
                            LAYOUT => on_layout(peer, Side::from_code(m.a), m.b),
                            FORGET => {
                                forgotten = true;
                                break;
                            }
                            _ => {
                                if let Some(reply) = inject(peer, m) {
                                    let _ = tx.send(Out::Msg(reply));
                                }
                            }
                        }
                    }
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
    let mine = {
        let mut links = LINKS.lock().unwrap();
        let mine = links.iter().any(|l| l.serial == serial);
        links.retain(|l| l.serial != serial);
        mine
    };
    drop(tx);
    let _ = wt.join();
    if !mine && !forgotten {
        return; // replaced by a newer connection
    }
    if REMOTE_PEER.load(Relaxed) == peer {
        leave_remote(None);
    }
    if CONTROLLER.load(Relaxed) == peer {
        release_inbound();
    }
    if forgotten {
        crate::ui::log(format!("{} removed the pairing", peer_name(peer)));
        drop_peer(peer);
    } else if RUN.load(Relaxed) == gen {
        crate::ui::log(format!("{} disconnected", peer_name(peer)));
    }
    crate::ui::notify();
}

/// Incoming connection on the main port: a paired device, or a pairing attempt.
fn accept(mut s: TcpStream, gen: u32) {
    let _ = s.set_nodelay(true);
    let _ = s.set_read_timeout(Some(Duration::from_secs(5)));
    let mut mode = [0u8];
    if std::io::Read::read_exact(&mut s, &mut mode).is_err() {
        return;
    }
    match mode[0] {
        MODE_CONNECT => {}
        MODE_PAIR => return pair_respond(s),
        _ => return,
    }
    let mut idb = [0u8; 8];
    if std::io::Read::read_exact(&mut s, &mut idb).is_err() {
        return;
    }
    let peer = u64::from_le_bytes(idb);
    let Some(key) = config::get().peer(peer).map(|p| p.key.clone()) else {
        let ip = s.peer_addr().map(|a| a.ip().to_string()).unwrap_or_default();
        log_rarely(format!("Unknown device {ip} tried to connect (pair it first)"));
        return;
    };
    let mut salt = [0u8; SALT];
    if getrandom::getrandom(&mut salt).is_err() || s.write_all(&salt).is_err() {
        return;
    }
    let _ = s.set_read_timeout(Some(Duration::from_millis(500)));
    let (mut sealer, mut opener) = pair(&key, &salt, true);
    let mut rr = RecReader::new();
    let hello = rr.read_one(&mut s, 5).ok().and_then(|ct| opener.open(&ct)).and_then(|r| Msg::parse(&r));
    if !matches!(hello, Some(m) if m.t == HELLO) {
        log_rarely(format!("{} was rejected: keys differ (pair the devices again)", peer_name(peer)));
        return;
    }
    if sealer.write(&mut s, &Msg::new(HELLO, 0, 0).bytes()).is_err() {
        return;
    }
    run_link(s, peer, sealer, opener, rr, gen);
}

/// Port of the other devices: the same as ours, except for a test copy on one machine
/// (KVMLINK_PEER_PORT).
pub fn peer_port() -> u16 {
    std::env::var("KVMLINK_PEER_PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(config::get().port)
}

fn resolve(host: &str, port: u16) -> std::io::Result<std::net::SocketAddr> {
    (host, port)
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| std::io::Error::other("invalid address"))
}

/// Outgoing connection to a paired device. Err only before the link was established.
fn dial(peer: u64, gen: u32) -> Result<(), String> {
    let c = config::get();
    let Some(p) = c.peer(peer) else { return Ok(()) };
    let addr = resolve(&p.host, peer_port()).map_err(|e| e.to_string())?;
    let mut s = TcpStream::connect_timeout(&addr, Duration::from_secs(2)).map_err(|e| e.to_string())?;
    let _ = s.set_nodelay(true);
    let _ = s.set_read_timeout(Some(Duration::from_secs(3)));
    let mut hello = vec![MODE_CONNECT];
    hello.extend_from_slice(&c.id.to_le_bytes());
    s.write_all(&hello).map_err(|e| e.to_string())?;
    let mut salt = [0u8; SALT];
    std::io::Read::read_exact(&mut s, &mut salt).map_err(|_| "the other computer does not know this one".to_string())?;
    let _ = s.set_read_timeout(Some(Duration::from_millis(500)));
    let (mut sealer, mut opener) = pair(&p.key, &salt, false);
    sealer.write(&mut s, &Msg::new(HELLO, 0, 0).bytes()).map_err(|e| e.to_string())?;
    let mut rr = RecReader::new();
    let ok = rr.read_one(&mut s, 5).ok().and_then(|ct| opener.open(&ct)).and_then(|r| Msg::parse(&r));
    if !matches!(ok, Some(m) if m.t == HELLO) {
        return Err("the other computer rejected the key; pair the devices again".into());
    }
    run_link(s, peer, sealer, opener, rr, gen);
    Ok(())
}

/// Keeps a connection to every paired device. Of each pair, the device with the smaller id
/// dials and the other one listens, so there is never more than one link between two devices.
fn supervisor(gen: u32) {
    struct Try {
        next: Instant,
        fails: u32,
        busy: Arc<AtomicBool>,
    }
    let mut tries: HashMap<u64, Try> = HashMap::new();
    while RUN.load(Relaxed) == gen {
        let c = config::get();
        tries.retain(|id, _| c.peer(*id).is_some());
        for p in c.peers.iter().filter(|p| c.id < p.id) {
            let t = tries.entry(p.id).or_insert(Try { next: Instant::now(), fails: 0, busy: Arc::new(AtomicBool::new(false)) });
            if t.busy.load(Relaxed) || connected(p.id) || Instant::now() < t.next {
                continue;
            }
            t.busy.store(true, Relaxed);
            let (id, busy, fails) = (p.id, t.busy.clone(), t.fails);
            // Its address may have changed: look for it on the LAN every few failed attempts.
            if fails > 0 && fails % 3 == 0 {
                let port = peer_port();
                std::thread::spawn(move || {
                    if let Some(f) = discover(port).into_iter().find(|f| f.id == id) {
                        config::update(|c| {
                            if let Some(p) = c.peer_mut(id) {
                                p.host = f.ip.clone();
                            }
                        });
                    }
                });
            }
            std::thread::spawn(move || {
                if let Err(e) = dial(id, gen) {
                    if fails == 0 {
                        crate::ui::log(format!("Cannot reach {}: {e}. Retrying…", peer_name(id)));
                    }
                }
                busy.store(false, Relaxed);
            });
            t.fails += 1;
            t.next = Instant::now() + Duration::from_secs((t.fails as u64).min(5));
        }
        // A link that came up resets the back-off.
        for (id, t) in tries.iter_mut() {
            if connected(*id) {
                t.fails = 0;
                t.next = Instant::now();
            }
        }
        std::thread::sleep(Duration::from_millis(300));
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

/// "id\nside\nname": who we are, and where the receiver should put us.
fn info(side: Option<Side>) -> Vec<u8> {
    format!("{:016x}\n{}\n{}", config::get().id, Side::code(side), my_name()).into_bytes()
}

fn parse_info(b: &[u8]) -> Option<(u64, Option<Side>, String)> {
    let s = String::from_utf8_lossy(b);
    let mut l = s.splitn(3, '\n');
    let id = u64::from_str_radix(l.next()?, 16).ok().filter(|i| *i != 0)?;
    let side = Side::from_code(l.next()?.parse().ok()?);
    let name: String = l.next()?.chars().filter(|c| !c.is_control() && *c != '|').take(64).collect();
    Some((id, side, name))
}

/// Saves a new pairing (or replaces an older one with the same device).
fn add_peer(id: u64, name: String, host: String, key: String, side: Option<Side>) {
    let stamp = config::now_stamp();
    let changed = config::update(|c| {
        c.peers.retain(|p| p.id != id);
        c.peers.push(config::Peer { id, name, host, key, side: None, stamp });
        c.legacy = false;
        c.place(id, side, stamp)
    });
    refresh();
    let c = config::get();
    for pid in changed.into_iter().filter(|i| *i != id) {
        if let Some(p) = c.peer(pid) {
            send_to(pid, Msg::new(LAYOUT, Side::code(p.side.map(Side::opposite)), p.stamp));
        }
    }
    crate::ui::notify();
}

/// The side showing the PIN. SPAKE2: a wrong guess reveals nothing, 3 guesses per PIN.
fn pair_respond(mut s: TcpStream) {
    use spake2::{Ed25519Group, Identity, Password, Spake2};
    use std::io::Read;
    let Some((pin, _)) = pairing_state() else {
        let _ = s.write_all(&[0]); // no pairing window open
        return;
    };
    let mut len = [0u8];
    if s.read_exact(&mut len).is_err() {
        return;
    }
    let mut msg_a = vec![0u8; len[0] as usize];
    if s.read_exact(&mut msg_a).is_err() {
        return;
    }
    let (sp, msg_b) = Spake2::<Ed25519Group>::start_b(
        &Password::new(pin.as_bytes()),
        &Identity::new(SPAKE_ID_A),
        &Identity::new(SPAKE_ID_B),
    );
    let mut out = vec![1u8, msg_b.len() as u8];
    out.extend_from_slice(&msg_b);
    if s.write_all(&out).is_err() {
        return;
    }
    let Ok(k) = sp.finish(&msg_a) else { return };

    let confirm = pair_confirm_secret(&k);
    let (mut sealer, mut opener) = pair(&confirm, &[0u8; SALT], true);
    let mut rr = RecReader::new();
    let ok = rr.read_one(&mut s, 5).ok().and_then(|ct| opener.open(&ct)).and_then(|r| Msg::parse(&r));
    if !matches!(ok, Some(m) if m.t == HELLO) {
        let mut g = PAIRING.lock().unwrap();
        if let Some(w) = g.as_mut() {
            w.fails += 1;
            if w.fails >= PAIR_MAX_FAILS {
                *g = None;
                crate::ui::log("Pairing cancelled: 3 wrong PINs in a row");
            } else {
                crate::ui::log("A wrong PIN was entered on the other computer");
            }
        }
        crate::ui::notify();
        return;
    }
    if sealer.write(&mut s, &Msg::new(HELLO, 0, 0).bytes()).is_err() {
        return;
    }
    if sealer.write(&mut s, &info(None)).is_err() {
        return;
    }
    let Some((id, side, name)) = rr.read_one(&mut s, 5).ok().and_then(|ct| opener.open(&ct)).and_then(|r| parse_info(&r)) else {
        return;
    };
    close_pairing();
    let host = s.peer_addr().map(|a| a.ip().to_string()).unwrap_or_default();
    add_peer(id, name.clone(), host.clone(), long_term_key(&k), side);
    crate::ui::log(format!("Paired with {name} ({host})"));
    crate::ui::paired(&name);
}

/// The side typing the PIN. Returns the other device's name.
pub fn pair_with(host: &str, pin: &str) -> Result<String, String> {
    use spake2::{Ed25519Group, Identity, Password, Spake2};
    use std::io::Read;
    let addr = resolve(host, peer_port()).map_err(|e| e.to_string())?;
    let mut s = TcpStream::connect_timeout(&addr, Duration::from_secs(3)).map_err(|e| format!("computer unreachable: {e}"))?;
    let _ = s.set_read_timeout(Some(Duration::from_secs(5)));
    let (sp, msg_a) = Spake2::<Ed25519Group>::start_a(
        &Password::new(pin.as_bytes()),
        &Identity::new(SPAKE_ID_A),
        &Identity::new(SPAKE_ID_B),
    );
    let mut out = vec![MODE_PAIR, msg_a.len() as u8];
    out.extend_from_slice(&msg_a);
    let io = |e: std::io::Error| format!("network error: {e}");
    s.write_all(&out).map_err(io)?;
    let mut hdr = [0u8; 1];
    s.read_exact(&mut hdr).map_err(io)?;
    if hdr[0] != 1 {
        return Err("the other computer is not showing a PIN (click Add device → Show PIN there)".into());
    }
    s.read_exact(&mut hdr).map_err(io)?;
    let mut msg_b = vec![0u8; hdr[0] as usize];
    s.read_exact(&mut msg_b).map_err(io)?;
    let k = sp.finish(&msg_b).map_err(|_| "pairing failed".to_string())?;

    let confirm = pair_confirm_secret(&k);
    let (mut sealer, mut opener) = pair(&confirm, &[0u8; SALT], false);
    sealer.write(&mut s, &Msg::new(HELLO, 0, 0).bytes()).map_err(io)?;
    let mut rr = RecReader::new();
    let ok = rr.read_one(&mut s, 5).ok().and_then(|ct| opener.open(&ct)).and_then(|r| Msg::parse(&r));
    if !matches!(ok, Some(m) if m.t == HELLO) {
        return Err("wrong PIN".into());
    }
    let (id, _, name) = rr
        .read_one(&mut s, 5)
        .ok()
        .and_then(|ct| opener.open(&ct))
        .and_then(|r| parse_info(&r))
        .ok_or("the other computer did not answer")?;
    let c = config::get();
    if id == c.id {
        return Err("that is this computer".into());
    }
    // A known device keeps its place; a new one goes to the first free side. There, this
    // computer stands on the opposite side.
    let side = match c.peer(id) {
        Some(p) => p.side,
        None => c.free_side(),
    };
    sealer.write(&mut s, &info(side.map(Side::opposite))).map_err(io)?;
    add_peer(id, name.clone(), addr.ip().to_string(), long_term_key(&k), side);
    crate::ui::log(format!("Paired with {name} ({})", addr.ip()));
    Ok(name)
}

// ---------- discovery ----------

#[derive(Clone)]
pub struct Found {
    pub ip: String,
    pub id: u64,
    pub name: String,
}

fn discovery_responder(port: u16, gen: u32) {
    let Ok(sock) = std::net::UdpSocket::bind(("0.0.0.0", port)) else { return };
    let _ = sock.set_read_timeout(Some(Duration::from_millis(500)));
    let mut buf = [0u8; 64];
    while RUN.load(Relaxed) == gen {
        if let Ok((n, from)) = sock.recv_from(&mut buf) {
            if &buf[..n] == DISCOVER_ASK {
                let mut reply = DISCOVER_REPLY.to_vec();
                reply.extend_from_slice(format!("{:016x} {}", config::get().id, my_name()).as_bytes());
                let _ = sock.send_to(&reply, from);
            }
        }
    }
}

pub fn local_ipv4() -> Option<std::net::Ipv4Addr> {
    // Connecting a UDP socket sends nothing; it only picks the outgoing interface.
    let s = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect("8.8.8.8:80").ok()?;
    match s.local_addr().ok()?.ip() {
        std::net::IpAddr::V4(v4) => Some(v4),
        _ => None,
    }
}

/// Finds other computers running KVM Link on the local network.
pub fn discover(port: u16) -> Vec<Found> {
    let mut found: Vec<Found> = Vec::new();
    let Ok(sock) = std::net::UdpSocket::bind("0.0.0.0:0") else { return found };
    let _ = sock.set_broadcast(true);
    let _ = sock.set_read_timeout(Some(Duration::from_millis(200)));
    let mut targets = vec![format!("255.255.255.255:{port}"), format!("127.0.0.1:{port}")];
    if let Some(ip) = local_ipv4() {
        let o = ip.octets();
        targets.push(format!("{}.{}.{}.255:{port}", o[0], o[1], o[2]));
    }
    // Prefer our own subnet, then private LAN ranges; VPN adapters (e.g. Radmin 26.x) go last.
    let own = local_ipv4().map(|i| i.octets());
    let rank = |ip: &str| -> u8 {
        let Ok(a) = ip.parse::<std::net::Ipv4Addr>() else { return 4 };
        let o = a.octets();
        if own.is_some_and(|m| m[..3] == o[..3]) {
            0
        } else if a.is_private() {
            1
        } else if a.is_loopback() {
            3
        } else {
            2
        }
    };
    let me = config::get().id;
    let t0 = Instant::now();
    let mut buf = [0u8; 160];
    while t0.elapsed() < Duration::from_millis(1500) {
        for t in &targets {
            let _ = sock.send_to(DISCOVER_ASK, t.as_str());
        }
        while let Ok((n, from)) = sock.recv_from(&mut buf) {
            let Some(rest) = buf[..n].strip_prefix(DISCOVER_REPLY) else { continue };
            let rest = String::from_utf8_lossy(rest);
            let (id, name) = rest.split_once(' ').unwrap_or((&rest, ""));
            let Ok(id) = u64::from_str_radix(id, 16) else { continue };
            if id == me {
                continue;
            }
            // A computer with several adapters answers from each: keep its best address.
            let ip = from.ip().to_string();
            match found.iter_mut().find(|f| f.id == id) {
                Some(f) if rank(&ip) < rank(&f.ip) => f.ip = ip,
                Some(_) => {}
                None => found.push(Found { ip, id, name: name.to_string() }),
            }
        }
    }
    found.sort_by_key(|f| rank(&f.ip));
    found
}

// ---------- start / stop ----------

pub fn start() -> Result<(), String> {
    if RUNNING.load(Relaxed) {
        return Ok(());
    }
    let port = config::get().port;
    refresh();
    REMOTE_PEER.store(0, Relaxed);
    CONTROLLER.store(0, Relaxed);
    MODS.store(0, Relaxed);
    // The previous listener may still be closing after a quick stop/start.
    let mut listener = Err(std::io::Error::other("busy"));
    for _ in 0..10 {
        listener = TcpListener::bind(("0.0.0.0", port));
        if listener.is_ok() {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let listener = listener.map_err(|e| format!("Port {port} is busy: {e}"))?;
    listener.set_nonblocking(true).map_err(|e| e.to_string())?;
    let gen = RUN.fetch_add(1, Relaxed) + 1;
    std::thread::spawn(move || {
        while RUN.load(Relaxed) == gen {
            match listener.accept() {
                Ok((s, _)) => {
                    let _ = s.set_nonblocking(false);
                    std::thread::spawn(move || accept(s, gen));
                }
                Err(_) => std::thread::sleep(Duration::from_millis(20)),
            }
        }
    });
    std::thread::spawn(move || discovery_responder(port, gen));
    std::thread::spawn(move || supervisor(gen));
    start_hooks();
    RUNNING.store(true, Relaxed);
    crate::ui::notify();
    Ok(())
}

pub fn stop() {
    if !RUNNING.swap(false, Relaxed) {
        return;
    }
    RUN.fetch_add(1, Relaxed);
    close_pairing();
    leave_remote(None);
    release_inbound();
    stop_hooks();
    LINKS.lock().unwrap().clear();
    crate::ui::notify();
}
