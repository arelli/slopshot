//! `cargo install-desktop`: install slopshot to ~/.local/bin and add a desktop entry.
//!
//! Cargo has no post-install hooks, so this wraps `cargo install`. It's a separate
//! package so it builds in a moment and `cargo install` doesn't pick it up as a binary.

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

const APP: &str = "slopshot";
const ICON_SVG: &[u8] = include_bytes!("../../assets/icon.svg");
const ICON_PNG: &[u8] = include_bytes!("../../assets/icon.png");

fn main() -> ExitCode {
    match install() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn install() -> Result<(), String> {
    if !cfg!(target_os = "linux") {
        return Err("desktop entries are Linux-only; use `cargo install --path .` here".into());
    }
    let home = std::env::var_os("HOME").filter(|h| !h.is_empty()).ok_or("HOME isn't set")?;
    let root = PathBuf::from(home).join(".local");
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();

    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let status = Command::new(cargo)
        .args(["install", "--locked", "--force", "--path"])
        .arg(repo)
        .arg("--root")
        .arg(&root)
        .status()
        .map_err(|e| format!("couldn't run cargo: {e}"))?;
    if !status.success() {
        return Err("cargo install failed".into());
    }

    let bin = root.join("bin").join(APP);
    let data = std::env::var_os("XDG_DATA_HOME").filter(|d| !d.is_empty()).map_or_else(|| root.join("share"), PathBuf::from);
    let entry = data.join("applications").join(format!("{APP}.desktop"));
    let bin_str = bin.to_str().ok_or("install path isn't valid UTF-8")?;
    write(&entry, desktop_entry(bin_str).as_bytes())?;
    // Both sizes: not every launcher can draw an SVG.
    let icons = data.join("icons/hicolor");
    let svg = icons.join(format!("scalable/apps/{APP}.svg"));
    write(&svg, ICON_SVG)?;
    write(&icons.join(format!("256x256/apps/{APP}.png")), ICON_PNG)?;

    println!("Installed {}", bin.display());
    println!("Added {}", entry.display());
    println!("Added icons under {}", icons.display());
    let on_path = std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| d == root.join("bin")));
    if !on_path {
        println!("Note: {} isn't on your PATH. The desktop entry works anyway.", root.join("bin").display());
    }
    Ok(())
}

fn write(path: &Path, content: &[u8]) -> Result<(), String> {
    std::fs::create_dir_all(path.parent().unwrap()).map_err(|e| format!("couldn't create {}: {e}", path.display()))?;
    std::fs::write(path, content).map_err(|e| format!("couldn't write {}: {e}", path.display()))
}

/// The entry's Exec uses the full path, because a desktop session's PATH often lacks
/// ~/.local/bin. No MimeType: claiming inode/directory could take over opening folders.
fn desktop_entry(bin: &str) -> String {
    format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name=slopshot\n\
         GenericName=Photo Culler\n\
         Comment=Star, pick, reject and export Sony ARW and Canon CR3 files\n\
         Exec={} %f\n\
         Icon={APP}\n\
         Terminal=false\n\
         Categories=Graphics;Photography;Viewer;\n\
         Keywords=raw;arw;cr3;photo;cull;\n\
         StartupWMClass={APP}\n",
        exec_quote(bin)
    )
}

/// Quote a path for Exec. The spec escapes `"`, `` ` ``, `$` and `\` inside quotes, a
/// literal `%` is doubled, and a desktop file value then escapes each backslash again.
fn exec_quote(path: &str) -> String {
    let plain = |c: char| c.is_ascii_alphanumeric() || "/._-+".contains(c);
    if path.chars().all(plain) {
        return path.into();
    }
    let mut out = String::from("\"");
    for c in path.chars() {
        match c {
            '"' | '`' | '$' => out.extend(['\\', '\\', c]),
            '\\' => out.push_str("\\\\\\\\"),
            '%' => out.push_str("%%"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exec_paths_are_quoted_only_when_needed() {
        assert_eq!(exec_quote("/home/me/.local/bin/slopshot"), "/home/me/.local/bin/slopshot");
        assert_eq!(exec_quote("/home/a b/slopshot"), "\"/home/a b/slopshot\"");
        assert_eq!(exec_quote("/h/$x/100%"), "\"/h/\\\\$x/100%%\"");
    }
}
