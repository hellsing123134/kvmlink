//! Settings file. Lives in ProgramData so the SYSTEM-mode copy reads the same file as the
//! user's copy; %APPDATA% is the fallback when ProgramData is not writable.

use crate::setup::{is_system, NO_WINDOW};
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Clone)]
pub struct Cfg {
    pub server: bool,
    pub port: u16,
    pub side_left: bool,
    pub host: String,
    pub block: bool,
    pub clip: bool,
    pub service: bool,
    /// 64 hex chars after pairing, empty before.
    pub key: String,
}

impl Default for Cfg {
    fn default() -> Self {
        Cfg {
            server: true,
            port: 24800,
            side_left: false,
            host: String::new(),
            block: true,
            clip: true,
            service: false,
            key: String::new(),
        }
    }
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

pub fn load() -> Cfg {
    let mut c = Cfg::default();
    let newest = paths()
        .into_iter()
        .filter_map(|p| std::fs::metadata(&p).and_then(|m| m.modified()).ok().map(|t| (t, p)))
        .max_by_key(|(t, _)| *t)
        .map(|(_, p)| p);
    let Some(text) = newest.and_then(|p| std::fs::read_to_string(p).ok()) else { return c };
    for line in text.lines() {
        let Some((k, v)) = line.split_once('=') else { continue };
        match k {
            "mode" => c.server = v != "client",
            "port" => c.port = v.parse().unwrap_or(c.port),
            "side" => c.side_left = v == "left",
            "host" => c.host = v.into(),
            "block" => c.block = v == "1",
            "clip" => c.clip = v == "1",
            "service" => c.service = v == "1",
            "key" if valid_key(v) => c.key = v.into(),
            _ => {}
        }
    }
    c
}

/// The pairing key is a secret: only SYSTEM, Administrators and the current user may read it.
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

pub fn save(c: &Cfg) {
    let text = format!(
        "mode={}\nport={}\nside={}\nhost={}\nblock={}\nclip={}\nservice={}\nkey={}\n",
        if c.server { "server" } else { "client" },
        c.port,
        if c.side_left { "left" } else { "right" },
        c.host,
        c.block as u8,
        c.clip as u8,
        c.service as u8,
        c.key
    );
    for p in paths() {
        let dir = p.parent().unwrap();
        if std::fs::create_dir_all(dir).is_err() {
            continue;
        }
        restrict(dir);
        if std::fs::write(&p, &text).is_ok() {
            break;
        }
    }
}
