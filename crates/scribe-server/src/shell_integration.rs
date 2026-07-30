use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use scribe_common::app::current_identity;
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
///
/// A packaged hit is memoized: those scripts live inside the install and cannot
/// move while the server runs, so every launch after the first skips the probe
/// stats entirely. A dev-build hit is deliberately not cached — `dist/` is the
/// tree a developer edits and re-lays-out under a running server, and caching it
/// would pin the session to whatever layout existed at the first launch.
pub fn find_scripts_dir() -> Option<PathBuf> {
    if let Some(cached) = PACKAGED_SCRIPTS_DIR.get() {
        return Some(cached.clone());
    }

    let exe = std::env::current_exe().ok()?;
    let (dir, layout) = resolve_scripts_dir(exe.parent()?, current_identity().share_dir_name())?;
    match layout {
        ScriptsLayout::Packaged => Some(PACKAGED_SCRIPTS_DIR.get_or_init(|| dir).clone()),
        ScriptsLayout::Dev => Some(dir),
    }
}

/// The packaged scripts directory, resolved at most once per process.
///
/// Left unset for dev builds and for a failed probe, both of which re-resolve.
static PACKAGED_SCRIPTS_DIR: OnceLock<PathBuf> = OnceLock::new();

/// Which install layout a resolved scripts directory came from.
enum ScriptsLayout {
    /// Shipped inside a deb or DMG install, next to the binary that found it.
    Packaged,
    /// A repo checkout's `dist/shell-integration`, edited between launches.
    Dev,
}

/// Layout probe behind [`find_scripts_dir`], split out so each layout can be
/// exercised without moving the running executable.
fn resolve_scripts_dir(exe_dir: &Path, share_dir_name: &str) -> Option<(PathBuf, ScriptsLayout)> {
    // Installed Linux: /usr/bin/scribe-server → /usr/share/scribe/shell-integration
    let installed = exe_dir.parent()?.join("share").join(share_dir_name).join("shell-integration");
    if installed.is_dir() {
        return Some((installed, ScriptsLayout::Packaged));
    }

    // macOS bundle: Contents/MacOS/scribe-server → Contents/Resources/shell-integration
    let macos = exe_dir.parent()?.join("Resources/shell-integration");
    if macos.is_dir() {
        return Some((macos, ScriptsLayout::Packaged));
    }

    // Dev build: walk up from exe to find the repo root (has dist/shell-integration).
    let mut dir = exe_dir;
    for _ in 0..5_u8 {
        let candidate = dir.join("dist/shell-integration");
        if candidate.is_dir() {
            return Some((candidate, ScriptsLayout::Dev));
        }
        dir = dir.parent()?;
    }

    None
}

/// Resolve the `scribe-hook-helper` binary for the current install layout.
///
/// A bare PATH lookup only works for dev shells that happen to have
/// `target/<profile>` on `PATH`; none of the packaged layouts install the
/// helper into a PATH directory. The server therefore resolves it here and
/// exports the absolute path as `SCRIBE_HOOK_HELPER` (see
/// `specs/017-audit-findings-triage/spec.md` OQ5), which the shell
/// integration scripts and the `ai-hook-*.sh` adapters both honour.
///
/// Covers all four layouts: DMG (`Contents/MacOS`) and dev builds
/// (`target/<profile>`) place the helper next to the server binary; prod-deb
/// and dev-deb place it under `/usr/share/<flavor>/`.
pub fn find_hook_helper() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let found = resolve_hook_helper(exe.parent()?, current_identity().share_dir_name());
    if found.is_none() {
        debug!("scribe-hook-helper not found for this layout; falling back to PATH");
    }
    found
}

/// Layout probe behind [`find_hook_helper`], split out so the four packaged
/// layouts can be exercised without moving the running executable.
fn resolve_hook_helper(exe_dir: &Path, share_dir_name: &str) -> Option<PathBuf> {
    // macOS bundle (Contents/MacOS/) and dev builds (target/<profile>/) both
    // keep the helper next to the server binary.
    let sibling = exe_dir.join(HOOK_HELPER_BIN);
    if sibling.is_file() {
        return Some(sibling);
    }

    // Installed Linux, both flavors: /usr/bin/scribe-server →
    // /usr/share/{scribe,scribe-dev}/scribe-hook-helper.
    let installed = exe_dir.parent()?.join("share").join(share_dir_name).join(HOOK_HELPER_BIN);
    installed.is_file().then_some(installed)
}

