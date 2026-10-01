// Prevents an additional console window on Windows in release.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    #[cfg(target_os = "macos")]
    fix_path();
    escouade_lib::run()
}

/// An app opened from the Finder or the Dock gets a bare PATH (/usr/bin:/bin…): `claude`, git
/// from Homebrew, node… would not be found. Takes the PATH of the user's login shell instead,
/// plus the usual install folders.
#[cfg(target_os = "macos")]
fn fix_path() {
    use std::process::{Command, Stdio};
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".into());
    let from_shell = Command::new(&shell)
        .args(["-ilc", "printf '__PATH__%s__PATH__' \"$PATH\""])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()
        .and_then(|o| {
            let out = String::from_utf8_lossy(&o.stdout).into_owned();
            let mut parts = out.split("__PATH__");
            parts.next()?;
            parts.next().map(str::to_string)
        })
        .filter(|p| !p.is_empty());
    let mut dirs: Vec<String> = Vec::new();
    let home = std::env::var("HOME").unwrap_or_default();
    let extra = [
        from_shell.unwrap_or_default(),
        std::env::var("PATH").unwrap_or_default(),
        format!("{home}/.local/bin"),
        format!("{home}/.cargo/bin"),
        "/opt/homebrew/bin".into(),
        "/opt/homebrew/sbin".into(),
        "/usr/local/bin".into(),
    ];
    for chunk in extra {
        for d in chunk.split(':') {
            if !d.is_empty() && !dirs.iter().any(|x| x == d) {
                dirs.push(d.to_string());
            }
        }
    }
    std::env::set_var("PATH", dirs.join(":"));
}
