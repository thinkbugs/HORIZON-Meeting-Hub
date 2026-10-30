//! Windows Defender Firewall rule for the file-transfer listener.
//!
//! HORIZON installs per-user, so the installer never runs elevated and cannot
//! create the rule itself. Without one, the public profile blocks inbound TCP
//! and a phone on the same Wi-Fi cannot open the address the panel shows — the
//! page works locally and nowhere else.

pub const RULE_NAME: &str = "HORIZON Meeting SnapDrop";

#[cfg(windows)]
mod imp {
    use super::RULE_NAME;
    use std::process::Command;

    use windows::core::{HSTRING, PCWSTR};
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{GetExitCodeProcess, WaitForSingleObject, INFINITE};
    use windows::Win32::UI::Shell::{ShellExecuteExW, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW};

    const ERROR_CANCELLED_CODE: i32 = 1223;

    /// `netsh` exits non-zero when no rule matches, which is exactly the
    /// not-allowed case.
    pub fn is_allowed() -> bool {
        Command::new("netsh")
            .args([
                "advfirewall",
                "firewall",
                "show",
                "rule",
                &format!("name={RULE_NAME}"),
            ])
            .output()
            .map(|output| output.status.success())
            .unwrap_or(false)
    }

    /// Returns the `netsh` exit code. Raises the usual UAC dialog; declining it
    /// surfaces as an error instead of a silent no-op.
    pub fn allow() -> Result<i32, String> {
        let exe = std::env::current_exe().map_err(|error| format!("无法定位程序路径：{error}"))?;
        let params = format!(
            "advfirewall firewall add rule name=\"{RULE_NAME}\" dir=in action=allow program=\"{}\" enable=yes profile=any",
            exe.to_string_lossy()
        );
        run_elevated("netsh.exe", &params)
    }

    fn run_elevated(file: &str, params: &str) -> Result<i32, String> {
        // ShellExecuteEx runs on a blocking pool thread with no COM apartment of
        // its own; RPC_E_CHANGED_MODE means someone already set one up.
        use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
        unsafe {
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        }
        let verb = HSTRING::from("runas");
        let file = HSTRING::from(file);
        let params = HSTRING::from(params);
        let mut info = SHELLEXECUTEINFOW {
            cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
            fMask: SEE_MASK_NOCLOSEPROCESS,
            lpVerb: PCWSTR(verb.as_ptr()),
            lpFile: PCWSTR(file.as_ptr()),
            lpParameters: PCWSTR(params.as_ptr()),
            nShow: 0,
            ..Default::default()
        };
        unsafe {
            if let Err(error) = ShellExecuteExW(&mut info) {
                return Err(if error.code().0 & 0xFFFF == ERROR_CANCELLED_CODE {
                    "已取消管理员授权".to_string()
                } else {
                    format!("无法请求管理员权限：{error}")
                });
            }
            if info.hProcess.is_invalid() {
                return Ok(0);
            }
            WaitForSingleObject(info.hProcess, INFINITE);
            let mut code = 1u32;
            let _ = GetExitCodeProcess(info.hProcess, &mut code);
            let _ = CloseHandle(info.hProcess);
            Ok(code as i32)
        }
    }
}

#[cfg(windows)]
pub fn is_allowed() -> bool {
    imp::is_allowed()
}

#[cfg(windows)]
pub fn allow() -> Result<i32, String> {
    imp::allow()
}

#[cfg(not(windows))]
pub fn is_allowed() -> bool {
    true
}

#[cfg(not(windows))]
pub fn allow() -> Result<i32, String> {
    Ok(0)
}
