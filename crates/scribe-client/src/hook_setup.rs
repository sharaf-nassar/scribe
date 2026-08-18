//! macOS AI-hook repair for packaged stable installs, plus the Pi extension
//! setup path shared with the settings window.
//!
//! Fresh DMG installs have no maintainer script equivalent to Debian's
//! `postinst`, so Claude Code and Codex never pick up Scribe's hook adapters
//! unless something explicitly runs `setup-{claude,codex}-hooks.sh`.
//! Stable macOS launches therefore probe the user's AI-tool configs and invoke
//! the bundled setup scripts when the current app bundle's hook paths are
//! missing.
//!
//! [`repair_pi_extension_if_enabled`] runs at packaged startup on every
//! platform and after a settings-window `terminal.pi_integration` enable
//! transition. Both call sites share resource discovery and script execution,
//! so Linux can repair installs whose maintainer script lacked a target user
//! and behavior cannot drift between platforms.

#[cfg(target_os = "macos")]
use std::path::Path;
use std::path::PathBuf;

use scribe_common::app::current_identity;
#[cfg(target_os = "macos")]
use serde_json::Value;

/// Best-effort startup repair for bundled AI integrations.
///
/// Claude and Codex repair remains macOS-only. Pi repair runs independently on
/// every packaged platform so an unrelated hook failure cannot skip it. Errors
/// are logged and printed as plain text, but never block client startup.
pub fn repair_ai_hooks_on_startup() {
    #[cfg(target_os = "macos")]
    if let Err(error) = repair_ai_hooks_on_startup_inner() {
        tracing::warn!(%error, "AI hook startup repair skipped");
    }

    if let Err(error) = repair_pi_extension_if_enabled() {
        tracing::warn!(%error, "Pi extension startup repair needs attention");
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
        run_setup_script(
            &resources_dir.join("setup-claude-hooks.sh"),
            &resources_dir,
            "--hook-source",
        )?;
    }

    let codex_dir = home.join(".codex");
    if codex_dir.is_dir() && codex_needs_setup(&codex_dir, &resources_dir)? {
        run_setup_script(
            &resources_dir.join("setup-codex-hooks.sh"),
            &resources_dir,
            "--hook-source",
        )?;
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

fn run_setup_script(
    script_path: &std::path::Path,
    resources_dir: &std::path::Path,
    source_flag: &str,
) -> Result<(), String> {
    if !script_path.is_file() {
        return Err(format!("setup script {} is missing", script_path.display()));
    }

    let output = std::process::Command::new("/bin/bash")
        .arg(script_path)
        .arg(source_flag)
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

/// Validate one packaged Pi asset directory. Existing package directories are
/// errors when either required asset is absent; only a genuinely unpackaged
/// build may no-op.
fn validate_pi_extension_resource_dir(dir: PathBuf) -> Result<PathBuf, String> {
    for asset in ["pi-extension.ts", "setup-pi-extension.sh"] {
        let path = dir.join(asset);
        if !path.is_file() {
            return Err(format!("packaged Pi integration asset is missing: {}", path.display()));
        }
    }
    Ok(dir)
}

/// Directory containing the packaged Pi extension assets: an explicit
/// override, the macOS app bundle's `Resources` directory, or the Linux share
/// directory for the active flavor. `None` means this is an unpackaged build.
fn pi_extension_resource_dir() -> Result<Option<PathBuf>, String> {
    if let Some(dir) = std::env::var_os("SCRIBE_INSTALL_PREFIX").map(PathBuf::from) {
        return validate_pi_extension_resource_dir(dir).map(Some);
    }

    #[cfg(target_os = "macos")]
    if let Some(dir) = bundled_resources_dir() {
        return validate_pi_extension_resource_dir(dir).map(Some);
    }

    let share_dir = PathBuf::from("/usr/share").join(current_identity().slug());
    if share_dir.is_dir() {
        return validate_pi_extension_resource_dir(share_dir).map(Some);
    }

    Ok(None)
}

fn pi_integration_enabled(config: &scribe_common::config::ScribeConfig) -> bool {
    config.terminal.ai_integration.pi.enabled()
}

/// Best-effort install/repair of the packaged Pi extension.
///
/// Shared by the macOS startup repair path above and the settings window's
/// `terminal.pi_integration` enable-transition trigger. No-ops when Pi
/// integration is disabled or no packaged extension source can be found.
/// Never panics or blocks longer than the setup script itself; a failure is
/// returned as a plain string for the caller to log or surface as a
/// non-blocking notice — setup must never block Scribe or Pi.
pub fn repair_pi_extension_if_enabled() -> Result<(), String> {
    let config = scribe_common::config::load_config().map_err(|error| error.to_string())?;
    if !pi_integration_enabled(&config) {
        return Ok(());
    }
    let Some(resources_dir) = pi_extension_resource_dir()? else {
        return Ok(());
    };
    run_setup_script(
        &resources_dir.join("setup-pi-extension.sh"),
        &resources_dir,
        "--extension-source",
    )
}

#[cfg(target_os = "macos")]
fn needs_claude_setup(contents: &str, expected_hook: &str, expected_statusline: &str) -> bool {
    let Ok(settings) = serde_json::from_str::<Value>(contents) else {
        return true;
    };
    if !has_session_end_hook(contents, expected_hook) {
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
    !config_contents.is_some_and(|contents| has_session_end_hook(contents, expected_hook))
        && !hooks_contents.is_some_and(|contents| has_session_end_hook(contents, expected_hook))
}

#[cfg(target_os = "macos")]
fn has_session_end_hook(contents: &str, expected_hook: &str) -> bool {
    contents.lines().any(|line| line.contains(expected_hook) && line.contains("session_end"))
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
    fn claude_stop_only_hook_needs_session_end_repair() {
        let settings = r#"{
            "hooks":{"Stop":[{"hooks":[{"command":"/Applications/Scribe.app/Contents/Resources/ai-hook-claude.sh stop"}]}]},
            "statusLine":{"type":"command","command":"node /tmp/custom.js"}
        }"#;
        assert!(needs_claude_setup(
            settings,
            "/Applications/Scribe.app/Contents/Resources/ai-hook-claude.sh",
            "/Applications/Scribe.app/Contents/Resources/ai-hook-statusline.sh",
        ));
    }

    #[test]
    fn claude_session_end_hook_with_custom_statusline_is_fresh() {
        let settings = r#"{
            "hooks":{"SessionEnd":[{"hooks":[{"command":"/Applications/Scribe.app/Contents/Resources/ai-hook-claude.sh session_end"}]}]},
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
    fn codex_stop_only_hook_needs_session_end_repair() {
        let config = r#"[[hooks.Stop]]
[[hooks.Stop.hooks]]
command = "\"/Applications/Scribe.app/Contents/Resources/ai-hook-codex.sh\" stop"
"#;
        assert!(needs_codex_setup(
            Some(config),
            None,
            "/Applications/Scribe.app/Contents/Resources/ai-hook-codex.sh",
        ));
    }

    #[test]
    fn codex_session_end_hook_in_config_is_fresh() {
        let config = r#"[[hooks.SessionEnd]]
[[hooks.SessionEnd.hooks]]
command = "\"/Applications/Scribe.app/Contents/Resources/ai-hook-codex.sh\" session_end"
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

#[cfg(test)]
mod pi_extension_tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::{pi_integration_enabled, validate_pi_extension_resource_dir};

    #[test]
    fn pi_integration_defaults_enabled_and_respects_toggle() {
        let mut config = scribe_common::config::ScribeConfig::default();
        assert!(pi_integration_enabled(&config));

        config.terminal.ai_integration.pi = scribe_common::config::AiIntegrationToggle::new(false);
        assert!(!pi_integration_enabled(&config));
    }

    #[test]
    fn packaged_pi_assets_must_both_exist() {
        let nonce =
            SystemTime::now().duration_since(UNIX_EPOCH).expect("clock after epoch").as_nanos();
        let dir =
            std::env::temp_dir().join(format!("scribe-pi-assets-{}-{nonce}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create fixture directory");

        let source_error =
            validate_pi_extension_resource_dir(dir.clone()).expect_err("source is missing");
        assert!(source_error.contains("pi-extension.ts"));
        std::fs::write(dir.join("pi-extension.ts"), "// SCRIBE-MANAGED-PI-EXTENSION\n")
            .expect("write extension fixture");

        let setup_error =
            validate_pi_extension_resource_dir(dir.clone()).expect_err("setup is missing");
        assert!(setup_error.contains("setup-pi-extension.sh"));
        std::fs::write(dir.join("setup-pi-extension.sh"), "#!/bin/bash\n")
            .expect("write setup fixture");

        assert_eq!(validate_pi_extension_resource_dir(dir.clone()).expect("assets valid"), dir);
        std::fs::remove_dir_all(&dir).expect("remove fixture directory");
    }
}
