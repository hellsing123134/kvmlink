//! Settings file. Lives in ProgramData so the SYSTEM-mode copy reads the same file as the
//! user's copy; %APPDATA% is the fallback when ProgramData is not writable.
//!
//! The live copy is kept in memory behind a mutex: the window, the network threads and the
//! input hooks all read it, and every change is written to disk right away.

use crate::setup::{is_system, NO_WINDOW};
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

/// Where a neighbour stands relative to this computer's screen.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Side {
    Left,
    Right,
    Top,
    Bottom,
}

impl Side {
    pub const ALL: [Side; 4] = [Side::Right, Side::Left, Side::Top, Side::Bottom];

    pub fn opposite(self) -> Side {
        match self {
            Side::Left => Side::Right,
            Side::Right => Side::Left,
            Side::Top => Side::Bottom,
            Side::Bottom => Side::Top,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Side::Left => "left",
            Side::Right => "right",
            Side::Top => "top",
            Side::Bottom => "bottom",
        }
    }

    /// Wire / file code; 4 means "not placed".
    pub fn code(s: Option<Side>) -> i32 {
        match s {
            Some(Side::Left) => 0,
            Some(Side::Right) => 1,
            Some(Side::Top) => 2,
            Some(Side::Bottom) => 3,
            None => 4,
        }
    }

    pub fn from_code(c: i32) -> Option<Side> {
        match c {
            0 => Some(Side::Left),
            1 => Some(Side::Right),
            2 => Some(Side::Top),
            3 => Some(Side::Bottom),
            _ => None,
        }
    }
}

/// A paired device.
#[derive(Clone)]
pub struct Peer {
    pub id: u64,
    pub name: String,
    /// Last known address; refreshed by LAN discovery when it changes.
    pub host: String,
    /// 64 hex chars, shared only with this device.
    pub key: String,
    pub side: Option<Side>,
    /// When `side` was last changed (seconds since 2024-01-01); the newer layout wins on sync.
    pub stamp: i32,
}

#[derive(Clone)]
pub struct Cfg {
    /// Random, generated once; tells devices apart even when their IP changes.
    pub id: u64,
    pub port: u16,
    pub block: bool,
    pub clip: bool,
    pub service: bool,
    pub peers: Vec<Peer>,
    /// The file had a pairing from version 0.2, which cannot be carried over.
    pub legacy: bool,
}

impl Default for Cfg {
    fn default() -> Self {
        Cfg { id: 0, port: 24800, block: false, clip: true, service: false, peers: Vec::new(), legacy: false }
    }
}

impl Cfg {
    pub fn peer(&self, id: u64) -> Option<&Peer> {
        self.peers.iter().find(|p| p.id == id)
    }

    pub fn peer_mut(&mut self, id: u64) -> Option<&mut Peer> {
        self.peers.iter_mut().find(|p| p.id == id)
    }

    pub fn at(&self, side: Side) -> Option<&Peer> {
        self.peers.iter().find(|p| p.side == Some(side))
    }

    pub fn free_side(&self) -> Option<Side> {
        Side::ALL.into_iter().find(|s| self.at(*s).is_none())
    }

    /// Puts `id` at `side`; whoever stood there becomes unplaced. Returns the ids whose side
    /// changed, so the caller can tell them.
    pub fn place(&mut self, id: u64, side: Option<Side>, stamp: i32) -> Vec<u64> {
        let mut changed = vec![id];
        if let Some(s) = side {
            for p in self.peers.iter_mut().filter(|p| p.id != id && p.side == Some(s)) {
                p.side = None;
                p.stamp = stamp;
                changed.push(p.id);
            }
        }
        if let Some(p) = self.peer_mut(id) {
            p.side = side;
            p.stamp = stamp;
        }
        changed
    }
}

pub fn now_stamp() -> i32 {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    secs.saturating_sub(1_704_067_200) as i32
}

static CFG: Mutex<Option<Cfg>> = Mutex::new(None);

/// A snapshot of the current settings.
pub fn get() -> Cfg {
    CFG.lock().unwrap().clone().unwrap_or_default()
}

/// Changes the settings and saves them. Returns what `f` returned.
pub fn update<R>(f: impl FnOnce(&mut Cfg) -> R) -> R {
    let mut g = CFG.lock().unwrap();
    let c = g.get_or_insert_with(Cfg::default);
    let r = f(c);
    save(c);
    r
}

