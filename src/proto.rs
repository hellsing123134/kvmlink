//! Wire protocol: fixed-size encrypted frames (9 bytes plaintext + 16 byte tag).

use chacha20poly1305::{
    aead::{AeadInPlace, KeyInit},
    ChaCha20Poly1305, Key, Nonce, Tag,
};
use sha2::{Digest, Sha256};
use std::io::{self, Read};

pub const MOVE: u8 = 1; // a=dx b=dy
pub const KEY: u8 = 2; // a=scancode b=bit0 up, bit1 extended
pub const BUTTON: u8 = 3; // a=0 L,1 R,2 M,3 X1,4 X2  b=1 down
pub const WHEEL: u8 = 4; // a=delta b=1 horizontal
pub const ENTER: u8 = 5; // a=y in 1/1000 of screen, b=0 laptop is right of PC, 1 left
pub const LEAVE: u8 = 6; // a=y in 1/1000 of screen
pub const HELLO: u8 = 7;
pub const RELEASE: u8 = 8;
pub const PING: u8 = 9;

pub const FRAME: usize = 25;
pub const SALT: usize = 16;

/// First byte a client sends on the main port.
pub const MODE_CONNECT: u8 = b'C';
pub const MODE_PAIR: u8 = b'P';

/// UDP discovery on the same port number as the main TCP port.
pub const DISCOVER_ASK: &[u8] = b"KVMLINK?1";
pub const DISCOVER_REPLY: &[u8] = b"KVMLINK!1 ";

pub const SPAKE_ID_CLIENT: &[u8] = b"kvmlink-client";
pub const SPAKE_ID_SERVER: &[u8] = b"kvmlink-server";

/// 256-bit pairing key (hex) shared by the paired PC and laptop; empty = not paired.
static PAIR_KEY: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());

pub fn key() -> String {
    PAIR_KEY.lock().unwrap().clone()
}

pub fn set_key(k: &str) {
    *PAIR_KEY.lock().unwrap() = k.to_string();
}

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Key used only to confirm that both sides typed the same PIN.
pub fn pair_confirm_secret(spake_key: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(b"kvmlink-pair-confirm");
    h.update(spake_key);
    hex(&h.finalize())
}

/// The long-term key both sides store after a successful pairing.
pub fn long_term_key(spake_key: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(b"kvmlink-long-term-key");
    h.update(spake_key);
    hex(&h.finalize())
}

#[derive(Clone, Copy)]
pub struct Msg {
    pub t: u8,
    pub a: i32,
    pub b: i32,
}

impl Msg {
    pub const fn new(t: u8, a: i32, b: i32) -> Self {
        Msg { t, a, b }
    }
}

fn cipher(secret: &str, salt: &[u8; SALT]) -> ChaCha20Poly1305 {
    let mut h = Sha256::new();
    h.update(b"kvmlink-v1");
    h.update(secret.as_bytes());
    h.update(salt);
    ChaCha20Poly1305::new(Key::from_slice(&h.finalize()))
}

fn nonce(dir: u8, ctr: u64) -> [u8; 12] {
    let mut n = [0u8; 12];
    n[0] = dir;
    n[4..].copy_from_slice(&ctr.to_le_bytes());
    n
}

pub struct Sealer {
    c: ChaCha20Poly1305,
    dir: u8,
    ctr: u64,
}

pub struct Opener {
    c: ChaCha20Poly1305,
    dir: u8,
    ctr: u64,
}

/// Direction 0 = server -> client, 1 = client -> server.
pub fn pair(secret: &str, salt: &[u8; SALT], is_server: bool) -> (Sealer, Opener) {
    let (s, o) = if is_server { (0, 1) } else { (1, 0) };
    (
        Sealer { c: cipher(secret, salt), dir: s, ctr: 0 },
        Opener { c: cipher(secret, salt), dir: o, ctr: 0 },
    )
}

impl Sealer {
    /// Variable-length record (clipboard channel): returns ciphertext || tag.
    pub fn seal_vec(&mut self, plain: &[u8]) -> Vec<u8> {
        let mut buf = plain.to_vec();
        let n = nonce(self.dir, self.ctr);
        self.ctr += 1;
        self.c.encrypt_in_place(Nonce::from_slice(&n), b"", &mut buf).expect("encrypt");
        buf
    }

    pub fn seal(&mut self, m: Msg) -> [u8; FRAME] {
        let mut buf = [0u8; 9];
        buf[0] = m.t;
        buf[1..5].copy_from_slice(&m.a.to_le_bytes());
        buf[5..9].copy_from_slice(&m.b.to_le_bytes());
        let n = nonce(self.dir, self.ctr);
        self.ctr += 1;
        let tag = self
            .c
            .encrypt_in_place_detached(Nonce::from_slice(&n), b"", &mut buf)
            .expect("encrypt");
        let mut out = [0u8; FRAME];
        out[..9].copy_from_slice(&buf);
        out[9..].copy_from_slice(&tag);
        out
    }
}

impl Opener {
    pub fn open_vec(&mut self, ct: &[u8]) -> Option<Vec<u8>> {
        let mut buf = ct.to_vec();
        let n = nonce(self.dir, self.ctr);
        self.c.decrypt_in_place(Nonce::from_slice(&n), b"", &mut buf).ok()?;
        self.ctr += 1;
        Some(buf)
    }

    pub fn open(&mut self, f: &[u8; FRAME]) -> Option<Msg> {
        let mut buf = [0u8; 9];
        buf.copy_from_slice(&f[..9]);
        let n = nonce(self.dir, self.ctr);
        self.c
            .decrypt_in_place_detached(Nonce::from_slice(&n), b"", &mut buf, Tag::from_slice(&f[9..]))
            .ok()?;
        self.ctr += 1;
        Some(Msg {
            t: buf[0],
            a: i32::from_le_bytes(buf[1..5].try_into().unwrap()),
            b: i32::from_le_bytes(buf[5..9].try_into().unwrap()),
        })
    }
}

/// Accumulates bytes so a read timeout in the middle of a frame loses nothing.
pub struct FrameReader {
    buf: [u8; FRAME],
    filled: usize,
}

impl FrameReader {
    pub fn new() -> Self {
        FrameReader { buf: [0; FRAME], filled: 0 }
    }

    /// Ok(None) on read timeout, Err on EOF / socket error.
    pub fn poll(&mut self, s: &mut impl Read) -> io::Result<Option<[u8; FRAME]>> {
        while self.filled < FRAME {
            match s.read(&mut self.buf[self.filled..]) {
                Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
                Ok(n) => self.filled += n,
                Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => {
                    return Ok(None)
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        self.filled = 0;
        Ok(Some(self.buf))
    }
}
