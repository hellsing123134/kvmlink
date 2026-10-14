//! The window, drawn with egui: a layout editor where this computer sits in the middle and
//! paired devices are dragged to the side of the screen they physically stand on.

use crate::config::{self, Peer, Side};
use crate::net::{self, Found, State};
use crate::{clip, setup, tray};
use eframe::egui::{
    self, text::LayoutJob, Align, Align2, Color32, CornerRadius, CursorIcon, FontFamily, FontId, Frame, Id,
    Layout, Margin, Pos2, Rect, RichText, Sense, Shape, Stroke, StrokeKind, TextWrapMode, Vec2,
};
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

// ---------- calls from worker threads ----------

static CTX: OnceLock<egui::Context> = OnceLock::new();
static LOG: Mutex<Vec<(String, String)>> = Mutex::new(Vec::new());
/// Name of a device that just paired by typing our PIN.
static PAIRED: Mutex<Option<String>> = Mutex::new(None);

/// Asks the window to redraw (state changed somewhere).
pub fn notify() {
    if let Some(c) = CTX.get() {
        c.request_repaint();
    }
}

fn clock() -> String {
    use windows_sys::Win32::System::SystemInformation::GetLocalTime;
    let t = unsafe {
        let mut t = std::mem::zeroed();
        GetLocalTime(&mut t);
        t
    };
    format!("{:02}:{:02}:{:02}", t.wHour, t.wMinute, t.wSecond)
}

/// Adds a line to the activity log.
pub fn log(s: impl Into<String>) {
    let mut l = LOG.lock().unwrap();
    l.push((clock(), s.into()));
    let n = l.len();
    if n > 500 {
        l.drain(..n - 500);
    }
    drop(l);
    notify();
}

/// Pairing finished on the side that showed the PIN.
pub fn paired(name: &str) {
    *PAIRED.lock().unwrap() = Some(name.to_string());
    notify();
}

// ---------- look ----------

const BG: Color32 = Color32::from_rgb(0x0e, 0x10, 0x15);
const PANEL: Color32 = Color32::from_rgb(0x14, 0x17, 0x1f);
const CARD: Color32 = Color32::from_rgb(0x1a, 0x1e, 0x29);
const CARD_HI: Color32 = Color32::from_rgb(0x23, 0x28, 0x36);
const LINE: Color32 = Color32::from_rgb(0x2a, 0x30, 0x40);
const TEXT: Color32 = Color32::from_rgb(0xe8, 0xeb, 0xf2);
const WEAK: Color32 = Color32::from_rgb(0x8b, 0x93, 0xa7);
const FAINT: Color32 = Color32::from_rgb(0x5a, 0x62, 0x75);
const ACCENT: Color32 = Color32::from_rgb(0x5b, 0x8d, 0xff);
const GREEN: Color32 = Color32::from_rgb(0x3c, 0xcf, 0x86);
const AMBER: Color32 = Color32::from_rgb(0xf2, 0xa9, 0x3b);
const RED: Color32 = Color32::from_rgb(0xf0, 0x60, 0x60);
const VIOLET: Color32 = Color32::from_rgb(0xa7, 0x82, 0xff);

fn semibold(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name("semibold".into()))
}

fn regular(size: f32) -> FontId {
    FontId::proportional(size)
}

fn mix(a: Color32, b: Color32, t: f32) -> Color32 {
    let l = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Color32::from_rgb(l(a.r(), b.r()), l(a.g(), b.g()), l(a.b(), b.b()))
}

fn setup_fonts(ctx: &egui::Context) {
    let mut f = egui::FontDefinitions::default();
    let dir = std::path::Path::new(&std::env::var("WINDIR").unwrap_or_else(|_| r"C:\Windows".into())).join("Fonts");
    let mut have = |name: &str, file: &str| -> bool {
        match std::fs::read(dir.join(file)) {
            Ok(b) => {
                f.font_data.insert(name.into(), Arc::new(egui::FontData::from_owned(b)));
                true
            }
            Err(_) => false,
        }
    };
    let ui_font = have("segoe", "segoeui.ttf");
    let sb = have("segoe-sb", "seguisb.ttf");
    let sym = have("segoe-sym", "seguisym.ttf");
    let mono = have("cascadia", "CascadiaMono.ttf") || have("cascadia", "consola.ttf");
    let defaults = f.families.get(&FontFamily::Proportional).cloned().unwrap_or_default();
    let mut prop = Vec::new();
    if ui_font {
        prop.push("segoe".to_string());
    }
    prop.extend(defaults.iter().cloned());
    if sym {
        prop.push("segoe-sym".into());
    }
    let mut bold = Vec::new();
    if sb {
        bold.push("segoe-sb".to_string());
    }
    bold.extend(prop.iter().cloned());
    f.families.insert(FontFamily::Proportional, prop);
    f.families.insert(FontFamily::Name("semibold".into()), bold);
    if mono {
        f.families.entry(FontFamily::Monospace).or_default().insert(0, "cascadia".into());
    }
    ctx.set_fonts(f);
}

fn setup_style(ctx: &egui::Context) {
    use egui::TextStyle::*;
    ctx.set_theme(egui::Theme::Dark);
    ctx.style_mut_of(egui::Theme::Dark, |s| {
        s.text_styles = [
            (Heading, semibold(20.0)),
            (Body, regular(14.0)),
            (Button, regular(14.0)),
            (Small, regular(12.0)),
            (Monospace, FontId::monospace(12.5)),
        ]
        .into();
        s.spacing.item_spacing = Vec2::new(8.0, 8.0);
        s.spacing.button_padding = Vec2::new(14.0, 6.0);
        s.spacing.interact_size.y = 30.0;
        s.interaction.selectable_labels = false;

        let v = &mut s.visuals;
        *v = egui::Visuals::dark();
        v.panel_fill = BG;
        v.window_fill = PANEL;
        v.window_stroke = Stroke::new(1.0, LINE);
        v.window_corner_radius = CornerRadius::same(14);
        v.extreme_bg_color = Color32::from_rgb(0x0b, 0x0d, 0x12);
        v.faint_bg_color = CARD;
        v.override_text_color = Some(TEXT);
        v.hyperlink_color = ACCENT;
        v.selection.bg_fill = ACCENT.gamma_multiply(0.45);
        v.selection.stroke = Stroke::new(1.0, ACCENT);
        let r = CornerRadius::same(8);
        for (w, fill, stroke) in [
            (&mut v.widgets.noninteractive, CARD, LINE),
            (&mut v.widgets.inactive, CARD_HI, LINE),
            (&mut v.widgets.hovered, mix(CARD_HI, ACCENT, 0.18), mix(LINE, ACCENT, 0.5)),
            (&mut v.widgets.active, mix(CARD_HI, ACCENT, 0.32), ACCENT),
            (&mut v.widgets.open, CARD_HI, LINE),
        ] {
            w.bg_fill = fill;
            w.weak_bg_fill = fill;
            w.bg_stroke = Stroke::new(1.0, stroke);
            w.corner_radius = r;
            w.fg_stroke.color = TEXT;
        }
        v.widgets.noninteractive.fg_stroke.color = TEXT;
    });
}

