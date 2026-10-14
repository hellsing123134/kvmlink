//! System integration: firewall rules, autostart, and the service-mode logon task.

use std::os::windows::process::CommandExt;
use std::path::Path;
use std::process::Command;
use windows_sys::Win32::Foundation::CloseHandle;
use windows_sys::Win32::System::Threading::{GetExitCodeProcess, WaitForSingleObject};
use windows_sys::Win32::UI::Shell::{IsUserAnAdmin, ShellExecuteExW, SHELLEXECUTEINFOW};
use windows_sys::Win32::UI::WindowsAndMessaging::SW_HIDE;

pub const NO_WINDOW: u32 = 0x0800_0000;
const RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
const TASK: &str = "kvmlink";

fn w(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

fn exe_path() -> String {
    std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_default()
}

fn quiet(cmd: &str, args: &[&str]) -> bool {
    Command::new(cmd)
        .args(args)
        .creation_flags(NO_WINDOW)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

pub fn is_admin() -> bool {
    unsafe { IsUserAnAdmin() != 0 }
}

/// True when this copy runs as SYSTEM (started by the logon task through the launcher).
pub fn is_system() -> bool {
    std::env::var("USERNAME").map(|u| u.ends_with('$')).unwrap_or(false)
}

/// Runs a program elevated (one UAC prompt) and waits for it. True if it exited with 0.
pub fn run_elevated(file: &str, params: &str) -> bool {
    unsafe {
        let (verb, file, params) = (w("runas"), w(file), w(params));
        let mut sei: SHELLEXECUTEINFOW = std::mem::zeroed();
        sei.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
        sei.fMask = 0x40; // SEE_MASK_NOCLOSEPROCESS
        sei.lpVerb = verb.as_ptr();
        sei.lpFile = file.as_ptr();
        sei.lpParameters = params.as_ptr();
        sei.nShow = SW_HIDE;
        if ShellExecuteExW(&mut sei) == 0 {
            return false;
        }
        if sei.hProcess.is_null() {
            return true;
        }
        WaitForSingleObject(sei.hProcess, 60_000);
        let mut code = 1u32;
        GetExitCodeProcess(sei.hProcess, &mut code);
        CloseHandle(sei.hProcess);
        code == 0
    }
}

// ---------- firewall ----------

fn rule_exists(name: &str) -> bool {
    quiet("netsh", &["advfirewall", "firewall", "show", "rule", &format!("name={name}")])
}

/// Opens TCP port and port+1 (clipboard) and UDP port (discovery) for the local subnet only.
/// One UAC prompt, only the first time. Returns false if the user declined.
pub fn ensure_firewall(port: u16) -> bool {
    if std::env::var_os("KVMLINK_INSTANCE").is_some() {
        return true; // a test copy talks over loopback only
    }
    let tcp = format!("KVM Link TCP {port}");
    let udp = format!("KVM Link UDP {port}");
    if rule_exists(&tcp) && rule_exists(&udp) {
        return true;
    }
    let p1 = port.saturating_add(1);
    // profile=any: home networks are often marked "Public"; remoteip=localsubnet keeps the internet out.
    let cmd = format!(
        "/C netsh advfirewall firewall delete rule name=\"{tcp}\" & netsh advfirewall firewall delete rule name=\"{udp}\" \
         & netsh advfirewall firewall delete rule name=kvmlink-{port} & netsh advfirewall firewall delete rule name=kvmlink2-{port} \
         & netsh advfirewall firewall add rule name=\"{tcp}\" dir=in action=allow protocol=TCP localport={port},{p1} remoteip=localsubnet profile=any \
         && netsh advfirewall firewall add rule name=\"{udp}\" dir=in action=allow protocol=UDP localport={port} remoteip=localsubnet profile=any"
    );
    run_elevated("cmd", &cmd)
}

// ---------- autostart ----------

pub fn set_autostart(on: bool) {
    if on {
        let value = format!("\"{}\" --autostart", exe_path());
        quiet("reg", &["add", RUN_KEY, "/v", "kvmlink", "/t", "REG_SZ", "/d", &value, "/f"]);
    } else {
        quiet("reg", &["delete", RUN_KEY, "/v", "kvmlink", "/f"]);
    }
}

pub fn autostart_enabled() -> bool {
    quiet("reg", &["query", RUN_KEY, "/v", "kvmlink"])
}

// ---------- service mode ----------

pub fn service_enabled() -> bool {
    // Standard users cannot query a SYSTEM task, so also look for its file.
    let root = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
    Path::new(&root).join("System32").join("Tasks").join(TASK).exists() || quiet("schtasks", &["/Query", "/TN", TASK])
}

/// Console output of the elevated command (OEM code page 866 on Russian Windows), on one line.
fn read_oem(path: &Path) -> String {
    let bytes = std::fs::read(path).unwrap_or_default();
    let t: String = bytes
        .iter()
        .map(|&b| match b {
            0x80..=0xAF => char::from_u32(0x0410 + (b as u32 - 0x80)).unwrap(),
            0xE0..=0xEF => char::from_u32(0x0440 + (b as u32 - 0xE0)).unwrap(),
            _ => b as char,
        })
        .collect();
    t.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// Logon task that runs `--launcher` as SYSTEM; the launcher starts the app as SYSTEM in the
/// user's session. Only a SYSTEM process may inject input into UAC prompts and the lock screen.
/// With `restart`, the task is also started once this copy has exited.
pub fn set_service(on: bool, restart: bool) -> Result<(), String> {
    let tmp = std::env::temp_dir();
    let logf = tmp.join("kvmlink-svc.log");
    let _ = std::fs::remove_file(&logf);
    let redirect = format!("> \"{}\" 2>&1", logf.display());
    let fail = |logf: &Path| {
        let d = read_oem(logf);
        let _ = std::fs::remove_file(logf);
        if d.is_empty() {
            "the administrator prompt was declined".to_string()
        } else {
            d
        }
    };
    if !on {
        let cmd = format!("/C schtasks /Delete /TN {TASK} /F {redirect}");
        return if run_elevated("cmd", &cmd) { Ok(()) } else { Err(fail(&logf)) };
    }
    let exe = xml_escape(&exe_path());
    let xml = format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo><Description>KVM Link service mode</Description></RegistrationInfo>
  <Triggers><LogonTrigger><Enabled>true</Enabled></LogonTrigger></Triggers>
  <Principals><Principal id="Author"><UserId>S-1-5-18</UserId><RunLevel>HighestAvailable</RunLevel></Principal></Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <AllowStartOnDemand>true</AllowStartOnDemand>
    <Enabled>true</Enabled>
  </Settings>
  <Actions Context="Author"><Exec><Command>{exe}</Command><Arguments>--launcher</Arguments></Exec></Actions>
</Task>"#
    );
    let path = tmp.join("kvmlink-task.xml");
    let mut bytes = vec![0xFF, 0xFE];
    for u in xml.encode_utf16() {
        bytes.extend_from_slice(&u.to_le_bytes());
    }
    std::fs::write(&path, bytes).map_err(|e| e.to_string())?;
    let create = format!("schtasks /Create /TN {TASK} /XML \"{}\" /F {redirect}", path.display());
    let cmd = if restart {
        format!("/C {create} && start \"\" /B cmd /C \"ping -n 4 127.0.0.1 >nul & schtasks /Run /TN {TASK}\"")
    } else {
        format!("/C {create}")
    };
    let ok = run_elevated("cmd", &cmd);
    let _ = std::fs::remove_file(path);
    if ok {
        Ok(())
    } else {
        Err(fail(&logf))
    }
}
