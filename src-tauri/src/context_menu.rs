//! Windows Explorer "Send via QuickDrop" context-menu installer.
//!
//! Writes per-user registry keys under
//! `HKCU\Software\Classes\*\shell\QuickDrop\command`. Per-user keys
//! avoid needing admin elevation. Uninstall removes the same keys.

#[cfg(windows)]
pub fn install() -> anyhow::Result<()> {
    use winreg::enums::*;
    use winreg::RegKey;

    let exe = std::env::current_exe()?;
    let exe_str = exe.to_string_lossy().into_owned();
    let icon = format!("{exe_str},0");
    let cmd = format!("\"{exe_str}\" --send \"%1\"");
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);

    for root in &["Software\\Classes\\*\\shell\\QuickDrop", "Software\\Classes\\Directory\\shell\\QuickDrop"] {
        let (key, _) = hkcu.create_subkey(root)?;
        key.set_value("", &"Send via QuickDrop")?;
        key.set_value("Icon", &icon)?;
        let (sub, _) = key.create_subkey("command")?;
        sub.set_value("", &cmd)?;
    }
    tracing::info!("context menu installed");
    Ok(())
}

#[cfg(windows)]
pub fn uninstall() -> anyhow::Result<()> {
    use winreg::enums::*;
    use winreg::RegKey;
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    for root in &["Software\\Classes\\*\\shell\\QuickDrop", "Software\\Classes\\Directory\\shell\\QuickDrop"] {
        let _ = hkcu.delete_subkey_all(root);
    }
    tracing::info!("context menu uninstalled");
    Ok(())
}

#[cfg(not(windows))]
pub fn install() -> anyhow::Result<()> {
    Err(anyhow::anyhow!("context menu install only supported on Windows"))
}

#[cfg(not(windows))]
pub fn uninstall() -> anyhow::Result<()> {
    Err(anyhow::anyhow!("context menu uninstall only supported on Windows"))
}