// ---------- small widgets ----------

fn toggle(ui: &mut egui::Ui, on: &mut bool, enabled: bool) -> egui::Response {
    let (rect, mut resp) = ui.allocate_exact_size(Vec2::new(40.0, 22.0), if enabled { Sense::click() } else { Sense::hover() });
    if enabled && resp.clicked() {
        *on = !*on;
        resp.mark_changed();
    }
    let t = ui.ctx().animate_bool_responsive(resp.id, *on);
    let mut bg = mix(LINE, ACCENT, t);
    if !enabled {
        bg = bg.gamma_multiply(0.45);
    }
    let r = rect.height() / 2.0;
    let p = ui.painter();
    p.rect_filled(rect, r, bg);
    let x = egui::lerp((rect.left() + r)..=(rect.right() - r), t);
    p.circle_filled(Pos2::new(x, rect.center().y), r - 3.0, if enabled { Color32::WHITE } else { WEAK });
    if enabled {
        resp = resp.on_hover_cursor(CursorIcon::PointingHand);
    }
    resp
}

fn card<R>(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    Frame::new()
        .fill(CARD)
        .stroke(Stroke::new(1.0, LINE))
        .corner_radius(12.0)
        .inner_margin(Margin::symmetric(18, 14))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui)
        })
        .inner
}

fn section(ui: &mut egui::Ui, title: &str) {
    ui.add_space(6.0);
    ui.label(RichText::new(title.to_uppercase()).font(semibold(11.5)).color(FAINT));
}

/// A settings row: title and description on the left, a switch on the right.
fn setting(ui: &mut egui::Ui, title: &str, desc: &str, on: &mut bool, enabled: bool) -> bool {
    let mut changed = false;
    ui.horizontal(|ui| {
        let w = ui.available_width() - 56.0;
        ui.vertical(|ui| {
            ui.set_width(w);
            ui.label(RichText::new(title).font(semibold(14.5)).color(if enabled { TEXT } else { WEAK }));
            ui.label(RichText::new(desc).size(12.5).color(WEAK));
        });
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            changed = toggle(ui, on, enabled).changed();
        });
    });
    changed
}

fn primary_button(ui: &mut egui::Ui, text: &str, enabled: bool) -> egui::Response {
    let b = egui::Button::new(RichText::new(text).font(semibold(14.0)).color(Color32::WHITE))
        .fill(if enabled { ACCENT } else { ACCENT.gamma_multiply(0.35) })
        .stroke(Stroke::NONE)
        .corner_radius(8.0)
        .min_size(Vec2::new(0.0, 34.0));
    let r = ui.add_enabled(enabled, b);
    if enabled {
        r.on_hover_cursor(CursorIcon::PointingHand)
    } else {
        r
    }
}

fn ghost_button(ui: &mut egui::Ui, text: &str) -> egui::Response {
    ui.add(egui::Button::new(RichText::new(text).size(13.5)).min_size(Vec2::new(0.0, 32.0)))
        .on_hover_cursor(CursorIcon::PointingHand)
}

/// Text cut with an ellipsis to `width`.
fn fit_text(p: &egui::Painter, text: &str, font: FontId, color: Color32, width: f32) -> Arc<egui::Galley> {
    let mut job = LayoutJob::simple_singleline(text.to_string(), font, color);
    job.wrap = egui::text::TextWrapping::truncate_at_width(width);
    p.layout_job(job)
}

/// A small monitor glyph centred at `c`.
fn monitor(p: &egui::Painter, c: Pos2, w: f32, color: Color32) {
    let h = w * 0.62;
    let screen = Rect::from_center_size(c - Vec2::new(0.0, h * 0.12), Vec2::new(w, h));
    let s = Stroke::new((w / 16.0).clamp(1.2, 2.0), color);
    p.rect_stroke(screen, CornerRadius::same((w / 9.0) as u8), s, StrokeKind::Inside);
    let foot = screen.bottom() + h * 0.22;
    p.line_segment([Pos2::new(c.x, screen.bottom()), Pos2::new(c.x, foot)], s);
    p.line_segment([Pos2::new(c.x - w * 0.2, foot), Pos2::new(c.x + w * 0.2, foot)], s);
}

fn state_style(s: State) -> (&'static str, Color32) {
    match s {
        State::Offline => ("Offline", FAINT),
        State::Online => ("Connected", GREEN),
        State::Controlling => ("Using your keyboard", ACCENT),
        State::ControlledBy => ("Controls this computer", VIOLET),
    }
}

fn dashed_rect(p: &egui::Painter, r: Rect, color: Color32) {
    let pts = [r.left_top(), r.right_top(), r.right_bottom(), r.left_bottom(), r.left_top()];
    p.extend(Shape::dashed_line(&pts, Stroke::new(1.2, color), 6.0, 5.0));
}

// ---------- the app ----------

#[derive(Clone, Copy, PartialEq)]
enum Tab {
    Layout,
    Settings,
    Activity,
}

#[derive(Clone, Copy, PartialEq)]
enum AddMode {
    Enter,
    Show,
}

type Slot<T> = Arc<Mutex<Option<T>>>;

struct AddDialog {
    mode: AddMode,
    scan: Slot<Vec<Found>>,
    scanning: Arc<AtomicBool>,
    found: Vec<Found>,
    host: String,
    pin: String,
    result: Slot<Result<String, String>>,
    busy: bool,
    error: Option<String>,
    done: Option<String>,
}

impl AddDialog {
    fn new() -> Self {
        let d = AddDialog {
            mode: AddMode::Enter,
            scan: Arc::new(Mutex::new(None)),
            scanning: Arc::new(AtomicBool::new(false)),
            found: Vec::new(),
            host: String::new(),
            pin: String::new(),
            result: Arc::new(Mutex::new(None)),
            busy: false,
            error: None,
            done: None,
        };
        d.rescan();
        d
    }

