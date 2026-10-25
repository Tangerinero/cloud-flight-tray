use winreg::enums::*;
use winreg::RegKey;

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const PREF_KEY: &str = r"Software\cloud-flight-tray";
const RUN_VALUE: &str = "CloudFlightTray";
const PREF_VALUE: &str = "start_with_windows";

/// Turn startup on the first time this setting exists, and keep the Run path current.
pub fn ensure_default_enabled() {
    match read_pref() {
        None => {
            let _ = set_enabled(true);
        }
        Some(true) => {
            let _ = write_run_entry();
        }
        Some(false) => {}
    }
}

pub fn is_enabled() -> bool {
    read_pref().unwrap_or(false)
}

pub fn set_enabled(on: bool) -> bool {
    if write_pref(on).is_err() {
        return is_enabled();
    }
    let result = if on {
        write_run_entry()
    } else {
        delete_run_entry()
    };
    if result.is_err() && on {
        let _ = write_pref(false);
    }
    is_enabled()
}

fn read_pref() -> Option<bool> {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let key = hkcu.open_subkey(PREF_KEY).ok()?;
    let value: u32 = key.get_value(PREF_VALUE).ok()?;
    Some(value != 0)
}

fn write_pref(on: bool) -> std::io::Result<()> {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let (key, _) = hkcu.create_subkey(PREF_KEY)?;
    key.set_value(PREF_VALUE, &(u32::from(on)))
}

fn write_run_entry() -> std::io::Result<()> {
    let command = exe_command()?;
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let (key, _) = hkcu.create_subkey(RUN_KEY)?;
    key.set_value(RUN_VALUE, &command)
}

fn delete_run_entry() -> std::io::Result<()> {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let (key, _) = hkcu.create_subkey(RUN_KEY)?;
    match key.delete_value(RUN_VALUE) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
}

fn exe_command() -> std::io::Result<String> {
    let path = std::env::current_exe()?;
    Ok(format!("\"{}\"", path.display()))
}
