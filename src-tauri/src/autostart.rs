//! Login autostart through `HKCU\...\CurrentVersion\Run`.
//!
//! The registry is the only source of truth: a value the user deleted by hand
//! disappears from the toggle by itself, with no app-side copy to drift.

use windows::core::HSTRING;
use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
use windows::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW,
    HKEY, HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE, REG_OPTION_NON_VOLATILE, REG_SZ,
};

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const VALUE_NAME: &str = "HORIZON Meeting";

fn command_line() -> Result<String, String> {
    let exe = std::env::current_exe().map_err(|error| format!("无法定位程序路径：{error}"))?;
    Ok(format!("\"{}\"", exe.to_string_lossy()))
}

pub fn is_enabled() -> bool {
    unsafe {
        let mut key = HKEY::default();
        if RegOpenKeyExW(
            HKEY_CURRENT_USER,
            &HSTRING::from(RUN_KEY),
            Some(0),
            KEY_QUERY_VALUE,
            &mut key,
        ) != ERROR_SUCCESS
        {
            return false;
        }
        let mut size: u32 = 0;
        let status = RegQueryValueExW(
            key,
            &HSTRING::from(VALUE_NAME),
            None,
            None,
            None,
            Some(&mut size),
        );
        let _ = RegCloseKey(key);
        status == ERROR_SUCCESS && size > 0
    }
}

pub fn set_enabled(enabled: bool) -> Result<(), String> {
    unsafe {
        if !enabled {
            let mut key = HKEY::default();
            if RegOpenKeyExW(
                HKEY_CURRENT_USER,
                &HSTRING::from(RUN_KEY),
                Some(0),
                KEY_SET_VALUE,
                &mut key,
            ) != ERROR_SUCCESS
            {
                return Ok(());
            }
            let status = RegDeleteValueW(key, &HSTRING::from(VALUE_NAME));
            let _ = RegCloseKey(key);
            // Nothing registered is the state being asked for.
            if status == ERROR_SUCCESS || status == ERROR_FILE_NOT_FOUND {
                return Ok(());
            }
            return Err(format!("无法删除开机启动项：{}", status.0));
        }

        let mut key = HKEY::default();
        let status = RegCreateKeyExW(
            HKEY_CURRENT_USER,
            &HSTRING::from(RUN_KEY),
            None,
            None,
            REG_OPTION_NON_VOLATILE,
            KEY_SET_VALUE,
            None,
            &mut key,
            None,
        );
        if status != ERROR_SUCCESS {
            return Err(format!("无法打开开机启动项：{}", status.0));
        }

        let command = command_line()?;
        // REG_SZ is UTF-16; the size includes the terminator.
        let wide: Vec<u16> = command.encode_utf16().chain(std::iter::once(0)).collect();
        let bytes = std::slice::from_raw_parts(wide.as_ptr() as *const u8, wide.len() * 2);
        let status = RegSetValueExW(
            key,
            &HSTRING::from(VALUE_NAME),
            None,
            REG_SZ,
            Some(bytes),
        );
        let _ = RegCloseKey(key);
        if status != ERROR_SUCCESS {
            return Err(format!("无法写入开机启动项：{}", status.0));
        }
        Ok(())
    }
}