    fn rescan(&self) {
        if self.scanning.swap(true, Relaxed) {
            return;
        }
        let (slot, flag, port) = (self.scan.clone(), self.scanning.clone(), net::peer_port());
        std::thread::spawn(move || {
            *slot.lock().unwrap() = Some(net::discover(port));
            flag.store(false, Relaxed);
            notify();
        });
    }
}

struct Drag {
    id: u64,
    grab: Vec2,
}

pub struct App {
    tab: Tab,
    drag: Option<Drag>,
    selected: Option<u64>,
    add: Option<AddDialog>,
    forget: Option<u64>,
    autostart: bool,
    service: bool,
    service_busy: Arc<AtomicBool>,
    service_result: Slot<bool>,
    port_text: String,
    ip: String,
    /// Started for the tray only: eframe shows the window after its first frame anyway, so it
    /// is created off screen and hidden right after that frame.
    start_hidden: bool,
    frames: u32,
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>, start_hidden: bool) -> Self {
        let _ = CTX.set(cc.egui_ctx.clone());
        setup_fonts(&cc.egui_ctx);
        setup_style(&cc.egui_ctx);
        {
            use raw_window_handle::{HasWindowHandle, RawWindowHandle};
            if let Ok(h) = cc.window_handle() {
                if let RawWindowHandle::Win32(w) = h.as_raw() {
                    tray::set_app_window(w.hwnd.get());
                }
            }
        }
        let c = config::get();
        let service = c.service || setup::service_enabled();
        App {
            tab: Tab::Layout,
            drag: None,
            selected: None,
            add: None,
            forget: None,
            autostart: !service && setup::autostart_enabled(),
            service,
            service_busy: Arc::new(AtomicBool::new(false)),
            service_result: Arc::new(Mutex::new(None)),
            port_text: c.port.to_string(),
            ip: net::local_ipv4().map(|i| i.to_string()).unwrap_or_else(|| "no network".into()),
            start_hidden,
            frames: 0,
        }
    }

    // ----- header -----

    fn header(&mut self, ui: &mut egui::Ui, c: &config::Cfg) {
        ui.horizontal(|ui| {
            let (r, _) = ui.allocate_exact_size(Vec2::splat(38.0), Sense::hover());
            let p = ui.painter();
            p.rect_filled(r, 10.0, ACCENT);
            monitor(p, r.center() + Vec2::new(0.0, 1.0), 20.0, Color32::WHITE);
            ui.add_space(4.0);
            ui.vertical(|ui| {
                ui.add_space(1.0);
                ui.label(RichText::new("KVM Link").font(semibold(19.0)));
                ui.add_space(-6.0);
                ui.label(RichText::new("One keyboard and mouse for all your computers").size(12.5).color(WEAK));
            });
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let mut on = net::running();
                if toggle(ui, &mut on, true).on_hover_text("Turn sharing on or off").changed() {
                    if on {
                        if let Err(e) = net::start() {
                            log(e);
                        }
                    } else {
                        net::stop();
                        log("Sharing paused");
                    }
                }
                ui.add_space(6.0);
                let (text, color) = self.summary(c);
                Frame::new()
                    .fill(color.gamma_multiply(0.14))
                    .corner_radius(20.0)
                    .inner_margin(Margin::symmetric(12, 5))
                    .show(ui, |ui| {
                        // right-to-left here: the label is added first so the dot ends up before it
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(text).size(13.0).color(color));
                            let (d, _) = ui.allocate_exact_size(Vec2::splat(8.0), Sense::hover());
                            ui.painter().circle_filled(d.center(), 4.0, color);
                        });
                    });
            });
        });
        ui.add_space(10.0);
        ui.horizontal(|ui| {
            for (t, name) in [(Tab::Layout, "Layout"), (Tab::Settings, "Settings"), (Tab::Activity, "Activity")] {
                let sel = self.tab == t;
                let text = RichText::new(name).font(if sel { semibold(14.0) } else { regular(14.0) }).color(if sel { TEXT } else { WEAK });
                let r = ui.add(egui::Button::new(text).frame(false).min_size(Vec2::new(0.0, 30.0)));
                if sel {
                    let rr = r.rect;
                    ui.painter().line_segment(
                        [Pos2::new(rr.left() + 2.0, rr.bottom() + 3.0), Pos2::new(rr.right() - 2.0, rr.bottom() + 3.0)],
                        Stroke::new(2.5, ACCENT),
                    );
                }
                if r.on_hover_cursor(CursorIcon::PointingHand).clicked() {
                    self.tab = t;
                }
                ui.add_space(10.0);
            }
        });
    }

    fn summary(&self, c: &config::Cfg) -> (String, Color32) {
        if !net::running() {
            return ("Paused".into(), WEAK);
        }
        if c.peers.is_empty() {
            return ("No devices yet".into(), AMBER);
        }
        let mut online = 0;
        for p in &c.peers {
            match net::state(p.id) {
                State::Controlling => return (format!("Using {}", p.name), ACCENT),
                State::ControlledBy => return (format!("{} is in control", p.name), VIOLET),
                State::Online => online += 1,
                State::Offline => {}
            }
        }
        match online {
            0 => ("Waiting for devices".into(), AMBER),
            1 => ("1 device connected".into(), GREEN),
            n => (format!("{n} devices connected"), GREEN),
        }
    }

    // ----- layout tab -----

    fn layout_tab(&mut self, ui: &mut egui::Ui, c: &config::Cfg) {
        if c.legacy {
            Frame::new()
                .fill(AMBER.gamma_multiply(0.12))
                .stroke(Stroke::new(1.0, AMBER.gamma_multiply(0.4)))
                .corner_radius(10.0)
                .inner_margin(Margin::symmetric(14, 10))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal(|ui| {
                        ui.label(
                            RichText::new("This version pairs devices differently. Pair your computers again with Add device.")
                                .color(AMBER),
                        );
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if ui.small_button("Dismiss").clicked() {
                                config::update(|c| c.legacy = false);
                            }
                        });
                    });
                });
            ui.add_space(4.0);
        }
        ui.label(
            RichText::new("Drag each device to the side of the screen it stands on, then push the cursor past that edge. Works both ways.")
                .size(13.0)
                .color(WEAK),
        );
        ui.add_space(4.0);

        let strip_h = 86.0;
        let details_h = 56.0;
        let canvas_h = (ui.available_height() - strip_h - details_h - 24.0).max(300.0);
        let (canvas, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), canvas_h), Sense::hover());
        let pointer = ui.ctx().input(|i| i.pointer.latest_pos());
        let dragging = self.drag.as_ref().map(|d| d.id);

        // geometry
        let gap = 22.0;
        let ch = ((canvas.height() - 2.0 * gap - 24.0) / 3.0).clamp(72.0, 118.0);
        let cw = (ch * 1.9).max(160.0).min((canvas.width() - 2.0 * gap - 24.0) / 3.0);
        let size = Vec2::new(cw, ch);
        let center = canvas.center();
        let slot = |s: Side| -> Rect {
            let off = match s {
                Side::Left => Vec2::new(-(size.x + gap), 0.0),
                Side::Right => Vec2::new(size.x + gap, 0.0),
                Side::Top => Vec2::new(0.0, -(size.y + gap)),
                Side::Bottom => Vec2::new(0.0, size.y + gap),
            };
            Rect::from_center_size(center + off, size)
        };

        let p = ui.painter_at(canvas);
        p.rect_filled(canvas, 14.0, PANEL);
        // dot grid
        let step = 22.0;
        let mut y = canvas.top() + step / 2.0;
        while y < canvas.bottom() {
            let mut x = canvas.left() + step / 2.0;
            while x < canvas.right() {
                p.circle_filled(Pos2::new(x, y), 0.9, Color32::from_rgb(0x22, 0x27, 0x33));
                x += step;
            }
            y += step;
        }

        let me = Rect::from_center_size(center, size);
        let hovered_slot = match (dragging, pointer) {
            (Some(_), Some(pt)) => Side::ALL.into_iter().find(|s| slot(*s).expand(gap / 2.0).contains(pt)),
            _ => None,
        };

        // links from this computer to placed devices
        for s in Side::ALL {
            if let Some(peer) = c.at(s) {
                if Some(peer.id) == dragging {
                    continue;
                }
                let st = net::state(peer.id);
                let color = match st {
                    State::Offline => LINE,
                    _ => state_style(st).1.gamma_multiply(0.8),
                };
                let (a, b) = edge_points(me, slot(s), s);
                p.line_segment([a, b], Stroke::new(2.0, color));
                p.circle_filled(a, 3.0, color);
                p.circle_filled(b, 3.0, color);
            }
        }

        // empty slots
        for s in Side::ALL {
            let r = slot(s);
            let occupant = c.at(s).filter(|p| Some(p.id) != dragging);
            if occupant.is_some() {
                continue;
            }
            let hot = hovered_slot == Some(s);
            let color = if hot { ACCENT } else if dragging.is_some() { mix(LINE, ACCENT, 0.35) } else { LINE };
            if hot {
                p.rect_filled(r, 12.0, ACCENT.gamma_multiply(0.10));
            }
            dashed_rect(&p, r.shrink(1.0), color);
            let label = match s {
                Side::Left => "Left",
                Side::Right => "Right",
                Side::Top => "Above",
                Side::Bottom => "Below",
            };
            p.text(r.center(), Align2::CENTER_CENTER, label, regular(13.0), if hot { ACCENT } else { FAINT });
        }
        if let (Some(s), Some(id)) = (hovered_slot, dragging) {
            if let Some(other) = c.at(s).filter(|p| p.id != id) {
                // dropping here moves the current occupant off the layout
                let r = slot(s);
                p.rect_stroke(r, 12.0, Stroke::new(2.0, ACCENT), StrokeKind::Outside);
                let _ = other;
            }
        }

        // this computer
        p.rect_filled(me, 12.0, Color32::from_rgb(0x1d, 0x2a, 0x4d));
        p.rect_stroke(me, 12.0, Stroke::new(1.5, ACCENT), StrokeKind::Inside);
        monitor(&p, Pos2::new(me.center().x, me.top() + size.y * 0.27), (size.y * 0.30).min(30.0), ACCENT);
        let name = std::env::var("COMPUTERNAME").unwrap_or_default();
        let g = fit_text(&p, &name, semibold(14.5), TEXT, size.x - 20.0);
        p.galley(Pos2::new(me.center().x - g.size().x / 2.0, me.top() + size.y * 0.46), g, TEXT);
        p.text(Pos2::new(me.center().x, me.top() + size.y * 0.79), Align2::CENTER_CENTER, "This computer", regular(11.5), WEAK);
        ui.interact(me, Id::new("me-card"), Sense::hover()).on_hover_text(format!("{name}
{}", self.ip));

        // placed devices
        let mut drop: Option<(u64, Option<Side>)> = None;
        for s in Side::ALL {
            if let Some(peer) = c.at(s) {
                self.device_card(ui, peer, slot(s), true);
            }
        }

        ui.add_space(10.0);
        // strip: devices that are paired but not placed, and the add button
        let (strip, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), strip_h), Sense::hover());
        let over_strip = dragging.is_some() && pointer.is_some_and(|pt| strip.contains(pt));
        let sp = ui.painter_at(strip.expand(2.0));
        if dragging.is_some() {
            sp.rect_filled(strip, 12.0, if over_strip { ACCENT.gamma_multiply(0.10) } else { PANEL });
            dashed_rect(&sp, strip.shrink(1.0), if over_strip { ACCENT } else { LINE });
        }
        let chip = Vec2::new(176.0, 58.0);
        let mut x = strip.left() + 10.0;
        let cy = strip.top() + 14.0;
        sp.text(
            Pos2::new(strip.left() + 10.0, strip.top()),
            Align2::LEFT_TOP,
            if dragging.is_some() { "Drop here to take a device off the layout" } else { "NOT PLACED" },
            if dragging.is_some() { regular(11.5) } else { semibold(10.5) },
            FAINT,
        );
        for peer in c.peers.iter().filter(|p| p.side.is_none()) {
            let r = Rect::from_min_size(Pos2::new(x, cy + 8.0), chip);
            if r.right() > strip.right() - 150.0 {
                break;
            }
            self.device_card(ui, peer, r, false);
            x += chip.x + 10.0;
        }
        let add_r = Rect::from_min_size(Pos2::new(x, cy + 8.0), Vec2::new(140.0, chip.y));
        let add = ui.interact(add_r, Id::new("add-device"), Sense::click()).on_hover_cursor(CursorIcon::PointingHand);
        let ac = if add.hovered() { ACCENT } else { mix(WEAK, ACCENT, 0.4) };
        sp.rect_filled(add_r, 10.0, if add.hovered() { ACCENT.gamma_multiply(0.12) } else { Color32::TRANSPARENT });
        dashed_rect(&sp, add_r.shrink(1.0), ac);
        sp.text(add_r.center(), Align2::CENTER_CENTER, "+  Add device", semibold(13.5), ac);
        if add.clicked() {
            self.add = Some(AddDialog::new());
        }

        // finish a drag
        if let Some(d) = &self.drag {
            let released = ui.ctx().input(|i| i.pointer.any_released() || !i.pointer.any_down());
            if released {
                let id = d.id;
                if let Some(s) = hovered_slot {
                    drop = Some((id, Some(s)));
                } else if over_strip {
                    drop = Some((id, None));
                }
                self.drag = None;
            } else if let Some(pt) = pointer {
                // the card follows the pointer on top of everything
                if let Some(peer) = c.peer(d.id) {
                    let r = Rect::from_min_size(pt - d.grab, if peer.side.is_some() { size } else { chip });
                    let fp = ui.ctx().layer_painter(egui::LayerId::new(egui::Order::Tooltip, Id::new("drag-layer")));
                    fp.rect_filled(r.translate(Vec2::new(0.0, 6.0)), 12.0, Color32::from_black_alpha(90));
                    paint_device(&fp, peer, r, true, true);
                }
                ui.ctx().set_cursor_icon(CursorIcon::Grabbing);
            }
        }
        if let Some((id, side)) = drop {
            if config::get().peer(id).map(|p| p.side) != Some(side) {
                net::place(id, side);
                if let Some(p) = config::get().peer(id) {
                    log(match side {
                        Some(s) => format!("{} placed on the {}", p.name, s.name()),
                        None => format!("{} taken off the layout", p.name),
                    });
                }
            }
        }

        // details of the selected device (the row is always there, so nothing jumps)
        ui.add_space(8.0);
        let selected = self.selected.and_then(|id| c.peer(id)).cloned();
        Frame::new()
            .fill(if selected.is_some() { CARD } else { Color32::TRANSPARENT })
            .stroke(Stroke::new(1.0, LINE))
            .corner_radius(10.0)
            .inner_margin(Margin::symmetric(14, 6))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.set_min_height(32.0);
                ui.horizontal_centered(|ui| match &selected {
                    None => {
                        ui.label(
                            RichText::new("Click a device to see its address or forget it.  Ctrl+Alt+F12 switches without the mouse.")
                                .size(12.5)
                                .color(FAINT),
                        );
                    }
                    Some(peer) => {
                        let (st, color) = state_style(net::state(peer.id));
                        ui.label(RichText::new(&peer.name).font(semibold(14.5)));
                        ui.label(RichText::new(format!("{}  ·  ", peer.host)).color(WEAK));
                        ui.label(RichText::new(st).color(color));
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if ui.small_button("✕").on_hover_text("Close").clicked() {
                                self.selected = None;
                            }
                            if ghost_button(ui, "Forget device").clicked() {
                                self.forget = Some(peer.id);
                            }
                            if peer.side.is_some() && ghost_button(ui, "Take off layout").clicked() {
                                net::place(peer.id, None);
                            }
                        });
                    }
                });
            });
    }

    fn device_card(&mut self, ui: &mut egui::Ui, peer: &Peer, r: Rect, big: bool) {
        let resp = ui
            .interact(r, Id::new(("device", peer.id)), Sense::click_and_drag())
            .on_hover_cursor(CursorIcon::Grab);
        if resp.drag_started() {
            if let Some(pt) = resp.interact_pointer_pos() {
                self.drag = Some(Drag { id: peer.id, grab: pt - r.min });
            }
        }
        if resp.clicked() {
            self.selected = if self.selected == Some(peer.id) { None } else { Some(peer.id) };
        }
        let lifted = self.drag.as_ref().is_some_and(|d| d.id == peer.id);
        let p = ui.painter();
        if lifted {
            dashed_rect(p, r.shrink(1.0), LINE);
            return;
        }
        let selected = self.selected == Some(peer.id);
        paint_device(p, peer, r, big, resp.hovered() || selected);
        if selected {
            p.rect_stroke(r, 12.0, Stroke::new(1.5, ACCENT), StrokeKind::Inside);
        }
    }

    // ----- add-device dialog -----

    fn add_dialog(&mut self, ctx: &egui::Context) {
        let Some(d) = self.add.as_mut() else { return };
        if let Some(found) = d.scan.lock().unwrap().take() {
            if d.host.is_empty() {
                if let Some(f) = found.iter().find(|f| config::get().peer(f.id).is_none()) {
                    d.host = f.ip.clone();
                }
            }
            d.found = found;
        }
        if let Some(r) = d.result.lock().unwrap().take() {
            d.busy = false;
            match r {
                Ok(name) => d.done = Some(name),
                Err(e) => d.error = Some(e),
            }
        }
        if let Some(name) = PAIRED.lock().unwrap().take() {
            d.done = Some(name);
        }
        let mut close = false;
        let modal = egui::Modal::new(Id::new("add-dialog"))
            .frame(Frame::new().fill(PANEL).stroke(Stroke::new(1.0, LINE)).corner_radius(14.0).inner_margin(Margin::same(22)))
            .show(ctx, |ui| {
                ui.set_width(460.0);
                ui.set_min_height(400.0); // the dialog keeps its size when switching modes
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Add a device").font(semibold(18.0)));
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui.add(egui::Button::new(RichText::new("✕").size(14.0)).frame(false)).clicked() {
                            close = true;
                        }
                    });
                });
                ui.add_space(2.0);
                if let Some(name) = &d.done {
                    ui.add_space(14.0);
                    ui.vertical_centered(|ui| {
                        ui.label(RichText::new("✓").size(40.0).color(GREEN));
                        ui.label(RichText::new(format!("Paired with {name}")).font(semibold(16.0)));
                        ui.label(
                            RichText::new("It is now on the layout. Drag it to the side where it actually stands.")
                                .size(13.0)
                                .color(WEAK),
                        );
                        ui.add_space(12.0);
                        if primary_button(ui, "   Done   ", true).clicked() {
                            close = true;
                        }
                    });
                    return;
                }
                ui.label(
                    RichText::new("Open KVM Link on the other computer too. One of the two shows a PIN, the other one types it in.")
                        .size(13.0)
                        .color(WEAK),
                );
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    for (m, name) in [(AddMode::Enter, "Enter a PIN"), (AddMode::Show, "Show a PIN")] {
                        let sel = d.mode == m;
                        let b = egui::Button::new(RichText::new(name).font(if sel { semibold(13.5) } else { regular(13.5) }))
                            .fill(if sel { CARD_HI } else { Color32::TRANSPARENT })
                            .stroke(Stroke::new(1.0, if sel { LINE } else { Color32::TRANSPARENT }))
                            .corner_radius(8.0)
                            .min_size(Vec2::new(120.0, 30.0));
                        if ui.add(b).clicked() && !sel {
                            d.mode = m;
                            d.error = None;
                            if m == AddMode::Enter {
                                net::close_pairing();
                            }
                        }
                    }
                });
                ui.add_space(10.0);
                match d.mode {
                    AddMode::Enter => enter_pin(ui, d),
                    AddMode::Show => show_pin(ui),
                }
            });
        if modal.should_close() {
            close = true;
        }
        if close {
            net::close_pairing();
            self.add = None;
        }
    }

    // ----- settings tab -----

    fn settings_tab(&mut self, ui: &mut egui::Ui, c: &config::Cfg) {
        if let Some(ok) = self.service_result.lock().unwrap().take() {
            self.service = ok;
            if ok {
                self.autostart = false;
            }
        }
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            section(ui, "Sharing");
            card(ui, |ui| {
                let mut clip_on = c.clip;
                if setting(ui, "Shared clipboard", "Text and images up to 16 MB, synced with every connected device.", &mut clip_on, true) {
                    config::update(|c| c.clip = clip_on);
                    clip::set_enabled(clip_on);
                }
                ui.separator();
                let mut block = c.block;
                if setting(
                    ui,
                    "Ignore this computer's keyboard",
                    "For a faulty built-in keyboard: its keys do nothing while another device is connected.",
                    &mut block,
                    true,
                ) {
                    config::update(|c| c.block = block);
                    net::refresh();
                }
            });
            section(ui, "Startup");
            card(ui, |ui| {
                let mut auto = self.autostart;
                if setting(ui, "Start with Windows", "Starts hidden in the tray when you sign in.", &mut auto, !self.service) {
                    setup::set_autostart(auto);
                    self.autostart = setup::autostart_enabled();
                }
                ui.separator();
                let busy = self.service_busy.load(Relaxed);
                let mut svc = self.service;
                if setting(
                    ui,
                    "Service mode",
                    "Keeps working in administrator windows and UAC prompts. Asks for administrator approval once.",
                    &mut svc,
                    !busy,
                ) {
                    self.service_busy.store(true, Relaxed);
                    let (flag, slot) = (self.service_busy.clone(), self.service_result.clone());
                    std::thread::spawn(move || {
                        *slot.lock().unwrap() = Some(apply_service(svc));
                        flag.store(false, Relaxed);
                        notify();
                    });
                }
            });
            section(ui, "Network");
            card(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.vertical(|ui| {
                        ui.set_width(ui.available_width() - 190.0);
                        ui.label(RichText::new("Port").font(semibold(14.5)));
                        ui.label(RichText::new("TCP and UDP. Must be the same on every device.").size(12.5).color(WEAK));
                    });
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let parsed = self.port_text.trim().parse::<u16>().ok().filter(|p| *p > 0 && *p < 65535);
                        let changed = parsed.is_some_and(|p| p != c.port);
                        if ui.add_enabled(changed, egui::Button::new("Apply")).clicked() {
                            let port = parsed.unwrap();
                            let was = net::running();
                            net::stop();
                            config::update(|c| c.port = port);
                            std::thread::spawn(move || {
                                if !setup::ensure_firewall(port) {
                                    log("The firewall port was not opened: other devices may not reach this one");
                                }
                            });
                            if was {
                                if let Err(e) = net::start() {
                                    log(e);
                                }
                            }
                            log(format!("Port changed to {port}"));
                        }
                        ui.add(egui::TextEdit::singleline(&mut self.port_text).desired_width(70.0).char_limit(5));
                    });
                });
                ui.separator();
                ui.horizontal(|ui| {
                    ui.label(RichText::new("This computer").font(semibold(14.5)));
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.label(
                            RichText::new(format!(
                                "{}  ·  {}  ·  ID {:016x}",
                                std::env::var("COMPUTERNAME").unwrap_or_default(),
                                self.ip,
                                c.id
                            ))
                            .size(12.5)
                            .color(WEAK),
                        );
                    });
                });
            });
            section(ui, "Shortcut");
            card(ui, |ui| {
                ui.horizontal(|ui| {
                    Frame::new()
                        .fill(CARD_HI)
                        .stroke(Stroke::new(1.0, LINE))
                        .corner_radius(6.0)
                        .inner_margin(Margin::symmetric(8, 3))
                        .show(ui, |ui| ui.label(RichText::new("Ctrl + Alt + F12").font(semibold(13.0))));
                    ui.label(
                        RichText::new("Moves the keyboard and mouse to the next connected device, and finally back here.")
                            .size(13.0)
                            .color(WEAK),
                    );
                });
            });
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.label(RichText::new(format!("KVM Link {}", env!("CARGO_PKG_VERSION"))).size(12.0).color(FAINT));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui.add(egui::Button::new(RichText::new("Exit KVM Link").size(12.5).color(RED)).frame(false)).clicked() {
                        tray::exit();
                    }
                });
            });
        });
    }

    // ----- activity tab -----

    fn activity_tab(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label(RichText::new("Connections, pairing and errors from this session.").size(13.0).color(WEAK));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ghost_button(ui, "Clear").clicked() {
                    LOG.lock().unwrap().clear();
                }
            });
        });
        let lines = LOG.lock().unwrap().clone();
        Frame::new()
            .fill(PANEL)
            .stroke(Stroke::new(1.0, LINE))
            .corner_radius(12.0)
            .inner_margin(Margin::symmetric(14, 10))
            .show(ui, |ui| {
                egui::ScrollArea::vertical().auto_shrink([false, false]).stick_to_bottom(true).show(ui, |ui| {
                    ui.spacing_mut().interact_size.y = 18.0;
                    ui.spacing_mut().item_spacing.y = 6.0;
                    if lines.is_empty() {
                        ui.label(RichText::new("Nothing yet.").color(FAINT));
                    }
                    for (t, l) in &lines {
                        ui.horizontal_wrapped(|ui| {
                            ui.label(RichText::new(t).monospace().color(FAINT));
                            ui.add(egui::Label::new(RichText::new(l).size(13.5)).wrap_mode(TextWrapMode::Wrap));
                        });
                    }
                });
            });
    }

    fn forget_dialog(&mut self, ctx: &egui::Context) {
        let Some(id) = self.forget else { return };
        let name = config::get().peer(id).map(|p| p.name.clone()).unwrap_or_default();
        let mut close = false;
        let m = egui::Modal::new(Id::new("forget-dialog"))
            .frame(Frame::new().fill(PANEL).stroke(Stroke::new(1.0, LINE)).corner_radius(14.0).inner_margin(Margin::same(22)))
            .show(ctx, |ui| {
                ui.set_width(360.0);
                ui.label(RichText::new(format!("Forget {name}?")).font(semibold(17.0)));
                ui.label(
                    RichText::new("The pairing is removed on both computers. To use it again, pair it once more.")
                        .size(13.0)
                        .color(WEAK),
                );
                ui.add_space(12.0);
                ui.horizontal(|ui| {
                    let b = egui::Button::new(RichText::new("Forget").font(semibold(14.0)).color(Color32::WHITE))
                        .fill(RED)
                        .min_size(Vec2::new(90.0, 34.0));
                    if ui.add(b).clicked() {
                        net::forget(id);
                        log(format!("Forgot {name}"));
                        self.selected = None;
                        close = true;
                    }
                    if ghost_button(ui, "Cancel").clicked() {
                        close = true;
                    }
                });
            });
        if m.should_close() || close {
            self.forget = None;
        }
    }
}

