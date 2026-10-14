<p align="center">
  <img src="assets/icon-256.png" width="96" alt="KVM Link">
</p>

<h1 align="center">KVM Link</h1>

<p align="center">
  One keyboard and mouse for two Windows computers over your local network.<br>
  Move the cursor past the edge of your monitor and keep working on the laptop next to it.
</p>

<p align="center"><img src="docs/screenshot.png" width="420" alt="KVM Link window"></p>

> The window is currently in Russian. An English UI with a drag-and-drop layout editor is in progress.

## Features

- **Edge switching.** Push the cursor past the screen edge and control moves to the other computer. Come back through the opposite edge, or press `Ctrl+Alt+F12` to switch manually.
- **Low latency.** 25-byte frames, `TCP_NODELAY`, coalesced mouse motion and high-priority threads. On a cable or a good Wi-Fi link the delay is a few milliseconds.
- **PIN pairing.** One computer shows a 6-digit PIN, you type it on the other, and from then on they remember each other. No passwords.
- **Encryption.** All traffic is encrypted with ChaCha20-Poly1305.
- **Shared clipboard.** Text and images up to 16 MB, both ways. The clipboard uses its own connection, so big copies never slow down the mouse.
- **LAN discovery.** Find the other computer with one click instead of typing an IP address.
- **Block a faulty built-in keyboard.** Useful when laptop keys get stuck and type on their own.
- **Service mode.** Keeps working in administrator windows and UAC prompts.
- **Tray icon and autostart.** A single portable `.exe`, no installer and no dependencies.

## Getting started

1. Download `kvmlink.exe` from [Releases](../../releases) and put it on **both** computers.
2. **On the computer with the keyboard:** choose that role, pick which side the laptop is on and click the pairing button. Windows asks once to open a firewall port, then a PIN appears.
3. **On the laptop:** choose the other role, click **Find**, type the PIN and pair.

The laptop connects by itself. After that just start the app, or enable autostart.

Closing the window hides it to the tray. To quit completely, use **Exit** in the tray icon's menu.

### Service mode

Windows does not let a normal program control windows that run as administrator or UAC prompts, so the cursor gets "stuck" on them. With **service mode** enabled (on both computers, one UAC confirmation each) the app starts at sign-in as SYSTEM, the same approach the Mouse Without Borders service in PowerToys uses, and those windows work too.

Keep in mind that in this mode anyone who holds the pairing key can also control protected Windows prompts.

## Security

| What | How |
|---|---|
| Pairing | [SPAKE2](https://datatracker.ietf.org/doc/rfc9382/) over a 6-digit PIN. Recorded traffic does not help guessing the PIN: each PIN allows 3 attempts and the pairing window stays open for 2 minutes. |
| Key | Pairing produces a 256-bit key stored in `C:\ProgramData\kvmlink\config.txt`. The folder is readable only by SYSTEM, Administrators and your user. |
| Session | Every connection uses a fresh random salt and its own session key. Frames are encrypted with ChaCha20-Poly1305 and a counter, so tampering, replays and reordering are detected. |
| Network | Firewall rules allow the local subnet only. |

Not provided: forward secrecy. If the key in `config.txt` leaks, previously recorded traffic can be decrypted. To change the key, simply pair the devices again.

## Limitations

- Windows 10 and 11 only. One pair of devices.
- Only the primary monitor is used.
- The clipboard carries text and images, not files.
- The cursor is sent as absolute coordinates, so games that use raw input may feel unusual.

## Ports

| Port | Protocol | Purpose |
|---|---|---|
| 24800 | TCP | keyboard and mouse, pairing |
| 24801 | TCP | clipboard |
| 24800 | UDP | LAN discovery |

The port can be changed in the app. It must be the same on both computers.

## Building

You need [Rust](https://rustup.rs) (stable) with either the MSVC or the GNU toolchain. For GNU you also need MinGW-w64 with `windres` on `PATH`.

```bash
cargo build --release
```

The result is `target/release/kvmlink.exe`. To regenerate the icon run `python assets/make_icon.py` (needs Pillow).

To debug on a single machine you can run two independent copies with different environment variables: `KVMLINK_INSTANCE` (instance name) and `KVMLINK_CONFIG_DIR` (settings folder). A test copy does not touch the firewall.

### Code layout

| File | Contents |
|---|---|
| `src/server.rs` | Captures local input (low-level hooks), pairing, discovery replies |
| `src/client.rs` | Injects input with `SendInput`, pairing, discovery |
| `src/proto.rs` | Frame format, encryption, key derivation |
| `src/clip.rs` | Clipboard sync |
| `src/ui.rs` | Window and tray icon (plain Win32) |
| `src/setup.rs` | Firewall, autostart, service-mode task |
| `src/svc.rs` | Starts a SYSTEM copy in the user's session |

## Author

Created by [hellsing](https://github.com/hellsing123134).

## License

[MIT](LICENSE)
