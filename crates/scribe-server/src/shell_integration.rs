use std::collections::HashMap;
use std::path::{Path, PathBuf};

use tracing::debug;

/// Detected shell type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellKind {
    Bash,
    Zsh,
    Fish,
    Nushell,
    PowerShell,
    Unknown,
}

/// Detect the shell kind from a binary path or name.
pub fn detect_shell(binary: &str) -> ShellKind {
    let name = Path::new(binary).file_stem().and_then(|n| n.to_str()).unwrap_or(binary);
    match name {
        "bash" => ShellKind::Bash,
        "zsh" => ShellKind::Zsh,
        "fish" => ShellKind::Fish,
        "nu" => ShellKind::Nushell,
        "pwsh" | "powershell" => ShellKind::PowerShell,
        _ => ShellKind::Unknown,
    }
}

/// Resolve the shell integration scripts directory.
///
/// Tries exe-relative paths (installed and dev builds), then standard locations.
pub fn find_scripts_dir() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let exe_dir = exe.parent()?;

    // Installed Linux: /usr/bin/scribe-server → /usr/share/scribe/shell-integration
    let installed = exe_dir.parent()?.join("share/scribe/shell-integration");
    if installed.is_dir() {
        return Some(installed);
    }

    // macOS bundle: Contents/MacOS/scribe-server → Contents/Resources/shell-integration
    let macos = exe_dir.parent()?.join("Resources/shell-integration");
    if macos.is_dir() {
        return Some(macos);
    }

    // Dev build: walk up from exe to find the repo root (has dist/shell-integration).
    let mut dir = exe_dir;
    for _ in 0..5_u8 {
        let candidate = dir.join("dist/shell-integration");
        if candidate.is_dir() {
            return Some(candidate);
        }
        dir = dir.parent()?;
    }

    None
}

/// Build extra environment variables for shell integration.
///
/// Returns a `HashMap` to merge into `PtyOptions.env`.
pub fn build_env(shell_binary: &str, scripts_dir: &Path) -> HashMap<String, String> {
    let mut env = HashMap::new();
    let kind = detect_shell(shell_binary);

    env.insert("SCRIBE_SHELL_INTEGRATION".to_owned(), "1".to_owned());

    match kind {
        ShellKind::Bash => inject_bash(&mut env, scripts_dir),
        ShellKind::Zsh => inject_zsh(&mut env, scripts_dir),
        ShellKind::Fish => inject_fish(&mut env, scripts_dir),
        ShellKind::Nushell => inject_nushell(&mut env, scripts_dir),
        ShellKind::PowerShell => {}
        ShellKind::Unknown => {
            debug!(shell = shell_binary, "unknown shell, skipping integration env");
        }
    }

    env
}

/// Resolve the integration script path for shells that require an explicit
/// startup-file argument.
pub fn integration_script_path(shell_binary: &str, scripts_dir: &Path) -> Option<PathBuf> {
    let relative = match detect_shell(shell_binary) {
        ShellKind::Bash => "bash/scribe.bash",
        ShellKind::PowerShell => "powershell/scribe.ps1",
        ShellKind::Zsh | ShellKind::Fish | ShellKind::Nushell | ShellKind::Unknown => {
            return None;
        }
    };

    let script = scripts_dir.join(relative);
    script.is_file().then_some(script)
}

fn inject_bash(env: &mut HashMap<String, String>, scripts_dir: &Path) {
    let script = scripts_dir.join("bash/scribe.bash");
    if script.is_file() {
        env.insert("ENV".to_owned(), script.to_string_lossy().into_owned());
    }
}

fn inject_zsh(env: &mut HashMap<String, String>, scripts_dir: &Path) {
    let zsh_dir = scripts_dir.join("zsh");
    if zsh_dir.join(".zshenv").is_file() {
        let orig = std::env::var("ZDOTDIR").unwrap_or_default();
        env.insert("SCRIBE_ORIG_ZDOTDIR".to_owned(), orig);
        env.insert("ZDOTDIR".to_owned(), zsh_dir.to_string_lossy().into_owned());
    }
}

fn inject_fish(env: &mut HashMap<String, String>, scripts_dir: &Path) {
    // Fish searches `$XDG_DATA_DIRS/fish/vendor_conf.d/` for config files.
    // We must prepend `scripts_dir` itself (e.g. `.../shell-integration`) so
    // that fish resolves `scripts_dir/fish/vendor_conf.d/scribe.fish`.
    // Prepending `scripts_dir/fish` would cause fish to look at
    // `.../shell-integration/fish/fish/vendor_conf.d/` which doesn't exist.
    if scripts_dir.join("fish/vendor_conf.d").is_dir() {
        let existing = std::env::var("XDG_DATA_DIRS")
            .unwrap_or_else(|_| "/usr/local/share:/usr/share".to_owned());
        env.insert("XDG_DATA_DIRS".to_owned(), format!("{}:{existing}", scripts_dir.display()));
    }
}

fn inject_nushell(env: &mut HashMap<String, String>, scripts_dir: &Path) {
    // Nushell auto-loads vendor modules from `$XDG_DATA_DIRS/nushell/vendor/autoload/`.
    // As with fish, prepend the shell-integration root itself so Nushell resolves
    // `scripts_dir/nushell/vendor/autoload/scribe.nu`.
    if scripts_dir.join("nushell/vendor/autoload").is_dir() {
        let existing = std::env::var("XDG_DATA_DIRS")
            .unwrap_or_else(|_| "/usr/local/share:/usr/share".to_owned());
        env.insert("XDG_DATA_DIRS".to_owned(), format!("{}:{existing}", scripts_dir.display()));
    }
}