fn edge_points(me: Rect, other: Rect, s: Side) -> (Pos2, Pos2) {
    match s {
        Side::Left => (me.left_center(), other.right_center()),
        Side::Right => (me.right_center(), other.left_center()),
        Side::Top => (me.center_top(), other.center_bottom()),
        Side::Bottom => (me.center_bottom(), other.center_top()),
    }
}

/// A device tile: big ones sit around this computer, small ones wait in the strip below.
fn paint_device(p: &egui::Painter, peer: &Peer, r: Rect, big: bool, hot: bool) {
    let st = net::state(peer.id);
    let (label, color) = state_style(st);
    p.rect_filled(r, 12.0, if hot { CARD_HI } else { CARD });
    let edge = if st == State::Offline { LINE } else { color.gamma_multiply(0.55) };
    p.rect_stroke(r, 12.0, Stroke::new(1.0, edge), StrokeKind::Inside);
    if big {
        let icon = if st == State::Offline { WEAK } else { TEXT };
        monitor(p, Pos2::new(r.center().x, r.top() + r.height() * 0.27), (r.height() * 0.30).min(30.0), icon);
        let g = fit_text(p, &peer.name, semibold(14.5), TEXT, r.width() - 20.0);
        p.galley(Pos2::new(r.center().x - g.size().x / 2.0, r.top() + r.height() * 0.46), g, TEXT);
        let g = fit_text(p, label, regular(11.5), color, r.width() - 34.0);
        let w = g.size().x + 12.0;
        let base = Pos2::new(r.center().x - w / 2.0, r.top() + r.height() * 0.79 - g.size().y / 2.0);
        p.circle_filled(Pos2::new(base.x + 3.0, base.y + g.size().y / 2.0), 3.5, color);
        p.galley(Pos2::new(base.x + 12.0, base.y), g, color);
    } else {
        monitor(p, Pos2::new(r.left() + 24.0, r.center().y), 22.0, if st == State::Offline { WEAK } else { TEXT });
        let g = fit_text(p, &peer.name, semibold(13.5), TEXT, r.width() - 56.0);
        p.galley(Pos2::new(r.left() + 46.0, r.top() + 10.0), g, TEXT);
        p.circle_filled(Pos2::new(r.left() + 50.0, r.bottom() - 17.0), 3.5, color);
        let g = fit_text(p, label, regular(11.5), color, r.width() - 66.0);
        p.galley(Pos2::new(r.left() + 58.0, r.bottom() - 17.0 - g.size().y / 2.0), g, color);
    }
}

