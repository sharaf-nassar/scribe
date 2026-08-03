//! macOS AI-hook repair for packaged stable installs.
//!
//! Fresh DMG installs have no maintainer script equivalent to Debian's
//! `postinst`, so Claude Code and Codex never pick up Scribe's hook adapters
//! unless something explicitly runs `setup-{claude,codex}-hooks.sh`.
//! Stable macOS launches therefore probe the user's AI-tool configs and invoke
//! the bundled setup scripts when the current app bundle's hook paths are
//! missing.

#[cfg(target_os = "macos")]
use std::path::{Path, PathBuf};

#[cfg(target_os = "macos")]
use scribe_common::app::current_identity;
#[cfg(target_os = "macos")]
use serde_json::Value;

/// Best-effort startup repair for the bundled Claude/Codex hook adapters.
///
/// No-op on non-macOS platforms, dev builds, or non-bundled runs. Failures are
/// logged and never block client startup.
pub fn repair_ai_hooks_on_startup() {
    #[cfg(target_os = "macos")]
    if let Err(error) = repair_ai_hooks_on_startup_inner() {
        tracing::warn!(%error, "AI hook startup repair skipped");
    }
}

#[cfg(target_os = "macos")]
fn repair_ai_hooks_on_startup_inner() -> Result<(), String> {
    if current_identity().is_dev() {
        return Ok(());
    }

    let Some(resources_dir) = bundled_resources_dir() else {
        return Ok(());
    };
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return Err("HOME is not set".to_owned());
    };

    let claude_dir = home.join(".claude");
    if claude_dir.is_dir() && claude_needs_setup(&claude_dir, &resources_dir)? {
        run_setup_script(&resources_dir.join("setup-claude-hooks.sh"), &resources_dir)?;
    }

    let codex_dir = home.join(".codex");
    if codex_dir.is_dir() && codex_needs_setup(&codex_dir, &resources_dir)? {
        run_setup_script(&resources_dir.join("setup-codex-hooks.sh"), &resources_dir)?;
    }

    Ok(())
}

#[cfg(target_os = "macos")]
fn bundled_resources_dir() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let exe_dir = exe.parent()?;
    let resources = exe_dir.parent()?.join("Resources");
    resources.is_dir().then_some(resources)
}

#[cfg(target_os = "macos")]
fn claude_needs_setup(claude_dir: &Path, resources_dir: &Path) -> Result<bool, String> {
    let settings_path = claude_dir.join("settings.json");
    let expected_hook = resources_dir.join("ai-hook-claude.sh").to_string_lossy().into_owned();
    let expected_statusline =
        resources_dir.join("ai-hook-statusline.sh").to_string_lossy().into_owned();
    let contents = match std::fs::read_to_string(&settings_path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(true),
        Err(error) => {
            return Err(format!("failed to read {}: {error}", settings_path.display()));
        }
    };

    Ok(needs_claude_setup(&contents, &expected_hook, &expected_statusline))
}

#[cfg(target_os = "macos")]
fn codex_needs_setup(codex_dir: &Path, resources_dir: &Path) -> Result<bool, String> {
    let expected_hook = resources_dir.join("ai-hook-codex.sh").to_string_lossy().into_owned();
    let config_path = codex_dir.join("config.toml");
    let hooks_path = codex_dir.join("hooks.json");

    let config_contents = match std::fs::read_to_string(&config_path) {
        Ok(contents) => Some(contents),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            return Err(format!("failed to read {}: {error}", config_path.display()));
        }
    };
    let hooks_contents = match std::fs::read_to_string(&hooks_path) {
        Ok(contents) => Some(contents),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            return Err(format!("failed to read {}: {error}", hooks_path.display()));
        }
    };

    Ok(needs_codex_setup(config_contents.as_deref(), hooks_contents.as_deref(), &expected_hook))
}

