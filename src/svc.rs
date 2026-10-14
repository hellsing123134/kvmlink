//! Service mode: a SYSTEM logon task starts `kvmlink --launcher` in session 0; the launcher
//! starts a SYSTEM copy of the app on the interactive desktop of the logged-in user.
//! Only a SYSTEM process can inject input into UAC prompts and the lock screen.

use windows_sys::Win32::Foundation::CloseHandle;
use windows_sys::Win32::Security::{DuplicateTokenEx, TokenPrimary, SecurityImpersonation};
use windows_sys::Win32::Security::TOKEN_DUPLICATE;
use windows_sys::Win32::System::Diagnostics::ToolHelp::*;
use windows_sys::Win32::System::RemoteDesktop::{ProcessIdToSessionId, WTSGetActiveConsoleSessionId};
use windows_sys::Win32::System::Threading::*;

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

fn winlogon_pid(session: u32) -> Option<u32> {
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        let mut pe: PROCESSENTRY32W = std::mem::zeroed();
        pe.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        let mut found = None;
        if Process32FirstW(snap, &mut pe) != 0 {
            loop {
                let len = pe.szExeFile.iter().position(|&c| c == 0).unwrap_or(pe.szExeFile.len());
                let name = String::from_utf16_lossy(&pe.szExeFile[..len]);
                if name.eq_ignore_ascii_case("winlogon.exe") {
                    let mut s = 0u32;
                    if ProcessIdToSessionId(pe.th32ProcessID, &mut s) != 0 && s == session {
                        found = Some(pe.th32ProcessID);
                        break;
                    }
                }
                if Process32NextW(snap, &mut pe) == 0 {
                    break;
                }
            }
        }
        CloseHandle(snap);
        found
    }
}

/// Starts this exe with `args` as SYSTEM on the active console session's default desktop.
pub fn launch_in_user_session(args: &str) -> bool {
    unsafe {
        let session = WTSGetActiveConsoleSessionId();
        if session == 0xFFFF_FFFF {
            return false;
        }
        let Some(pid) = winlogon_pid(session) else { return false };
        let proc = OpenProcess(PROCESS_QUERY_INFORMATION, 0, pid);
        if proc.is_null() {
            return false;
        }
        let mut token = std::ptr::null_mut();
        let ok = OpenProcessToken(proc, TOKEN_DUPLICATE, &mut token);
        CloseHandle(proc);
        if ok == 0 {
            return false;
        }
        let mut primary = std::ptr::null_mut();
        let ok = DuplicateTokenEx(token, 0x0200_0000 /*MAXIMUM_ALLOWED*/, std::ptr::null(), SecurityImpersonation, TokenPrimary, &mut primary);
        CloseHandle(token);
        if ok == 0 {
            return false;
        }

        let exe = std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_default();
        let mut cmd = wide(&format!("\"{exe}\" {args}"));
        let mut desktop = wide("winsta0\\default");
        let mut si: STARTUPINFOW = std::mem::zeroed();
        si.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
        si.lpDesktop = desktop.as_mut_ptr();
        let mut pi: PROCESS_INFORMATION = std::mem::zeroed();
        let ok = CreateProcessAsUserW(
            primary,
            std::ptr::null(),
            cmd.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            0,
            0,
            std::ptr::null(),
            std::ptr::null(),
            &si,
            &mut pi,
        );
        CloseHandle(primary);
        if ok == 0 {
            return false;
        }
        CloseHandle(pi.hThread);
        CloseHandle(pi.hProcess);
        true
    }
}