/// File name of the hook helper binary shipped alongside the server.
const HOOK_HELPER_BIN: &str = "scribe-hook-helper";

/// Build extra environment variables for shell integration.
///
/// Takes the [`ShellKind`] the launch already resolved rather than re-deriving
/// it from the binary path.
///
/// Returns a `HashMap` to merge into `PtyOptions.env`.
pub fn build_env(kind: ShellKind, scripts_dir: &Path) -> HashMap<String, String> {
    let mut env = HashMap::new();

    env.insert("SCRIBE_SHELL_INTEGRATION".to_owned(), "1".to_owned());

    match kind {
        ShellKind::Bash => inject_bash(&mut env, scripts_dir),
        ShellKind::Zsh => inject_zsh(&mut env, scripts_dir),
        ShellKind::Fish => inject_fish(&mut env, scripts_dir),
        ShellKind::Nushell => inject_nushell(&mut env, scripts_dir),
        ShellKind::PowerShell | ShellKind::Unknown => {}
    }

    env
}

/// Resolve the integration script path for shells that require an explicit
/// startup-file argument.
pub fn integration_script_path(kind: ShellKind, scripts_dir: &Path) -> Option<PathBuf> {
    let relative = match kind {
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

/// Keeps shells that tests spawn from reaching the developer's desktop.
///
/// pwsh resolves a dot-sourced non-`.ps1` path as a native command, and
/// .NET's shell-execute fallback then hands that path to a desktop opener,
/// so a suite that merely asserts "pwsh applies nothing" was enough to pop
/// a terminal window on the machine running `cargo test`. Sealed children
/// lose their session handles and resolve every opener .NET is willing to
/// run to a no-op stub instead.
#[cfg(test)]
pub mod desktop_isolation {
    use std::os::unix::fs::PermissionsExt as _;
    use std::path::{Path, PathBuf};
    use std::process::Command;

    /// The programs `System.Diagnostics.Process` will exec on Unix when
    /// `UseShellExecute` is set, in the order it tries them.
    const DESKTOP_OPENERS: [&str; 3] = ["xdg-open", "gnome-open", "kfmclient"];

    /// Scrubs the session handles a desktop opener needs, for probes that
    /// have no scratch directory to stage stubs in.
    pub fn scrub_desktop_env(command: &mut Command) {
        command
            .env_remove("DISPLAY")
            .env_remove("WAYLAND_DISPLAY")
            .env_remove("DBUS_SESSION_BUS_ADDRESS");
    }

    /// Scrubs the session handles and puts no-op opener stubs, staged
    /// under `scratch`, ahead of the inherited `PATH` so the shell still
    /// finds its own tools further down.
    pub fn seal_child(command: &mut Command, scratch: &Path) {
        let stubs = stage_opener_stubs(scratch);
        let path = match std::env::var_os("PATH") {
            Some(inherited) => {
                let mut entries = vec![stubs];
                entries.extend(std::env::split_paths(&inherited));
                std::env::join_paths(entries).expect("join stub PATH")
            }
            None => stubs.into_os_string(),
        };
        scrub_desktop_env(command.env("PATH", path));
    }

    fn stage_opener_stubs(scratch: &Path) -> PathBuf {
        let bin = scratch.join("opener-stubs");
        std::fs::create_dir_all(&bin).expect("create opener stub dir");
        for opener in DESKTOP_OPENERS {
            let stub = bin.join(opener);
            std::fs::write(&stub, "#!/bin/sh\nexit 0\n").expect("write opener stub");
            std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755))
                .expect("chmod opener stub");
        }
        bin
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use std::process::Command;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::desktop_isolation::{scrub_desktop_env, seal_child};

    /// The shipped fish integration, read straight out of `dist/` so the
    /// test exercises the same bytes the installers copy.
    fn fish_script() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../dist/shell-integration/fish/vendor_conf.d/scribe.fish")
    }

    /// The shipped nushell integration, read out of `dist/` for the same
    /// reason as [`fish_script`].
    fn nu_script() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../dist/shell-integration/nushell/vendor/autoload/scribe.nu")
    }

    /// One recorded `scribe-hook-helper` invocation: its argv plus the
    /// payload document it was handed on stdin.
    #[derive(Debug)]
    struct Invocation {
        argv: Vec<String>,
        stdin: String,
    }

    /// The `{ "added": …, "removed": … }` document the emit streams to the
    /// helper. Payloads are no longer arguments — argv is world-readable
    /// through `/proc/<pid>/cmdline` and one argument cannot exceed
    /// `MAX_ARG_STRLEN`.
    #[derive(serde::Deserialize)]
    struct EnvPayload {
        added: BTreeMap<String, String>,
        removed: Vec<String>,
    }

    fn payload(call: &Invocation) -> EnvPayload {
        assert!(
            call.argv.iter().any(|arg| arg == "--payload-stdin"),
            "emit must select the stdin transport, got {:?}",
            call.argv
        );
        serde_json::from_str(&call.stdin)
            .unwrap_or_else(|e| panic!("payload is not valid JSON ({e}): {:?}", call.stdin))
    }

    /// Sources the fish integration under a stub hook helper that records
    /// its argv and stdin, runs `body` afterwards, and returns every
    /// invocation.
    ///
    /// Returns `None` when fish is not installed; the shell scripts are
    /// only exercisable where their interpreter exists.
    fn record_fish_emits(body: &str) -> Option<Vec<Invocation>> {
        let mut probe = Command::new("fish");
        probe.arg("--version");
        scrub_desktop_env(&mut probe);
        if !probe.output().is_ok_and(|out| out.status.success()) {
            return None;
        }

        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).expect("clock").as_nanos();
        let dir =
            std::env::temp_dir().join(format!("scribe-fish-env-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create scratch dir");

        // NUL-separated fields per invocation: `CALL`, the argv, the
        // `STDIN` marker, then the payload document — so values containing
        // spaces or newlines stay intact. `string collect --allow-empty`
        // is what keeps the payload exactly one field even when the helper
        // was handed nothing at all.
        let record = dir.join("calls.bin");
        let recorder = dir.join("recorder.fish");
        std::fs::write(
            &recorder,
            format!(
                "#!/usr/bin/env fish\n\
                 set -l payload (cat | string collect --allow-empty)\n\
                 string join0 -- CALL $argv STDIN $payload >> '{}'\n",
                record.display(),
            ),
        )
        .expect("write recorder");
        std::fs::set_permissions(&recorder, std::fs::Permissions::from_mode(0o755))
            .expect("chmod recorder");

        let driver = dir.join("driver.fish");
        std::fs::write(
            &driver,
            format!(
                "set -gx TERM_PROGRAM Scribe\n\
                 set -gx SCRIBE_HOOK_HELPER '{}'\n\
                 {body}\n",
                recorder.display(),
            ),
        )
        .expect("write driver");

        let mut command = Command::new("fish");
        command
            .arg("--no-config")
            .arg(&driver)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        seal_child(&mut command, &dir);
        let status = command.status().expect("run fish");
        assert!(status.success(), "fish driver exited with {status}");

        let raw = std::fs::read(&record).unwrap_or_default();
        std::fs::remove_dir_all(&dir).expect("clean scratch dir");

        let fields: Vec<String> = raw
            .split(|byte| *byte == 0)
            .map(|field| String::from_utf8_lossy(field).into_owned())
            .collect();
        Some(parse_calls(&fields))
    }

    /// Positional parse of the recorder log: everything between `CALL` and
    /// `STDIN` is argv, and the single field after `STDIN` is the payload.
    /// A trailing empty field from `string join0`'s terminator is ignored.
    fn parse_calls(fields: &[String]) -> Vec<Invocation> {
        let mut calls = Vec::new();
        let mut idx = 0;
        while idx < fields.len() {
            if fields[idx] != "CALL" {
                idx += 1;
                continue;
            }
            idx += 1;
            let start = idx;
            while idx < fields.len() && fields[idx] != "STDIN" && fields[idx] != "CALL" {
                idx += 1;
            }
            let argv = fields[start..idx].to_vec();
            let stdin = take_stdin(fields, &mut idx);
            calls.push(Invocation { argv, stdin });
        }
        calls
    }

    /// Consumes the `STDIN` marker and the one payload field behind it.
    fn take_stdin(fields: &[String], idx: &mut usize) -> String {
        if fields.get(*idx).map(String::as_str) != Some("STDIN") {
            return String::new();
        }
        *idx += 1;
        let payload = fields.get(*idx).cloned().unwrap_or_default();
        *idx += 1;
        payload
    }

    /// An exported variable with an empty value used to wipe the whole
    /// fish payload: `__scribe_json_escape` echoed a zero-element list,
    /// and concatenating that is a cartesian product that collapses the
    /// accumulator, so fish dropped `--added-json` from the argv
    /// entirely and the server recorded an empty baseline. A list-valued
    /// export (`PATH` and friends) is the other silent one: an unquoted
    /// indirect read expands it to one element per component, so the
    /// recorded value stops being what a child process receives.
    #[test]
    fn fish_env_payload_survives_empty_and_list_values() {
        let body = format!(
            "set -gx SCRIBE_PROBE_EMPTY ''\n\
             set -gx SCRIBE_PROBE_LIST a b c\n\
             set -gx SCRIBE_PROBE_MULTI 'one\ntwo'\n\
             source '{}'\n\
             set -gx SCRIBE_PROBE_DELTA_EMPTY ''\n\
             set -gx SCRIBE_PROBE_DELTA_VALUE changed\n\
             set -e SCRIBE_PROBE_LIST\n\
             emit fish_prompt\n",
            fish_script().display(),
        );
        // No fish on this host: the shipped script has no interpreter to
        // exercise, and `fish_payload_interpolations_are_quoted` still runs.
        let Some(calls) = record_fish_emits(&body) else {
            return;
        };

        let (baseline, deltas): (Vec<_>, Vec<_>) =
            calls.iter().partition(|call| call.argv.iter().any(|arg| arg == "--baseline-ready"));
        assert_eq!(baseline.len(), 1, "expected one baseline emit, got {calls:?}");
        assert_eq!(deltas.len(), 1, "expected one per-prompt delta emit, got {calls:?}");

        // Nothing value-bearing may reach argv: that is the whole point of
        // the stdin transport.
        for call in &calls {
            for arg in &call.argv {
                assert!(
                    !arg.contains("SCRIBE_PROBE"),
                    "emit leaked an environment value into argv: {arg}"
                );
            }
        }

        let base = payload(baseline[0]);
        assert_eq!(base.added.get("SCRIBE_PROBE_EMPTY").map(String::as_str), Some(""));
        assert_eq!(base.added.get("SCRIBE_PROBE_LIST").map(String::as_str), Some("a b c"));
        assert_eq!(base.added.get("SCRIBE_PROBE_MULTI").map(String::as_str), Some("one\ntwo"));
        assert!(
            base.added.len() > 3,
            "baseline should carry the whole environment, got {:?}",
            base.added
        );
        assert!(base.removed.is_empty(), "baseline removes nothing, got {:?}", base.removed);

        let delta = payload(deltas[0]);
        assert_eq!(delta.added.get("SCRIBE_PROBE_DELTA_EMPTY").map(String::as_str), Some(""));
        assert_eq!(
            delta.added.get("SCRIBE_PROBE_DELTA_VALUE").map(String::as_str),
            Some("changed")
        );
        assert_eq!(delta.removed, vec!["SCRIBE_PROBE_LIST".to_owned()]);
    }

    /// A snapshot value is only usable as a restore value if it carries
    /// the separator fish itself hands a child process — a colon for a
    /// path variable, a space for every other list. The double-quoted
    /// indirect read the snapshot uses is the only expansion form that
    /// reproduces both, so this pins it against a rewrite to an explicit
    /// space join, which would record `PATH` as `a b c` and restore it as
    /// one entry that breaks command lookup. Path-ness is not derivable
    /// from the name, so `set --path` and `set --unpath` are pinned
    /// alongside the real `PATH`, which is compared against what a child
    /// process actually received and then fed back through fish's own
    /// `set -gx` restore form — read straight out of the diff cache,
    /// which doubles as a check that `__scribe_envm_PATH` holds the
    /// joined value rather than a re-split list.
    #[test]
    fn fish_env_snapshot_joins_path_variables_like_fish_exports() {
        let body = format!(
            "set -gx --path SCRIBE_PROBE_PATHVAR /probe/one /probe/two\n\
             set -gx --unpath SCRIBE_PROBE_FAKEPATH a b\n\
             set -gx SCRIBE_PROBE_PLAIN one two\n\
             set -gx SCRIBE_PROBE_CHILD_PATH (sh -c 'printf %s \"$PATH\"')\n\
             source '{}'\n\
             set -gx PATH $__scribe_envm_PATH\n\
             set -gx SCRIBE_PROBE_RESTORED_COUNT (count $PATH)\n\
             set -gx SCRIBE_PROBE_RESTORED_PATH (sh -c 'printf %s \"$PATH\"')\n\
             set -gx --path SCRIBE_PROBE_PATHVAR /probe/one /probe/two /probe/three\n\
             emit fish_prompt\n",
            fish_script().display(),
        );
        let Some(calls) = record_fish_emits(&body) else {
            return;
        };

        let (baseline, deltas): (Vec<_>, Vec<_>) =
            calls.iter().partition(|call| call.argv.iter().any(|arg| arg == "--baseline-ready"));
        assert_eq!(baseline.len(), 1, "expected one baseline emit, got {calls:?}");

        let base = payload(baseline[0]).added;
        assert_eq!(
            base.get("SCRIBE_PROBE_PATHVAR").map(String::as_str),
            Some("/probe/one:/probe/two")
        );
        assert_eq!(base.get("SCRIBE_PROBE_FAKEPATH").map(String::as_str), Some("a b"));
        assert_eq!(base.get("SCRIBE_PROBE_PLAIN").map(String::as_str), Some("one two"));
        let child_path = base.get("SCRIBE_PROBE_CHILD_PATH").expect("child PATH probe missing");
        assert!(child_path.contains(':'), "test PATH has nothing to join: {child_path}");
        assert_eq!(base.get("PATH"), Some(child_path), "baseline PATH is not what a child sees");

        // Restoring the recorded value the way `render_fish_restore`
        // writes it back leaves the session with the same PATH: fish
        // re-splits a path variable on the colon, so the list keeps its
        // entries and the delta reports no change to PATH at all. A
        // space-joined record instead restores as a single entry, and
        // the driver's own `sh` lookups stop resolving.
        assert_eq!(deltas.len(), 1, "expected one per-prompt delta emit, got {calls:?}");
        let delta = payload(deltas[0]).added;
        assert_eq!(
            delta.get("SCRIBE_PROBE_RESTORED_PATH"),
            Some(child_path),
            "restoring the recorded PATH changed what a child sees"
        );
        let restored_count: usize = delta
            .get("SCRIBE_PROBE_RESTORED_COUNT")
            .expect("restored PATH element count missing")
            .parse()
            .expect("element count is not a number");
        assert!(restored_count > 1, "restored PATH collapsed to {restored_count} entry");
        assert!(!delta.contains_key("PATH"), "PATH did not round-trip unchanged: {delta:?}");
        assert_eq!(
            delta.get("SCRIBE_PROBE_PATHVAR").map(String::as_str),
            Some("/probe/one:/probe/two:/probe/three")
        );
    }

    /// Even a well-formed payload is lost if the interpolation is
    /// unquoted: fish drops an argument entirely when the variable is a
    /// zero-element list, which is silent rather than a parse error. The
    /// payload now rides `printf`'s arguments into the helper's stdin, so
    /// that is where the quoting has to hold. The baseline and the
    /// per-prompt delta share one emit site, so there is exactly one such
    /// line and the two payloads cannot drift apart.
    #[test]
    fn fish_payload_interpolations_are_quoted() {
        let script = std::fs::read_to_string(fish_script()).expect("read fish integration");
        let emits: Vec<&str> =
            script.lines().filter(|line| line.trim_start().starts_with("printf '{")).collect();
        assert_eq!(emits.len(), 1, "expected the one shared payload emit, got {emits:?}");
        for line in emits {
            assert!(
                first_unquoted_interpolation(line).is_none(),
                "payload interpolates an unquoted variable; fish drops the whole \
                 argument when the list is empty: {line}"
            );
        }
    }

    /// [`inject_nushell`] prepends the scripts directory to
    /// `XDG_DATA_DIRS` so nushell autoloads `scribe.nu`; once autoload is
    /// over the script has to take the entry back out again. Both escape
    /// routes are asserted against the shipped script: what a child
    /// process inherits, and what the snapshot behind the env baseline
    /// would hand the server to persist.
    #[test]
    fn nu_strips_the_injected_xdg_data_dirs_entry() {
        let mut probe = Command::new("nu");
        probe.arg("--version");
        scrub_desktop_env(&mut probe);
        if !probe.output().is_ok_and(|out| out.status.success()) {
            return;
        }

        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).expect("clock").as_nanos();
        let dir =
            std::env::temp_dir().join(format!("scribe-nu-xdg-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create scratch dir");

        // A child sees only what nushell exports, so reading the variable
        // back out of one is the direct test of the leak.
        let recorder = dir.join("recorder.sh");
        std::fs::write(&recorder, "#!/bin/sh\nprintf 'child=%s' \"${XDG_DATA_DIRS-}\" > \"$1\"\n")
            .expect("write recorder");
        std::fs::set_permissions(&recorder, std::fs::Permissions::from_mode(0o755))
            .expect("chmod recorder");

        let child_seen = dir.join("child.txt");
        let baseline_seen = dir.join("baseline.txt");
        let driver = dir.join("driver.nu");
        std::fs::write(
            &driver,
            format!(
                "source '{}'\nrun-external '{}' '{}'\nlet seen = (__scribe-snapshot-env | where \
                 name == 'XDG_DATA_DIRS' | get value | str join ',')\n$\"baseline=($seen)\" | save \
                 --raw --force '{}'\n",
                nu_script().display(),
                recorder.display(),
                child_seen.display(),
                baseline_seen.display(),
            ),
        )
        .expect("write driver");

        let injected = dir.join("shell-integration");
        let mut command = Command::new("nu");
        command
            .arg("--no-config-file")
            .arg(&driver)
            .env("TERM_PROGRAM", "Scribe")
            .env("SCRIBE_SHELL_INTEGRATION", "1")
            // The baseline emit itself is out of scope here and would fork
            // a helper that does not exist in a test layout.
            .env("SCRIBE_ENV_PERSIST", "0")
            .env_remove("_SCRIBE_INTEGRATION_SOURCED")
            .env("XDG_DATA_DIRS", format!("{}:/usr/local/share:/usr/share", injected.display()))
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped());
        seal_child(&mut command, &dir);
        let result = command.output().expect("run nu driver");
        assert!(
            result.status.success(),
            "nu driver exited with {}: {}",
            result.status,
            String::from_utf8_lossy(&result.stderr)
        );

        let child = std::fs::read_to_string(&child_seen).expect("recorder output");
        let baseline = std::fs::read_to_string(&baseline_seen).expect("baseline output");
        std::fs::remove_dir_all(&dir).expect("clean scratch dir");
        assert_eq!(child, "child=/usr/local/share:/usr/share", "nu leaked the injected entry");
        assert_eq!(
            baseline, "baseline=/usr/local/share:/usr/share",
            "the env baseline would persist the injected entry"
        );
    }

    /// Byte offset of the first `$` not immediately preceded by a double
    /// quote, i.e. the first interpolation fish could silently drop.
    fn first_unquoted_interpolation(line: &str) -> Option<usize> {
        let chars: Vec<char> = line.chars().collect();
        chars.iter().enumerate().find_map(|(idx, ch)| {
            let prev = idx.checked_sub(1).map(|p| chars[p]);
            (*ch == '$' && prev != Some('"')).then_some(idx)
        })
    }
}