fn enter_pin(ui: &mut egui::Ui, d: &mut AddDialog) {
    ui.horizontal(|ui| {
        ui.label(RichText::new("Computers on your network").font(semibold(13.5)));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if d.scanning.load(Relaxed) {
                ui.add(egui::Spinner::new().size(14.0));
            } else if ui.small_button("Scan again").clicked() {
                d.rescan();
            }
        });
    });
    Frame::new()
        .fill(BG)
        .stroke(Stroke::new(1.0, LINE))
        .corner_radius(10.0)
        .inner_margin(Margin::same(6))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.set_min_height(44.0);
            if d.found.is_empty() {
                ui.add_space(6.0);
                ui.label(
                    RichText::new(if d.scanning.load(Relaxed) {
                        "  Looking…"
                    } else {
                        "  Nothing found. Make sure KVM Link runs there and both are on the same network, or type the address below."
                    })
                    .size(12.5)
                    .color(FAINT),
                );
            }
            egui::ScrollArea::vertical().max_height(130.0).show(ui, |ui| {
                for f in d.found.clone() {
                    let sel = d.host == f.ip;
                    let known = config::get().peer(f.id).is_some();
                    let (r, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 38.0), Sense::click());
                    let p = ui.painter();
                    if sel || resp.hovered() {
                        p.rect_filled(r, 8.0, if sel { ACCENT.gamma_multiply(0.18) } else { CARD });
                    }
                    monitor(p, Pos2::new(r.left() + 20.0, r.center().y), 18.0, if sel { ACCENT } else { WEAK });
                    p.text(Pos2::new(r.left() + 40.0, r.center().y), Align2::LEFT_CENTER, &f.name, semibold(13.5), TEXT);
                    let right = if known { format!("paired  ·  {}", f.ip) } else { f.ip.clone() };
                    p.text(Pos2::new(r.right() - 10.0, r.center().y), Align2::RIGHT_CENTER, right, regular(12.0), WEAK);
                    if resp.on_hover_cursor(CursorIcon::PointingHand).clicked() {
                        d.host = f.ip.clone();
                        d.error = None;
                    }
                }
            });
        });
    ui.add_space(8.0);
    egui::Grid::new("pair-grid").num_columns(2).spacing([12.0, 10.0]).show(ui, |ui| {
        ui.label(RichText::new("Address").color(WEAK));
        ui.add(egui::TextEdit::singleline(&mut d.host).hint_text("192.168.1.20").desired_width(260.0));
        ui.end_row();
        ui.label(RichText::new("PIN").color(WEAK));
        let r = ui.add(
            egui::TextEdit::singleline(&mut d.pin)
                .hint_text("6 digits from the other screen")
                .font(semibold(16.0))
                .char_limit(7)
                .desired_width(260.0),
        );
        if r.changed() {
            d.pin.retain(|c| c.is_ascii_digit());
            d.error = None;
        }
        ui.end_row();
    });
    if let Some(e) = &d.error {
        ui.add_space(4.0);
        ui.label(RichText::new(e).size(13.0).color(RED));
    }
    ui.add_space(10.0);
    ui.horizontal(|ui| {
        let ready = !d.busy && d.pin.len() == 6 && !d.host.trim().is_empty();
        if primary_button(ui, if d.busy { "  Pairing…  " } else { "  Pair  " }, ready).clicked() {
            d.busy = true;
            d.error = None;
            let (host, pin, slot) = (d.host.trim().to_string(), d.pin.clone(), d.result.clone());
            std::thread::spawn(move || {
                *slot.lock().unwrap() = Some(net::pair_with(&host, &pin));
                notify();
            });
        }
        if d.busy {
            ui.add(egui::Spinner::new().size(16.0));
        }
    });
}

