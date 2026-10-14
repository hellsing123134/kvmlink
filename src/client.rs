//! Runs on the machine that receives input (the laptop).

use crate::proto::*;
use crate::sys;
use std::io::Write;
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering::Relaxed};
use std::sync::Once;
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

static CONNECTED: AtomicBool = AtomicBool::new(false);
static BLOCK: AtomicBool = AtomicBool::new(false);
static RUN: AtomicU32 = AtomicU32::new(0);
static HOOK_ONCE: Once = Once::new();

/// Swallows the laptop's own (faulty) keyboard while connected. Injected keys pass.
unsafe extern "system" fn block_proc(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if code >= 0 && BLOCK.load(Relaxed) && CONNECTED.load(Relaxed) {
        let k = &*(lp as *const KBDLLHOOKSTRUCT);
        if k.flags & 0x10 == 0 {
            return 1;
        }
    }
    CallNextHookEx(0 as _, code, wp, lp)
}

struct Injector {
    active: bool,
    side_left: bool, // laptop is left of the PC => we enter at our right edge
    w: i32,
    h: i32,
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
        self.active = false;
    }

    /// Returns a message to send back to the PC, if any.
    fn apply(&mut self, m: Msg) -> Option<Msg> {
        match m.t {
            ENTER => {
                self.side_left = m.b == 1;
                self.active = true;
                let x = if self.side_left { self.w - 1 } else { 0 };
                sys::move_abs(x, (m.a.clamp(0, 1000) as i64 * self.h as i64 / 1000) as i32);
            }
            RELEASE => self.release_all(),
            _ if !self.active => {}
            MOVE => {
                let (cx, cy) = sys::cursor();
                let (nx, ny) = (cx + m.a, (cy + m.b).clamp(0, self.h - 1));
                let out = if self.side_left { nx > self.w - 1 && m.a > 0 } else { nx < 0 && m.a < 0 };
                if out {
                    self.release_all();
                    return Some(Msg::new(LEAVE, (cy.clamp(0, self.h - 1) as i64 * 1000 / self.h as i64) as i32, 0));
                }
                sys::move_abs(nx.clamp(0, self.w - 1), ny);
            }
            KEY => {
                let (scan, up, ext) = (m.a as u16, m.b & 1 != 0, m.b & 2 != 0);
                sys::key(scan, up, ext);
                self.keys.retain(|k| *k != (scan, ext));
                if !up {
                    self.keys.push((scan, ext));
                }
            }
            BUTTON => {
                let i = (m.a as usize).min(4);
                self.buttons[i] = m.b != 0;
                sys::button(i as i32, m.b != 0);
            }
            WHEEL => sys::wheel(m.a, m.b == 1),
            _ => {}
        }
        None
    }
}

fn resolve(host: &str, port: u16) -> std::io::Result<std::net::SocketAddr> {
    (host, port)
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| std::io::Error::other("неверный адрес"))
}

fn session(host: &str, port: u16, gen: u32, inj: &mut Injector) -> std::io::Result<()> {
    let addr = resolve(host, port)?;
    let mut s = TcpStream::connect_timeout(&addr, Duration::from_secs(2))?;
    s.set_nodelay(true)?;
    s.set_read_timeout(Some(Duration::from_millis(500)))?;
    s.write_all(&[MODE_CONNECT])?;

    let mut salt = [0u8; SALT];
    let mut got = 0;
    let t0 = Instant::now();
    while got < SALT {
        match std::io::Read::read(&mut s, &mut salt[got..]) {
            Ok(0) => return Err(std::io::ErrorKind::UnexpectedEof.into()),
            Ok(n) => got += n,
            Err(_) if t0.elapsed() < Duration::from_secs(3) => {}
            Err(e) => return Err(e),
        }
    }
    let (mut sealer, mut opener) = pair(&key(), &salt, false);
    s.write_all(&sealer.seal(Msg::new(HELLO, 0, 0)))?;
    let mut accepted = false;

    let mut fr = FrameReader::new();
    let mut last_rx = Instant::now();
    let mut last_ping = Instant::now();
    let res = loop {
        if RUN.load(Relaxed) != gen {
            break Ok(());
        }
        match fr.poll(&mut s) {
            Ok(Some(f)) => {
                last_rx = Instant::now();
                if !accepted {
                    accepted = true;
                    CONNECTED.store(true, Relaxed);
                    crate::ui::log(format!("Подключено к {}", addr.ip()));
                    crate::ui::status(crate::ui::Status::Connected, &format!("Подключено к ПК ({})", addr.ip()));
                }
                match opener.open(&f) {
                    Some(m) => {
                        if let Some(reply) = inj.apply(m) {
                            if let Err(e) = s.write_all(&sealer.seal(reply)) {
                                break Err(e);
                            }
                        }
                    }
                    None => break Err(std::io::Error::other("bad frame (key mismatch?)")),
                }
            }
            Ok(None) => {
                if last_rx.elapsed() > Duration::from_secs(5) {
                    break Err(std::io::ErrorKind::TimedOut.into());
                }
            }
            Err(e) if !accepted => {
                // The PC drops us right after HELLO when our key is not the one it has.
                break Err(std::io::Error::other(match e.kind() {
                    std::io::ErrorKind::UnexpectedEof | std::io::ErrorKind::ConnectionReset => {
                        "ПК отклонил подключение: ключ не совпадает, сопрягите устройства заново"
                    }
                    _ => "соединение прервано",
                }));
            }
            Err(e) => break Err(e),
        }
        if last_ping.elapsed() > Duration::from_secs(1) {
            last_ping = Instant::now();
            if let Err(e) = s.write_all(&sealer.seal(Msg::new(PING, 0, 0))) {
                break Err(e);
            }
        }
    };
    CONNECTED.store(false, Relaxed);
    inj.release_all();
    if accepted {
        crate::ui::log("Связь с ПК потеряна");
    }
    res
}