#[cfg(target_os = "macos")]
fn run_setup_script(script_path: &Path, resources_dir: &Path) -> Result<(), String> {
    if !script_path.is_file() {
        return Err(format!("setup script {} is missing", script_path.display()));
    }

    let output = std::process::Command::new("/bin/bash")
        .arg(script_path)
        .arg("--hook-source")
        .arg(resources_dir)
        .output()
        .map_err(|error| format!("failed to launch {}: {error}", script_path.display()))?;

    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    if !output.status.success() {
        return Err(format!(
            "{} exited with {} (stdout: {}; stderr: {})",
            script_path.display(),
            output.status.code().map_or_else(|| "signal".to_owned(), |code| format!("code {code}")),
            if stdout.is_empty() { "<empty>" } else { stdout.as_str() },
            if stderr.is_empty() { "<empty>" } else { stderr.as_str() },
        ));
    }

    tracing::info!(
        script = %script_path.display(),
        output = %stdout,
        "repaired bundled AI hook registration"
    );
    if !stderr.is_empty() {
        tracing::warn!(
            script = %script_path.display(),
            stderr = %stderr,
            "AI hook setup script wrote to stderr"
        );
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn needs_claude_setup(contents: &str, expected_hook: &str, expected_statusline: &str) -> bool {
    let Ok(settings) = serde_json::from_str::<Value>(contents) else {
        return true;
    };
    if !contents.contains(expected_hook) {
        return true;
    }

    let has_current_statusline = contents.contains(expected_statusline);
    let has_scribe_statusline = contents.contains("ai-hook-statusline.sh");
    let has_statusline = settings.as_object().is_some_and(|root| root.get("statusLine").is_some());
    !has_current_statusline && (!has_statusline || has_scribe_statusline)
}

#[cfg(target_os = "macos")]
fn needs_codex_setup(
    config_contents: Option<&str>,
    hooks_contents: Option<&str>,
    expected_hook: &str,
) -> bool {
    !config_contents.is_some_and(|contents| contents.contains(expected_hook))
        && !hooks_contents.is_some_and(|contents| contents.contains(expected_hook))
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::{needs_claude_setup, needs_codex_setup};

    #[test]
    fn claude_missing_scribe_hook_needs_repair() {
        let settings = r#"{"hooks":{"Stop":[{"hooks":[{"command":"node /tmp/quill.js"}]}]}}"#;
        assert!(needs_claude_setup(
            settings,
            "/Applications/Scribe.app/Contents/Resources/ai-hook-claude.sh",
            "/Applications/Scribe.app/Contents/Resources/ai-hook-statusline.sh",
        ));
    }

    #[test]
    fn claude_custom_statusline_does_not_force_repair() {
        let settings = r#"{
            "hooks":{"Stop":[{"hooks":[{"command":"/Applications/Scribe.app/Contents/Resources/ai-hook-claude.sh stop"}]}]},
            "statusLine":{"type":"command","command":"node /tmp/custom.js"}
        }"#;
        assert!(!needs_claude_setup(
            settings,
            "/Applications/Scribe.app/Contents/Resources/ai-hook-claude.sh",
            "/Applications/Scribe.app/Contents/Resources/ai-hook-statusline.sh",
        ));
    }

    #[test]
    fn claude_missing_statusline_without_custom_one_needs_repair() {
        let settings = r#"{
            "hooks":{"Stop":[{"hooks":[{"command":"/Applications/Scribe.app/Contents/Resources/ai-hook-claude.sh stop"}]}]}
        }"#;
        assert!(needs_claude_setup(
            settings,
            "/Applications/Scribe.app/Contents/Resources/ai-hook-claude.sh",
            "/Applications/Scribe.app/Contents/Resources/ai-hook-statusline.sh",
        ));
    }

    #[test]
    fn claude_stale_scribe_statusline_needs_repair() {
        let settings = r#"{
            "hooks":{"Stop":[{"hooks":[{"command":"/Applications/Scribe.app/Contents/Resources/ai-hook-claude.sh stop"}]}]},
            "statusLine":{"type":"command","command":"/Applications/Old Scribe.app/Contents/Resources/ai-hook-statusline.sh"}
        }"#;
        assert!(needs_claude_setup(
            settings,
            "/Applications/Scribe.app/Contents/Resources/ai-hook-claude.sh",
            "/Applications/Scribe.app/Contents/Resources/ai-hook-statusline.sh",
        ));
    }

    #[test]
    fn codex_current_hook_in_config_is_fresh() {
        let config = r#"[[hooks.Stop]]
[[hooks.Stop.hooks]]
command = "\"/Applications/Scribe.app/Contents/Resources/ai-hook-codex.sh\" stop"
"#;
        assert!(!needs_codex_setup(
            Some(config),
            None,
            "/Applications/Scribe.app/Contents/Resources/ai-hook-codex.sh",
        ));
    }

    #[test]
    fn codex_without_scribe_hook_needs_repair() {
        let config = r#"[[hooks.Stop]]
[[hooks.Stop.hooks]]
command = "node /tmp/quill.js"
"#;
        assert!(needs_codex_setup(
            Some(config),
            None,
            "/Applications/Scribe.app/Contents/Resources/ai-hook-codex.sh",
        ));
    }
}