fn show_pin(ui: &mut egui::Ui) {
    match net::pairing_state() {
        None => {
            ui.label(
                RichText::new("This computer shows a 6-digit PIN for two minutes. Type it on the other computer under Enter a PIN.")
                    .size(13.0)
                    .color(WEAK),
            );
            ui.add_space(10.0);
            if !net::running() {
                ui.label(RichText::new("Sharing is paused: turn it on first (switch at the top right).").color(AMBER));
            } else if primary_button(ui, "  Show PIN  ", true).clicked() {
                let pin = net::open_pairing();
                log(format!("Pairing open, PIN {} {}", &pin[..3], &pin[3..]));
            }
        }
        Some((pin, left)) => {
            ui.vertical_centered(|ui| {
                ui.add_space(6.0);
                ui.label(RichText::new("Type this PIN on the other computer").size(13.0).color(WEAK));
                ui.add_space(4.0);
                ui.label(RichText::new(format!("{}  {}", &pin[..3], &pin[3..])).font(semibold(44.0)).color(TEXT));
                let frac = left as f32 / 120.0;
                let (r, _) = ui.allocate_exact_size(Vec2::new(220.0, 4.0), Sense::hover());
                ui.painter().rect_filled(r, 2.0, LINE);
                ui.painter().rect_filled(Rect::from_min_size(r.min, Vec2::new(r.width() * frac, r.height())), 2.0, ACCENT);
                ui.label(RichText::new(format!("Expires in {}:{:02}", left / 60, left % 60)).size(12.5).color(FAINT));
                ui.add_space(4.0);
                ui.label(RichText::new("Waiting for the other computer…").size(13.0).color(WEAK));
                ui.add_space(4.0);
                if ghost_button(ui, "Cancel").clicked() {
                    net::close_pairing();
                }
            });
        }
    }
}