/// One-time pairing with the PIN shown on the PC. Returns the long-term key (hex).
pub fn pair_with(host: &str, port: u16, pin: &str) -> Result<String, String> {
    use spake2::{Ed25519Group, Identity, Password, Spake2};
    use std::io::Read;
    let addr = resolve(host, port).map_err(|e| e.to_string())?;
    let mut s = TcpStream::connect_timeout(&addr, Duration::from_secs(3)).map_err(|e| format!("ПК недоступен: {e}"))?;
    let _ = s.set_read_timeout(Some(Duration::from_secs(5)));
    let (sp, msg_a) = Spake2::<Ed25519Group>::start_a(
        &Password::new(pin.as_bytes()),
        &Identity::new(SPAKE_ID_CLIENT),
        &Identity::new(SPAKE_ID_SERVER),
    );
    let mut out = vec![MODE_PAIR, msg_a.len() as u8];
    out.extend_from_slice(&msg_a);
    let io = |e: std::io::Error| format!("ошибка связи: {e}");
    s.write_all(&out).map_err(io)?;
    let mut hdr = [0u8; 1];
    s.read_exact(&mut hdr).map_err(io)?;
    if hdr[0] != 1 {
        return Err("на ПК не открыто сопряжение (нажмите там «Сопрячь устройство»)".into());
    }
    s.read_exact(&mut hdr).map_err(io)?;
    let mut msg_b = vec![0u8; hdr[0] as usize];
    s.read_exact(&mut msg_b).map_err(io)?;
    let k = sp.finish(&msg_b).map_err(|_| "ошибка сопряжения".to_string())?;

    let confirm = pair_confirm_secret(&k);
    let (mut sealer, mut opener) = pair(&confirm, &[0u8; SALT], false);
    s.write_all(&sealer.seal(Msg::new(HELLO, 0, 0))).map_err(io)?;
    let mut f = [0u8; FRAME];
    let ok = s.read_exact(&mut f).is_ok() && matches!(opener.open(&f), Some(m) if m.t == HELLO);
    if !ok {
        return Err("неверный PIN".into());
    }
    Ok(long_term_key(&k))
}

/// Finds PCs running KVM Link on the local network: (ip, computer name).
pub fn discover(port: u16) -> Vec<(String, String)> {
    let mut found: Vec<(String, String)> = Vec::new();
    let Ok(sock) = std::net::UdpSocket::bind("0.0.0.0:0") else { return found };
    let _ = sock.set_broadcast(true);
    let _ = sock.set_read_timeout(Some(Duration::from_millis(200)));
    let mut targets = vec![format!("255.255.255.255:{port}")];
    if let Some(ip) = crate::ui::local_ipv4() {
        let o = ip.octets();
        targets.push(format!("{}.{}.{}.255:{port}", o[0], o[1], o[2]));
    }
    let t0 = Instant::now();
    let mut buf = [0u8; 128];
    while t0.elapsed() < Duration::from_millis(1500) {
        for t in &targets {
            let _ = sock.send_to(DISCOVER_ASK, t.as_str());
        }
        while let Ok((n, from)) = sock.recv_from(&mut buf) {
            if let Some(rest) = buf[..n].strip_prefix(DISCOVER_REPLY) {
                let ip = from.ip().to_string();
                if !found.iter().any(|(i, _)| *i == ip) {
                    found.push((ip, String::from_utf8_lossy(rest).into_owned()));
                }
            }
        }
    }
    // Prefer our own subnet, then private LAN ranges; VPN adapters (e.g. Radmin 26.x) go last.
    let own = crate::ui::local_ipv4().map(|i| i.octets());
    let rank = |ip: &str| -> u8 {
        let Ok(a) = ip.parse::<std::net::Ipv4Addr>() else { return 3 };
        let o = a.octets();
        if own.is_some_and(|m| m[..3] == o[..3]) {
            0
        } else if a.is_private() {
            1
        } else {
            2
        }
    };
    found.sort_by_key(|(ip, _)| rank(ip));
    found
}

pub fn start(host: String, port: u16, block_kb: bool) {
    BLOCK.store(block_kb, Relaxed);
    if block_kb {
        HOOK_ONCE.call_once(|| {
            std::thread::spawn(|| unsafe {
                SetWindowsHookExW(WH_KEYBOARD_LL, Some(block_proc), 0 as _, 0);
                sys::message_loop();
            });
        });
    }
    let gen = RUN.fetch_add(1, Relaxed) + 1;
    std::thread::spawn(move || {
        sys::boost_thread();
        let (w, h) = sys::screen();
        let mut inj = Injector { active: false, side_left: false, w, h, keys: vec![], buttons: [false; 5] };
        let mut reported = false;
        while RUN.load(Relaxed) == gen {
            match session(&host, port, gen, &mut inj) {
                Ok(()) => reported = false,
                Err(e) => {
                    if !reported {
                        crate::ui::log(format!("Нет связи с ПК: {e}. Пробую снова..."));
                        crate::ui::status(crate::ui::Status::Waiting, "Ищу ПК...");
                        reported = true;
                    }
                }
            }
            for _ in 0..10 {
                if RUN.load(Relaxed) != gen {
                    break;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    });
}

pub fn stop() {
    RUN.fetch_add(1, Relaxed);
    BLOCK.store(false, Relaxed);
}
