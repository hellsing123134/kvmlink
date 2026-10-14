//! Wire protocol: length-prefixed encrypted records (`u32 len || ciphertext || 16-byte tag`).
//! Input events are 9 bytes of plaintext; clipboard data travels in chunks on the same link.

use chacha20poly1305::{
    aead::{AeadInPlace, KeyInit},
    ChaCha20Poly1305, Key, Nonce,
};
use sha2::{Digest, Sha256};
use std::io::{self, Read, Write};

pub const MOVE: u8 = 1; // a=dx b=dy
pub const KEY: u8 = 2; // a=scancode b=bit0 up, bit1 extended
pub const BUTTON: u8 = 3; // a=0 L,1 R,2 M,3 X1,4 X2  b=1 down
pub const WHEEL: u8 = 4; // a=delta b=1 horizontal
pub const ENTER: u8 = 5; // a=position along the edge in 1/1000, b=edge of the receiver's screen (Side code)
pub const LEAVE: u8 = 6; // a=position along the edge in 1/1000, or -1 when the receiver's user took over
pub const HELLO: u8 = 7;
pub const RELEASE: u8 = 8;
pub const PING: u8 = 9;
pub const LAYOUT: u8 = 10; // a=where the sender stands relative to the receiver (Side code), b=stamp
pub const FORGET: u8 = 11; // the sender removed this pairing
pub const CLIP_PART: u8 = 12; // clipboard chunk, more follow
pub const CLIP_END: u8 = 13; // last clipboard chunk

pub const SALT: usize = 16;
/// Largest clipboard payload (text or image).
pub const CLIP_MAX: usize = 16 * 1024 * 1024;
const REC_MAX: usize = 256 * 1024;

/// First byte a dialing device sends on the main port.
pub const MODE_CONNECT: u8 = b'c';
pub const MODE_PAIR: u8 = b'p';

/// UDP discovery on the same port number as the main TCP port.
pub const DISCOVER_ASK: &[u8] = b"KVMLINK?2";
pub const DISCOVER_REPLY: &[u8] = b"KVMLINK!2 ";

pub const SPAKE_ID_A: &[u8] = b"kvmlink-pair-a";
pub const SPAKE_ID_B: &[u8] = b"kvmlink-pair-b";

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

    pub fn bytes(&self) -> [u8; 9] {
        let mut buf = [0u8; 9];
        buf[0] = self.t;
        buf[1..5].copy_from_slice(&self.a.to_le_bytes());
        buf[5..9].copy_from_slice(&self.b.to_le_bytes());
        buf
    }

    pub fn parse(b: &[u8]) -> Option<Msg> {
        (b.len() == 9).then(|| Msg {
            t: b[0],
            a: i32::from_le_bytes(b[1..5].try_into().unwrap()),
            b: i32::from_le_bytes(b[5..9].try_into().unwrap()),
        })
    }
}

fn cipher(secret: &str, salt: &[u8; SALT]) -> ChaCha20Poly1305 {
    let mut h = Sha256::new();
    h.update(b"kvmlink-v2");
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

/// Direction 0 = listener -> dialer, 1 = dialer -> listener.
pub fn pair(secret: &str, salt: &[u8; SALT], listener: bool) -> (Sealer, Opener) {
    let (s, o) = if listener { (0, 1) } else { (1, 0) };
    (
        Sealer { c: cipher(secret, salt), dir: s, ctr: 0 },
        Opener { c: cipher(secret, salt), dir: o, ctr: 0 },
    )
}

impl Sealer {
    /// Encrypts one record and appends it, length prefix included, to `out`.
    pub fn seal_into(&mut self, plain: &[u8], out: &mut Vec<u8>) {
        let mut buf = plain.to_vec();
        let n = nonce(self.dir, self.ctr);
        self.ctr += 1;
        self.c.encrypt_in_place(Nonce::from_slice(&n), b"", &mut buf).expect("encrypt");
        out.extend_from_slice(&(buf.len() as u32).to_le_bytes());
        out.extend_from_slice(&buf);
    }

    pub fn seal_msg(&mut self, m: Msg, out: &mut Vec<u8>) {
        self.seal_into(&m.bytes(), out);
    }

    pub fn write(&mut self, s: &mut impl Write, plain: &[u8]) -> io::Result<()> {
        let mut out = Vec::with_capacity(plain.len() + 20);
        self.seal_into(plain, &mut out);
        s.write_all(&out)
    }
}

impl Opener {
    pub fn open(&mut self, ct: &[u8]) -> Option<Vec<u8>> {
        let mut buf = ct.to_vec();
        let n = nonce(self.dir, self.ctr);
        self.c.decrypt_in_place(Nonce::from_slice(&n), b"", &mut buf).ok()?;
        self.ctr += 1;
        Some(buf)
    }
}

/// Accumulates bytes so a read timeout in the middle of a record loses nothing.
pub struct RecReader {
    hdr: [u8; 4],
    hfill: usize,
    buf: Vec<u8>,
    bfill: usize,
    len: Option<usize>,
}

impl RecReader {
    pub fn new() -> Self {
        RecReader { hdr: [0; 4], hfill: 0, buf: Vec::new(), bfill: 0, len: None }
    }

    /// Ok(None) on read timeout, Err on EOF / socket error / bad length.
    pub fn poll(&mut self, s: &mut impl Read) -> io::Result<Option<Vec<u8>>> {
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
                if !(16..=REC_MAX).contains(&n) {
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

    /// Blocking read of one record, giving up after `secs` seconds of silence.
    pub fn read_one(&mut self, s: &mut std::net::TcpStream, secs: u64) -> io::Result<Vec<u8>> {
        let t0 = std::time::Instant::now();
        loop {
            if let Some(r) = self.poll(s)? {
                return Ok(r);
            }
            if t0.elapsed().as_secs() >= secs {
                return Err(io::ErrorKind::TimedOut.into());
            }
        }
    }
}

/// Clipboard chunk size: small enough that mouse events squeezed in between stay fast.
pub const CLIP_CHUNK: usize = 60 * 1024;