/// Runs on a worker thread: the elevated call blocks until the user answers UAC.
/// Returns whether service mode is on afterwards.
fn apply_service(on: bool) -> bool {
    let restart = on && !setup::is_system();
    let res = setup::set_service(on, restart);
    let now = if res.is_ok() { on } else { !on };
    config::update(|c| c.service = now);
    if now {
        setup::set_autostart(false);
    }
    match res {
        Ok(()) => log(if on { "Service mode on" } else { "Service mode off (takes effect next time KVM Link starts)" }),
        Err(e) => {
            log(format!("Could not change service mode: {e}"));
            return now;
        }
    }
    if restart {
        log("Restarting in service mode…");
        std::thread::sleep(Duration::from_millis(300));
        tray::exit();
    }
    now
}

impl eframe::App for App {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if self.start_hidden {
            if self.frames >= 1 {
                self.start_hidden = false;
                tray::hide_app_centered();
            } else {
                ctx.request_repaint();
            }
        }
        // The cross hides the window to the tray; Exit in the tray menu quits.
        if ctx.input(|i| i.viewport().close_requested()) {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            tray::hide_app();
        }
        if tray::app_visible() {
            ctx.request_repaint_after(Duration::from_millis(500));
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.frames = self.frames.saturating_add(1);
        let c = config::get();
        if let Some(id) = self.selected {
            if c.peer(id).is_none() {
                self.selected = None;
            }
        }
        egui::Panel::top("header")
            .frame(Frame::new().fill(BG).inner_margin(Margin { left: 22, right: 22, top: 16, bottom: 6 }))
            .show_separator_line(false)
            .show(ui, |ui| self.header(ui, &c));
        egui::CentralPanel::default()
            .frame(Frame::new().fill(BG).inner_margin(Margin { left: 22, right: 22, top: 12, bottom: 18 }))
            .show(ui, |ui| {
                let r = ui.max_rect();
                ui.painter().line_segment(
                    [Pos2::new(r.left() - 22.0, r.top() - 12.0), Pos2::new(r.right() + 22.0, r.top() - 12.0)],
                    Stroke::new(1.0, LINE),
                );
                match self.tab {
                    Tab::Layout => self.layout_tab(ui, &c),
                    Tab::Settings => self.settings_tab(ui, &c),
                    Tab::Activity => self.activity_tab(ui),
                }
            });
        let ctx = ui.ctx().clone();
        self.add_dialog(&ctx);
        self.forget_dialog(&ctx);
    }

    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        BG.to_normalized_gamma_f32()
    }
}