fn paths() -> [PathBuf; 2] {
    // For running a second, independent copy (testing): KVMLINK_CONFIG_DIR=<dir>.
    if let Ok(dir) = std::env::var("KVMLINK_CONFIG_DIR") {
        let p = Path::new(&dir).join("config.txt");
        return [p.clone(), p];
    }
    let pd = std::env::var("PROGRAMDATA").unwrap_or_else(|_| r"C:\ProgramData".into());
    let ad = std::env::var("APPDATA").unwrap_or_else(|_| ".".into());
    [
        Path::new(&pd).join("kvmlink").join("config.txt"),
        Path::new(&ad).join("kvmlink").join("config.txt"),
    ]
}

fn valid_key(k: &str) -> bool {
    k.len() == 64 && k.bytes().all(|b| b.is_ascii_hexdigit())
}

pub fn random_id() -> u64 {
    let mut b = [0u8; 8];
    loop {
        let _ = getrandom::getrandom(&mut b);
        let v = u64::from_le_bytes(b);
        if v != 0 {
            return v;
        }
    }
}

/// `peer=<id>|<side>|<stamp>|<host>|<key>|<name>`
fn parse_peer(v: &str) -> Option<Peer> {
    let mut f = v.splitn(6, '|');
    let id = u64::from_str_radix(f.next()?, 16).ok().filter(|i| *i != 0)?;
    let side = Side::from_code(f.next()?.parse().ok()?);
    let stamp = f.next()?.parse().ok()?;
    let host = f.next()?.to_string();
    let key = f.next()?.to_string();
    let name = f.next()?.to_string();
    valid_key(&key).then_some(Peer { id, name, host, key, side, stamp })
}

/// Reads the settings file into memory (once, at startup).
pub fn load() -> Cfg {
    let mut c = Cfg::default();
    let newest = paths()
        .into_iter()
        .filter_map(|p| std::fs::metadata(&p).and_then(|m| m.modified()).ok().map(|t| (t, p)))
        .max_by_key(|(t, _)| *t)
        .map(|(_, p)| p);
    if let Some(text) = newest.and_then(|p| std::fs::read_to_string(p).ok()) {
        for line in text.lines() {
            let Some((k, v)) = line.split_once('=') else { continue };
            match k {
                "id" => c.id = u64::from_str_radix(v, 16).unwrap_or(0),
                "port" => c.port = v.parse().unwrap_or(c.port),
                "block" => c.block = v == "1",
                "clip" => c.clip = v == "1",
                "service" => c.service = v == "1",
                "peer" => {
                    if let Some(p) = parse_peer(v) {
                        if c.peer(p.id).is_none() {
                            c.peers.push(p);
                        }
                    }
                }
                "key" if valid_key(v) => c.legacy = true,
                _ => {}
            }
        }
    }
    let fresh = c.id == 0;
    if fresh {
        c.id = random_id();
    }
    *CFG.lock().unwrap() = Some(c.clone());
    if fresh || c.legacy {
        update(|_| ()); // persist the id and drop the old single-pair key
    }
    c
}

/// The pairing keys are secrets: only SYSTEM, Administrators and the current user may read them.
fn restrict(dir: &Path) {
    if is_system() {
        return;
    }
    let user = format!(
        "{}\\{}",
        std::env::var("USERDOMAIN").unwrap_or_default(),
        std::env::var("USERNAME").unwrap_or_default()
    );
    let _ = Command::new("icacls")
        .arg(dir)
        .args(["/inheritance:r", "/grant:r", "*S-1-5-18:(OI)(CI)F", "*S-1-5-32-544:(OI)(CI)F"])
        .arg(format!("{user}:(OI)(CI)F"))
        .creation_flags(NO_WINDOW)
        .output();
}

fn save(c: &Cfg) {
    let mut text = format!(
        "id={:016x}\nport={}\nblock={}\nclip={}\nservice={}\n",
        c.id, c.port, c.block as u8, c.clip as u8, c.service as u8
    );
    for p in &c.peers {
        let name: String = p.name.chars().filter(|ch| *ch != '\n' && *ch != '\r').collect();
        text += &format!("peer={:016x}|{}|{}|{}|{}|{}\n", p.id, Side::code(p.side), p.stamp, p.host, p.key, name);
    }
    for p in paths() {
        let dir = p.parent().unwrap();
        if std::fs::create_dir_all(dir).is_err() {
            continue;
        }
        if !p.exists() {
            restrict(dir);
        }
        if std::fs::write(&p, &text).is_ok() {
            break;
        }
    }
}
