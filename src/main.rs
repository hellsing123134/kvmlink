#![windows_subsystem = "windows"]

mod clip;
mod config;
mod net;
mod proto;
mod setup;
mod svc;
mod sys;
mod tray;
mod ui;

fn main() {
    let autostart = std::env::args().any(|a| a == "--autostart");
    if std::env::args().any(|a| a == "--launcher") {
        // Started by the SYSTEM logon task in session 0: hand over to a SYSTEM copy in the user's session.
        svc::launch_in_user_session("--autostart");
        return;
    }
    if sys::already_running() {
        sys::show_existing();
        return;
    }
    sys::init_process();
    ui::run(autostart);
}