pub fn run(autostart: bool) {
    let c = config::load();
    clip::set_enabled(c.clip);
    tray::start();
    let port = c.port;
    std::thread::spawn(move || {
        if !setup::ensure_firewall(port) {
            log("The firewall port was not opened: other devices may not reach this one");
        }
    });
    match net::start() {
        Ok(()) => log(format!("Sharing is on (port {port})")),
        Err(e) => log(e),
    }
    if c.peers.is_empty() {
        log("No paired devices yet: click Add device");
    }

    let title = if setup::is_system() {
        "KVM Link (service)"
    } else if setup::is_admin() {
        "KVM Link (administrator)"
    } else {
        "KVM Link"
    };
    let mut viewport = egui::ViewportBuilder::default()
        .with_title(title)
        .with_inner_size([780.0, 640.0])
        .with_min_inner_size([680.0, 560.0])
        // An empty icon keeps eframe's default away; the real one comes from the exe resources.
        .with_icon(egui::IconData::default());
    if autostart {
        viewport = viewport.with_position([-32000.0, -32000.0]);
    }
    let options = eframe::NativeOptions { viewport, centered: !autostart, ..Default::default() };
    let res = eframe::run_native(title, options, Box::new(move |cc| Ok(Box::new(App::new(cc, autostart)))));
    if let Err(e) = res {
        // No usable OpenGL (remote desktop, basic display driver): keep working from the tray.
        log(format!("The window could not be opened: {e}"));
        loop {
            std::thread::park();
        }
    }
    tray::exit();
}
