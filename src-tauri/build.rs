fn main() {
    let mut attributes = tauri_build::Attributes::new();
    // Embed the Common-Controls v6 manifest in every binary, test harnesses included:
    // without it, test binaries that link the dialog/tray code fail to start on Windows
    // (STATUS_ENTRYPOINT_NOT_FOUND, TaskDialogIndirect).
    #[cfg(windows)]
    {
        attributes = attributes
            .windows_attributes(tauri_build::WindowsAttributes::new_without_app_manifest());
        let manifest = std::env::current_dir()
            .expect("cwd")
            .join("windows-app-manifest.xml");
        println!("cargo:rerun-if-changed={}", manifest.display());
        println!("cargo:rustc-link-arg=/MANIFEST:EMBED");
        println!("cargo:rustc-link-arg=/MANIFESTINPUT:{}", manifest.display());
        println!("cargo:rustc-link-arg=/WX");
    }
    #[cfg(target_os = "macos")]
    link_clang_rt();
    tauri_build::try_build(attributes).expect("tauri build script");
}

/// whisper.cpp's Metal code uses `@available`, which needs `___isPlatformVersionAtLeast` from
/// clang's runtime; Rust links with `-nodefaultlibs`, so the runtime is linked by hand.
#[cfg(target_os = "macos")]
fn link_clang_rt() {
    let out = std::process::Command::new("xcrun")
        .args(["clang", "--print-resource-dir"])
        .output();
    if let Ok(o) = out {
        let dir = String::from_utf8_lossy(&o.stdout).trim().to_string();
        if !dir.is_empty() {
            println!("cargo:rustc-link-search={dir}/lib/darwin");
            println!("cargo:rustc-link-lib=static=clang_rt.osx");
        }
    }
}
