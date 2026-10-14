//! Native Win32 settings window and tray icon.

use crate::config::{self, Cfg};
use crate::setup;
use crate::{client, clip, proto, server, sys};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering::Relaxed};
use std::sync::Mutex;
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::HiDpi::GetDpiForSystem;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::EnableWindow;
use windows_sys::Win32::UI::Shell::{Shell_NotifyIconW, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

const ID_SERVER: i32 = 101;
const ID_CLIENT: i32 = 102;
const ID_START: i32 = 103;
const ID_PAIR_PC: i32 = 104;
const ID_PAIR_LAPTOP: i32 = 105;
const ID_FIND: i32 = 106;
const ID_AUTO: i32 = 107;
const ID_SERVICE: i32 = 108;
const ID_TRAY_OPEN: usize = 201;
const ID_TRAY_EXIT: usize = 202;
const TIMER_PIN: usize = 1;
// Edit/static control constants (they live in the Controls namespace of windows-sys).
const EM_SETSEL: u32 = 0x00B1;
const EM_REPLACESEL: u32 = 0x00C2;
const EM_LIMITTEXT: u32 = 0x00C5;
const SS_CENTER: u32 = 0x0001;
const SS_CENTERIMAGE: u32 = 0x0200;

const WM_LOG: u32 = WM_APP;
const WM_TRAY: u32 = WM_APP + 2;
const WM_EXIT: u32 = WM_APP + 3;
pub const WM_SHOWME: u32 = WM_APP + 4; // also sent by sys::show_existing
const WM_STATUS: u32 = WM_APP + 6;
const WM_PAIRED: u32 = WM_APP + 7;
const WM_FOUND: u32 = WM_APP + 8;
const WM_PAIR_DONE: u32 = WM_APP + 9;

#[derive(Clone, Copy, PartialEq)]
pub enum Status {
    Stopped,
    Waiting,
    Connected,
    Attention,
}

static LOG: Mutex<Vec<String>> = Mutex::new(Vec::new());
static STATUS: Mutex<(Status, String)> = Mutex::new((Status::Stopped, String::new()));
static NEW_KEY: Mutex<Option<String>> = Mutex::new(None);
static FOUND: Mutex<Vec<(String, String)>> = Mutex::new(Vec::new());
static MAIN: AtomicUsize = AtomicUsize::new(0);
static DPI: AtomicU32 = AtomicU32::new(96);
static RUNNING: AtomicBool = AtomicBool::new(false);
static TASKBAR_CREATED: AtomicU32 = AtomicU32::new(0);
static UI: Mutex<Option<Ui>> = Mutex::new(None);
static CFG: Mutex<Option<Cfg>> = Mutex::new(None);

#[derive(Clone, Copy)]
struct Ui {
    rb_server: usize,
    rb_client: usize,
    // PC
    lb_side: usize,
    cb_side: usize,
    lb_ip: usize,
    lb_pair_pc: usize,
    bt_pair_pc: usize,
    lb_pin: usize,
    // laptop
    lb_host: usize,
    ed_host: usize,
    bt_find: usize,
    lb_pin_in: usize,
    ed_pin: usize,
    bt_pair_laptop: usize,
    lb_pair_laptop: usize,
    ck_block: usize,
    // options
    ck_clip: usize,
    ck_auto: usize,
    ck_service: usize,
    ed_port: usize,
    // bottom
    lb_status: usize,
    bt_start: usize,
    log: usize,
}

// ---------- calls from worker threads ----------

fn post(msg: u32) {
    let h = MAIN.load(Relaxed);
    if h != 0 {
        unsafe {
            PostMessageW(h as HWND, msg, 0, 0);
        }
    }
}

/// Queue a line for the log box.
pub fn log(s: impl Into<String>) {
    LOG.lock().unwrap().push(s.into());
    post(WM_LOG);
}

/// Update the coloured status line.
pub fn status(st: Status, text: &str) {
    *STATUS.lock().unwrap() = (st, text.to_string());
    post(WM_STATUS);
}

/// Pairing finished on this side; the key is already active in `proto`.
pub fn paired(key: &str) {
    *NEW_KEY.lock().unwrap() = Some(key.to_string());
    post(WM_PAIRED);
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

// ---------- small Win32 helpers ----------

fn w(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

fn sc(x: i32) -> i32 {
    x * DPI.load(Relaxed) as i32 / 96
}

fn hw(h: usize) -> HWND {
    h as HWND
}

fn get_text(h: usize) -> String {
    unsafe {
        let n = GetWindowTextLengthW(hw(h));
        let mut buf = vec![0u16; n as usize + 1];
        let got = GetWindowTextW(hw(h), buf.as_mut_ptr(), n + 1);
        String::from_utf16_lossy(&buf[..got as usize])
    }
}

fn set_text(h: usize, t: &str) {
    unsafe {
        SetWindowTextW(hw(h), w(t).as_ptr());
    }
}

fn checked(h: usize) -> bool {
    unsafe { SendMessageW(hw(h), BM_GETCHECK, 0, 0) == 1 }
}

fn set_check(h: usize, on: bool) {
    unsafe {
        SendMessageW(hw(h), BM_SETCHECK, on as WPARAM, 0);
    }
}

fn show(h: usize, on: bool) {
    unsafe {
        ShowWindow(hw(h), if on { SW_SHOW } else { SW_HIDE });
    }
}

fn enable(h: usize, on: bool) {
    unsafe {
        EnableWindow(hw(h), on as i32);
    }
}

fn ui() -> Option<Ui> {
    *UI.lock().unwrap()
}

fn cfg() -> Cfg {
    CFG.lock().unwrap().clone().unwrap_or_default()
}

fn set_cfg(c: Cfg) {
    config::save(&c);
    *CFG.lock().unwrap() = Some(c);
}

fn app_icon(size: i32) -> HICON {
    unsafe {
        let h = LoadImageW(GetModuleHandleW(std::ptr::null()), 1 as _, IMAGE_ICON, size, size, 0);
        if h.is_null() {
            LoadIconW(0 as _, IDI_APPLICATION)
        } else {
            h as HICON
        }
    }
}

// ---------- tray ----------

fn tray_icon(h: HWND, op: u32) {
    unsafe {
        let mut nid: NOTIFYICONDATAW = std::mem::zeroed();
        nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        nid.hWnd = h;
        nid.uID = 1;
        nid.uFlags = 1 | 2 | 4; // NIF_MESSAGE | NIF_ICON | NIF_TIP
        nid.uCallbackMessage = WM_TRAY;
        nid.hIcon = app_icon(GetSystemMetrics(SM_CXSMICON));
        for (i, c) in "KVM Link".encode_utf16().enumerate() {
            nid.szTip[i] = c;
        }
        Shell_NotifyIconW(op, &nid);
    }
}

fn show_main(h: HWND) {
    unsafe {
        ShowWindow(h, SW_SHOW);
        ShowWindow(h, SW_RESTORE);
        SetForegroundWindow(h);
    }
}

fn tray_menu(h: HWND) {
    unsafe {
        let m = CreatePopupMenu();
        AppendMenuW(m, 0, ID_TRAY_OPEN, w("Открыть").as_ptr());
        AppendMenuW(m, 0, ID_TRAY_EXIT, w("Выход").as_ptr());
        let mut p = std::mem::zeroed();
        GetCursorPos(&mut p);
        SetForegroundWindow(h);
        TrackPopupMenu(m, TPM_RIGHTBUTTON, p.x, p.y, 0, h, std::ptr::null());
        PostMessageW(h, WM_NULL, 0, 0);
        DestroyMenu(m);
    }
}

// ---------- window logic ----------

fn is_pc(u: &Ui) -> bool {
    checked(u.rb_server)
}

fn refresh(u: &Ui) {
    let pc = is_pc(u);
    let running = RUNNING.load(Relaxed);
    let paired = !proto::key().is_empty();
    for h in [u.lb_side, u.cb_side, u.lb_ip, u.lb_pair_pc, u.bt_pair_pc] {
        show(h, pc);
    }
    show(u.lb_pin, pc && server::pairing_state().is_some());
    for h in [u.lb_host, u.ed_host, u.bt_find, u.lb_pin_in, u.ed_pin, u.bt_pair_laptop, u.lb_pair_laptop, u.ck_block] {
        show(h, !pc);
    }
    let pair_text = if paired { "Сопряжение: есть ✓" } else { "Сопряжение: нет" };
    set_text(u.lb_pair_pc, pair_text);
    set_text(u.lb_pair_laptop, pair_text);
    set_text(u.bt_pair_pc, if server::pairing_state().is_some() { "Отменить" } else { "Сопрячь устройство" });
    for h in [u.rb_server, u.rb_client, u.cb_side, u.ed_host, u.bt_find, u.ck_block, u.ck_clip, u.ed_port] {
        enable(h, !running);
    }
    set_text(u.bt_start, if running { "Остановить" } else { "Запустить" });
    if !running {
        status(Status::Stopped, "Остановлено");
    }
}

fn gather(u: &Ui) -> Result<Cfg, String> {
    let mut c = cfg();
    c.server = is_pc(u);
    c.side_left = unsafe { SendMessageW(hw(u.cb_side), CB_GETCURSEL, 0, 0) == 1 };
    c.host = get_text(u.ed_host).trim().to_string();
    c.block = checked(u.ck_block);
    c.clip = checked(u.ck_clip);
    c.port = get_text(u.ed_port).trim().parse::<u16>().ok().filter(|p| *p > 0 && *p < 65535).ok_or("Порт должен быть числом от 1 до 65534")?;
    Ok(c)
}

fn start(u: &Ui) -> bool {
    let c = match gather(u) {
        Ok(c) => c,
        Err(e) => {
            log(e);
            return false;
        }
    };
    if !c.server && c.host.is_empty() {
        log("Укажите адрес ПК или нажмите «Найти»");
        return false;
    }
    if !c.server && c.key.is_empty() {
        log("Сначала сопрягите ноутбук с ПК: введите PIN с экрана ПК и нажмите «Сопрячь»");
        return false;
    }
    set_cfg(c.clone());
    let main = MAIN.load(Relaxed) as HWND;
    if c.server {
        if !setup::ensure_firewall(c.port) {
            log("Порт в брандмауэре не открыт: ноутбук не сможет подключиться");
        }
        if let Err(e) = server::start(c.port, c.side_left) {
            log(e);
            return false;
        }
        if c.clip {
            clip::init(main);
            clip::start_server(c.port + 1);
        }
        RUNNING.store(true, Relaxed);
        if c.key.is_empty() {
            status(Status::Attention, "Не сопряжено");
        } else {
            status(Status::Waiting, "Ожидание ноутбука");
        }
        log(format!("Запущено. Ноутбук стоит {} от монитора", if c.side_left { "слева" } else { "справа" }));
    } else {
        client::start(c.host.clone(), c.port, c.block);
        if c.clip {
            clip::init(main);
            clip::start_client(c.host.clone(), c.port + 1);
        }
        RUNNING.store(true, Relaxed);
        status(Status::Waiting, &format!("Подключаюсь к {}…", c.host));
    }
    refresh(u);
    true
}

fn stop(u: &Ui) {
    server::stop();
    client::stop();
    clip::stop(MAIN.load(Relaxed) as HWND);
    RUNNING.store(false, Relaxed);
    unsafe {
        KillTimer(MAIN.load(Relaxed) as HWND, TIMER_PIN);
    }
    refresh(u);
}

fn on_pair_pc(u: &Ui) {
    if server::pairing_state().is_some() {
        server::close_pairing();
        update_pin(u);
        if !proto::key().is_empty() {
            status(Status::Waiting, "Ожидание ноутбука");
        }
        return;
    }
    if !RUNNING.load(Relaxed) && !start(u) {
        return;
    }
    let pin = server::open_pairing();
    status(Status::Waiting, "Жду PIN с ноутбука");
    log(format!("Введите на ноутбуке PIN {} {} (действует 2 минуты)", &pin[..3], &pin[3..]));
    unsafe {
        SetTimer(MAIN.load(Relaxed) as HWND, TIMER_PIN, 500, None);
    }
    update_pin(u);
    refresh(u);
}

fn update_pin(u: &Ui) {
    match server::pairing_state() {
        Some((pin, left)) => {
            set_text(u.lb_pin, &format!("PIN:  {} {}     {}:{:02}", &pin[..3], &pin[3..], left / 60, left % 60));
            show(u.lb_pin, true);
        }
        None => {
            unsafe {
                KillTimer(MAIN.load(Relaxed) as HWND, TIMER_PIN);
            }
            show(u.lb_pin, false);
            if RUNNING.load(Relaxed) && proto::key().is_empty() {
                status(Status::Attention, "Не сопряжено");
            }
            refresh(u);
        }
    }
}

fn on_pair_laptop(u: &Ui) {
    let pin: String = get_text(u.ed_pin).chars().filter(|c| c.is_ascii_digit()).collect();
    let host = get_text(u.ed_host).trim().to_string();
    if host.is_empty() {
        log("Укажите адрес ПК или нажмите «Найти»");
        return;
    }
    if pin.len() != 6 {
        log("PIN — это 6 цифр с экрана ПК");
        return;
    }
    let port = match gather(u) {
        Ok(c) => c.port,
        Err(e) => {
            log(e);
            return;
        }
    };
    enable(u.bt_pair_laptop, false);
    log(format!("Сопряжение с {host}…"));
    std::thread::spawn(move || {
        match client::pair_with(&host, port, &pin) {
            Ok(key) => {
                proto::set_key(&key);
                log("Сопряжено ✓");
                paired(&key);
            }
            Err(e) => log(format!("Сопряжение не удалось: {e}")),
        }
        post(WM_PAIR_DONE);
    });
}

fn on_paired(u: &Ui) {
    let Some(key) = NEW_KEY.lock().unwrap().take() else { return };
    let mut c = gather(u).unwrap_or_else(|_| cfg());
    c.key = key;
    set_cfg(c);
    set_text(u.ed_pin, "");
    if is_pc(u) {
        if RUNNING.load(Relaxed) {
            status(Status::Waiting, "Ожидание ноутбука");
        }
    } else {
        // (Re)connect with the new key.
        if RUNNING.load(Relaxed) {
            stop(u);
        }
        start(u);
    }
    refresh(u);
}

fn on_find(u: &Ui) {
    let port = match gather(u) {
        Ok(c) => c.port,
        Err(e) => {
            log(e);
            return;
        }
    };
    enable(u.bt_find, false);
    log("Ищу ПК в сети…");
    std::thread::spawn(move || {
        *FOUND.lock().unwrap() = client::discover(port);
        post(WM_FOUND);
    });
}

fn on_found(u: &Ui) {
    enable(u.bt_find, true);
    let found = std::mem::take(&mut *FOUND.lock().unwrap());
    match found.as_slice() {
        [] => log("ПК не найден. На ПК должно быть нажато «Запустить», и оба устройства — в одной сети"),
        [(ip, name), rest @ ..] => {
            set_text(u.ed_host, ip);
            log(format!("Найден ПК {name} ({ip})"));
            for (ip, name) in rest {
                log(format!("Ещё найден: {name} ({ip})"));
            }
        }
    }
}

/// Runs on a worker thread: the elevated call blocks until the user answers UAC.
fn apply_service(on: bool, u: Ui) {
    let restart = on && !setup::is_system();
    let res = setup::set_service(on, restart);
    let now = if res.is_ok() { on } else { !on };
    let mut c = cfg();
    c.service = now;
    set_cfg(c);
    set_check(u.ck_service, now);
    enable(u.ck_auto, !now);
    if now {
        setup::set_autostart(false);
        set_check(u.ck_auto, false);
    }
    match res {
        Ok(()) => log(if on { "Режим службы включён" } else { "Режим службы выключен (вступит в силу при следующем запуске)" }),
        Err(e) => {
            log(format!("Не удалось изменить режим службы: {e}"));
            return;
        }
    }
    if restart {
        log("Перезапускаюсь в режиме службы…");
        post(WM_EXIT);
    }
}

fn drain_log(u: &Ui) {
    let lines: Vec<String> = LOG.lock().unwrap().drain(..).collect();
    for l in lines {
        unsafe {
            SendMessageW(hw(u.log), EM_SETSEL, usize::MAX, -1);
            SendMessageW(hw(u.log), EM_REPLACESEL, 0, w(&format!("{l}\r\n")).as_ptr() as LPARAM);
        }
    }
}

fn apply_status(u: &Ui) {
    let (_, text) = STATUS.lock().unwrap().clone();
    set_text(u.lb_status, &format!("●  {text}"));
    unsafe {
        InvalidateRect(hw(u.lb_status), std::ptr::null(), 1);
    }
}

fn rgb(r: u8, g: u8, b: u8) -> u32 {
    r as u32 | (g as u32) << 8 | (b as u32) << 16
}

unsafe extern "system" fn wndproc(h: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let u = ui();
    match msg {
        WM_COMMAND => {
            let id = (wp & 0xFFFF) as i32;
            let code = (wp >> 16) & 0xFFFF;
            match id as usize {
                ID_TRAY_OPEN => show_main(h),
                ID_TRAY_EXIT => {
                    PostMessageW(h, WM_EXIT, 0, 0);
                }
                _ => {}
            }
            if let (Some(u), 0) = (u, code) {
                match id {
                    ID_SERVER | ID_CLIENT => refresh(&u),
                    ID_START => {
                        if RUNNING.load(Relaxed) {
                            stop(&u);
                            log("Остановлено");
                        } else {
                            start(&u);
                        }
                    }
                    ID_PAIR_PC => on_pair_pc(&u),
                    ID_PAIR_LAPTOP => on_pair_laptop(&u),
                    ID_FIND => on_find(&u),
                    ID_AUTO => setup::set_autostart(checked(u.ck_auto)),
                    ID_SERVICE => {
                        let on = checked(u.ck_service);
                        std::thread::spawn(move || apply_service(on, u));
                    }
                    _ => {}
                }
            }
            0
        }
        WM_CTLCOLORSTATIC => {
            let dc = wp as HDC;
            SetBkColor(dc, rgb(255, 255, 255));
            if let Some(u) = u {
                if lp as usize == u.lb_status {
                    let color = match STATUS.lock().unwrap().0 {
                        Status::Stopped => rgb(120, 120, 120),
                        Status::Waiting => rgb(217, 119, 6),
                        Status::Connected => rgb(22, 163, 74),
                        Status::Attention => rgb(220, 38, 38),
                    };
                    SetTextColor(dc, color);
                } else if lp as usize == u.lb_pin {
                    SetTextColor(dc, rgb(37, 99, 235));
                } else if lp as usize == u.lb_ip {
                    SetTextColor(dc, rgb(80, 80, 80));
                }
            }
            GetStockObject(WHITE_BRUSH) as LRESULT
        }
        WM_CLIPBOARDUPDATE => {
            clip::on_local_change();
            0
        }
        clip::WM_CLIP_IN => {
            clip::apply_remote();
            0
        }
        WM_TIMER if wp == TIMER_PIN => {
            if let Some(u) = u {
                update_pin(&u);
            }
            0
        }
        WM_LOG => {
            if let Some(u) = u {
                drain_log(&u);
            }
            0
        }
        WM_STATUS => {
            if let Some(u) = u {
                apply_status(&u);
            }
            0
        }
        WM_PAIRED => {
            if let Some(u) = u {
                on_paired(&u);
            }
            0
        }
        WM_PAIR_DONE => {
            if let Some(u) = u {
                enable(u.bt_pair_laptop, true);
            }
            0
        }
        WM_FOUND => {
            if let Some(u) = u {
                on_found(&u);
            }
            0
        }
        // The cross hides the window to the tray and keeps working; "Выход" in the tray menu quits.
        WM_CLOSE => {
            ShowWindow(h, SW_HIDE);
            0
        }
        WM_EXIT => {
            if let Some(u) = u {
                stop(&u);
            }
            tray_icon(h, NIM_DELETE);
            DestroyWindow(h);
            0
        }
        WM_SHOWME => {
            show_main(h);
            0
        }
        WM_TRAY => {
            match (lp & 0xFFFF) as u32 {
                WM_LBUTTONUP | WM_LBUTTONDBLCLK => show_main(h),
                WM_RBUTTONUP => tray_menu(h),
                _ => {}
            }
            0
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            0
        }
        m if m != 0 && m == TASKBAR_CREATED.load(Relaxed) => {
            tray_icon(h, NIM_ADD); // explorer restarted
            0
        }
        _ => DefWindowProcW(h, msg, wp, lp),
    }
}

fn ctl(parent: HWND, font: HFONT, class: &str, text: &str, style: u32, ex: u32, r: [i32; 4], id: i32) -> usize {
    unsafe {
        let h = CreateWindowExW(
            ex,
            w(class).as_ptr(),
            w(text).as_ptr(),
            style | WS_CHILD | WS_VISIBLE,
            sc(r[0]),
            sc(r[1]),
            sc(r[2]),
            sc(r[3]),
            parent,
            id as isize as _,
            GetModuleHandleW(std::ptr::null()),
            std::ptr::null(),
        );
        SendMessageW(h, WM_SETFONT, font as WPARAM, 1);
        h as usize
    }
}

pub fn run(autostart: bool) {
    let c = config::load();
    proto::set_key(&c.key);
    *CFG.lock().unwrap() = Some(c.clone());

    unsafe {
        DPI.store(GetDpiForSystem().max(96), Relaxed);
        let hinst = GetModuleHandleW(std::ptr::null());
        let class = w("kvmlink_main");
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: 0,
            lpfnWndProc: Some(wndproc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: hinst,
            hIcon: app_icon(GetSystemMetrics(SM_CXICON)),
            hCursor: LoadCursorW(0 as _, IDC_ARROW),
            hbrBackground: GetStockObject(WHITE_BRUSH) as _,
            lpszMenuName: std::ptr::null(),
            lpszClassName: class.as_ptr(),
            hIconSm: app_icon(GetSystemMetrics(SM_CXSMICON)),
        };
        RegisterClassExW(&wc);

        let title = if setup::is_system() {
            "KVM Link (служба)"
        } else if setup::is_admin() {
            "KVM Link (администратор)"
        } else {
            "KVM Link"
        };
        let style = WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX;
        let mut r = RECT { left: 0, top: 0, right: sc(500), bottom: sc(574) };
        AdjustWindowRect(&mut r, style, 0);
        let main = CreateWindowExW(
            0,
            class.as_ptr(),
            w(title).as_ptr(),
            style,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            r.right - r.left,
            r.bottom - r.top,
            0 as _,
            0 as _,
            hinst,
            std::ptr::null(),
        );
        MAIN.store(main as usize, Relaxed);

        let font = CreateFontW(-sc(15), 0, 0, 0, 400, 0, 0, 0, 1, 0, 0, 5, 0, w("Segoe UI").as_ptr());
        let font_big = CreateFontW(-sc(24), 0, 0, 0, 600, 0, 0, 0, 1, 0, 0, 5, 0, w("Segoe UI").as_ptr());
        let font_bold = CreateFontW(-sc(15), 0, 0, 0, 600, 0, 0, 0, 1, 0, 0, 5, 0, w("Segoe UI").as_ptr());
        let tab = WS_TABSTOP;
        let (chk, rad, grp) = (BS_AUTOCHECKBOX as u32, BS_AUTORADIOBUTTON as u32, BS_GROUPBOX as u32);
        let edit = ES_AUTOHSCROLL as u32;
        let edge = WS_EX_CLIENTEDGE;
        let c_ = |class: &str, text: &str, style: u32, ex: u32, rc: [i32; 4], id: i32| ctl(main, font, class, text, style, ex, rc, id);

        // Role
        c_("BUTTON", "Роль", grp, 0, [12, 8, 476, 78], -1);
        let rb_server = c_("BUTTON", "Этот компьютер — с клавиатурой и мышью (ПК)", rad | tab | WS_GROUP, 0, [26, 30, 450, 22], ID_SERVER);
        let rb_client = c_("BUTTON", "Этот компьютер принимает ввод (ноутбук)", rad | tab, 0, [26, 54, 450, 22], ID_CLIENT);

        // Connection
        c_("BUTTON", "Подключение", grp, 0, [12, 94, 476, 160], -1);
        let lb_side = c_("STATIC", "Ноутбук стоит:", 0, 0, [26, 122, 170, 20], -1);
        let cb_side = c_("COMBOBOX", "", CBS_DROPDOWNLIST as u32 | tab | WS_VSCROLL, 0, [200, 118, 274, 200], -1);
        SendMessageW(hw(cb_side), CB_ADDSTRING, 0, w("справа от монитора").as_ptr() as LPARAM);
        SendMessageW(hw(cb_side), CB_ADDSTRING, 0, w("слева от монитора").as_ptr() as LPARAM);
        let ip = local_ipv4().map(|i| i.to_string()).unwrap_or_else(|| "?".into());
        let name = std::env::var("COMPUTERNAME").unwrap_or_default();
        let lb_ip = c_("STATIC", &format!("Этот ПК: {ip}  ·  {name}"), 0, 0, [26, 152, 448, 20], -1);
        let lb_pair_pc = c_("STATIC", "", 0, 0, [26, 186, 250, 20], -1);
        let bt_pair_pc = c_("BUTTON", "Сопрячь устройство", tab, 0, [290, 180, 184, 30], ID_PAIR_PC);
        let lb_pin = c_("STATIC", "", SS_CENTER as u32, 0, [26, 216, 448, 32], -1);
        SendMessageW(hw(lb_pin), WM_SETFONT, font_big as WPARAM, 1);

        let lb_host = c_("STATIC", "Адрес ПК:", 0, 0, [26, 122, 170, 20], -1);
        let ed_host = c_("EDIT", "", edit | tab, edge, [200, 118, 180, 24], -1);
        let bt_find = c_("BUTTON", "Найти", tab, 0, [388, 117, 86, 26], ID_FIND);
        let lb_pin_in = c_("STATIC", "PIN с экрана ПК:", 0, 0, [26, 154, 170, 20], -1);
        let ed_pin = c_("EDIT", "", edit | ES_NUMBER as u32 | tab, edge, [200, 150, 100, 24], -1);
        SendMessageW(hw(ed_pin), EM_LIMITTEXT, 6, 0);
        let bt_pair_laptop = c_("BUTTON", "Сопрячь", tab, 0, [308, 149, 166, 26], ID_PAIR_LAPTOP);
        let lb_pair_laptop = c_("STATIC", "", 0, 0, [26, 186, 448, 20], -1);
        let ck_block = c_("BUTTON", "Блокировать клавиатуру этого ноутбука", chk | tab, 0, [26, 216, 448, 22], -1);

        // Options
        c_("BUTTON", "Параметры", grp, 0, [12, 262, 476, 128], -1);
        let ck_clip = c_("BUTTON", "Общий буфер обмена (текст и картинки)", chk | tab, 0, [26, 284, 448, 22], -1);
        let ck_auto = c_("BUTTON", "Запускать вместе с Windows", chk | tab, 0, [26, 308, 448, 22], ID_AUTO);
        let ck_service = c_("BUTTON", "Режим службы (окна администратора и запросы UAC)", chk | tab, 0, [26, 332, 448, 22], ID_SERVICE);
        c_("STATIC", "Порт:", 0, 0, [26, 362, 50, 20], -1);
        let ed_port = c_("EDIT", "", edit | ES_NUMBER as u32 | tab, edge, [80, 358, 70, 24], -1);

        // Status + start
        let lb_status = c_("STATIC", "", SS_CENTERIMAGE as u32, 0, [16, 402, 300, 36], -1);
        SendMessageW(hw(lb_status), WM_SETFONT, font_bold as WPARAM, 1);
        let bt_start = c_("BUTTON", "Запустить", tab | BS_DEFPUSHBUTTON as u32, 0, [330, 400, 158, 40], ID_START);
        let lg = c_("EDIT", "", (ES_MULTILINE | ES_READONLY | ES_AUTOVSCROLL) as u32 | WS_VSCROLL, edge, [12, 450, 476, 112], -1);

        let u = Ui {
            rb_server, rb_client, lb_side, cb_side, lb_ip, lb_pair_pc, bt_pair_pc, lb_pin,
            lb_host, ed_host, bt_find, lb_pin_in, ed_pin, bt_pair_laptop, lb_pair_laptop, ck_block,
            ck_clip, ck_auto, ck_service, ed_port, lb_status, bt_start, log: lg,
        };
        *UI.lock().unwrap() = Some(u);

        set_check(rb_server, c.server);
        set_check(rb_client, !c.server);
        SendMessageW(hw(cb_side), CB_SETCURSEL, c.side_left as WPARAM, 0);
        set_text(ed_host, &c.host);
        set_check(ck_block, c.block);
        set_check(ck_clip, c.clip);
        set_text(ed_port, &c.port.to_string());
        let svc = c.service || setup::service_enabled();
        set_check(ck_service, svc);
        set_check(ck_auto, !svc && setup::autostart_enabled());
        enable(ck_auto, !svc);
        refresh(&u);
        apply_status(&u);
        sys::boost_thread();

        // An elevated/SYSTEM window ignores messages from the (non-elevated) shell unless allowed.
        let tbc = RegisterWindowMessageW(w("TaskbarCreated").as_ptr());
        TASKBAR_CREATED.store(tbc, Relaxed);
        for m in [WM_SHOWME, WM_TRAY, tbc] {
            ChangeWindowMessageFilterEx(main, m, MSGFLT_ALLOW, std::ptr::null_mut());
        }
        tray_icon(main, NIM_ADD);
        if !autostart {
            ShowWindow(main, SW_SHOWNORMAL);
        }

        let ready = c.server || (!c.key.is_empty() && !c.host.is_empty());
        if autostart && ready {
            start(&u);
        } else if c.key.is_empty() {
            log(if c.server {
                "Нажмите «Сопрячь устройство», затем введите показанный PIN на ноутбуке"
            } else {
                "Найдите ПК («Найти»), введите PIN с экрана ПК и нажмите «Сопрячь»"
            });
        } else {
            log("Нажмите «Запустить»");
        }

        let mut m: MSG = std::mem::zeroed();
        while GetMessageW(&mut m, 0 as _, 0, 0) > 0 {
            if IsDialogMessageW(main, &m) == 0 {
                TranslateMessage(&m);
                DispatchMessageW(&m);
            }
        }
    }
}
