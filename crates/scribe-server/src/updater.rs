use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use scribe_common::app::current_identity;
use serde::Deserialize;
use tokio::sync::RwLock;
use tracing::{error, info, warn};

use scribe_common::config::{UpdateChannel, UpdateConfig};
use scribe_common::error::ScribeError;
use scribe_common::protocol::{ServerMessage, UpdateProgressState};

use crate::ipc_server::ConnectedClients;

const INITIAL_DELAY: Duration = Duration::from_secs(30);
#[cfg(target_os = "macos")]
const HOT_RELOAD_HANDOFF_TIMEOUT: Duration = Duration::from_secs(30);
const GITHUB_API_URL: &str = "https://api.github.com/repos/sharaf-nassar/scribe/releases/latest";
/// Minisign public key for verifying release signatures.
const MINISIGN_PUBLIC_KEY: &str = "RWSEN3ob4jI+FaJ5K+IIhUKdE6GZ9PvrCilK9ra2n/ajSZO6u6uRuILJ";

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
const STABLE_ASSET_SUFFIX: &str = "linux-x86_64.deb";

#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
const STABLE_ASSET_SUFFIX: &str = "linux-arm64.deb";

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
const STABLE_ASSET_SUFFIX: &str = "macos-arm64.dmg";

#[cfg(all(target_os = "macos", target_arch = "x86_64"))]
const STABLE_ASSET_SUFFIX: &str = "macos-x86_64.dmg";

// ── GitHub API types ──────────────────────────────────────────────

#[derive(Deserialize)]
struct GhRelease {
    tag_name: String,
    html_url: String,
    assets: Vec<GhAsset>,
    draft: bool,
    prerelease: bool,
}

#[derive(Deserialize)]
struct GhAsset {
    name: String,
    browser_download_url: String,
}

// ── Public API ────────────────────────────────────────────────────

/// Handle for controlling the background updater task.
pub struct UpdaterHandle {
    trigger_tx: tokio::sync::mpsc::Sender<()>,
    dismiss_tx: tokio::sync::mpsc::Sender<()>,
}

impl UpdaterHandle {
    /// Signal the updater to begin downloading and installing the latest version.
    pub fn trigger(&self) {
        if self.trigger_tx.try_send(()).is_err() {
            warn!("updater trigger channel full or closed");
        }
    }

    /// Signal the updater to suppress re-notification for the current version.
    pub fn dismiss(&self) {
        if self.dismiss_tx.try_send(()).is_err() {
            warn!("updater dismiss channel full or closed");
        }
    }
}

fn api_url() -> String {
    std::env::var("SCRIBE_UPDATE_API_URL").unwrap_or_else(|_| GITHUB_API_URL.to_owned())
}

fn asset_suffix() -> Option<&'static str> {
    (!current_identity().is_dev()).then_some(STABLE_ASSET_SUFFIX)
}

/// Spawn the background updater task and return a handle for IPC control.
pub fn spawn_updater(connected_clients: ConnectedClients, config: UpdateConfig) -> UpdaterHandle {
    let (trigger_tx, trigger_rx) = tokio::sync::mpsc::channel(1);
    let (dismiss_tx, dismiss_rx) = tokio::sync::mpsc::channel(1);

    tokio::spawn(run_updater_loop(connected_clients, trigger_rx, dismiss_rx, config));

    UpdaterHandle { trigger_tx, dismiss_tx }
}

// ── Background loop ───────────────────────────────────────────────

async fn run_updater_loop(
    connected_clients: ConnectedClients,
    mut trigger_rx: tokio::sync::mpsc::Receiver<()>,
    mut dismiss_rx: tokio::sync::mpsc::Receiver<()>,
    config: UpdateConfig,
) {
    if !config.enabled {
        info!("auto-update disabled by config");
        return;
    }

    tokio::time::sleep(INITIAL_DELAY).await;

    let http = match reqwest::Client::builder()
        .user_agent(format!("scribe/{}", env!("CARGO_PKG_VERSION")))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            error!("failed to build HTTP client for updater: {e}");
            return;
        }
    };

    // Track which version we last notified about so dismiss works correctly.
    let dismissed: Arc<RwLock<Option<String>>> = Arc::new(RwLock::new(None));

    let check_interval = Duration::from_secs(config.check_interval_secs.max(300));
    let mut interval = tokio::time::interval(check_interval);
    // The first tick fires immediately; we handle the initial check inline
    // after INITIAL_DELAY has already elapsed, so skip that first tick.
    interval.tick().await;

    loop {
        tokio::select! {
            _ = interval.tick() => {
                run_check(&http, &connected_clients, &dismissed, config.channel).await;
            }
            Some(()) = trigger_rx.recv() => {
                run_install(&http, &connected_clients).await;
            }
            Some(()) = dismiss_rx.recv() => {
                info!("update notification dismissed by user");
            }
        }
    }
}

/// Calls `check_for_update` up to two times, waiting 5 s between attempts.
async fn check_for_update_with_retry(
    client: &reqwest::Client,
    channel: UpdateChannel,
) -> Result<Option<(String, String)>, ScribeError> {
    const RETRY_DELAY: Duration = Duration::from_secs(5);
    match check_for_update(client, channel).await {
        Ok(v) => Ok(v),
        Err(e) => {
            warn!("update check attempt 1 failed: {e}; retrying in {RETRY_DELAY:?}");
            tokio::time::sleep(RETRY_DELAY).await;
            check_for_update(client, channel).await
        }
    }
}

async fn run_check(
    client: &reqwest::Client,
    connected_clients: &ConnectedClients,
    dismissed: &Arc<RwLock<Option<String>>>,
    channel: UpdateChannel,
) {
    match check_for_update_with_retry(client, channel).await {
        Ok(Some((version, release_url))) => {
            let already_dismissed = dismissed.read().await.as_deref() == Some(version.as_str());
            if already_dismissed {
                info!(%version, "update available but dismissed by user");
                return;
            }
            info!(%version, "update available — notifying clients");
            *dismissed.write().await = Some(version.clone());
            let msg = ServerMessage::UpdateAvailable { version: version.clone(), release_url };
            broadcast(&msg, connected_clients).await;
        }
        Ok(None) => {
            info!("no update available");
            *dismissed.write().await = None;
        }
        Err(e) => {
            warn!("update check failed: {e}");
        }
    }
}

async fn run_install(client: &reqwest::Client, connected_clients: &ConnectedClients) {
    info!("user triggered update — starting download");
    match try_install(client, connected_clients).await {
        Ok((version, hot_reload_succeeded)) => {
            info!(%version, "update installed successfully");
            let state = if hot_reload_succeeded {
                UpdateProgressState::Completed { version }
            } else {
                UpdateProgressState::CompletedRestartRequired { version }
            };
            let msg = ServerMessage::UpdateProgress { state };
            broadcast(&msg, connected_clients).await;
        }
        Err(e) => {
            error!("update install failed: {e}");
            let msg = ServerMessage::UpdateProgress {
                state: UpdateProgressState::Failed { reason: format!("{e}") },
            };
            broadcast(&msg, connected_clients).await;
        }
    }
}

/// Runs all download/verify/install steps and returns the installed version string
/// and whether hot-reload succeeded. Broadcasts progress messages along the way
/// but returns errors to the caller.
async fn try_install(
    client: &reqwest::Client,
    connected_clients: &ConnectedClients,
) -> Result<(String, bool), ScribeError> {
    let (asset_url, sig_url, version) = fetch_asset_urls(client).await?;

    broadcast(
        &ServerMessage::UpdateProgress { state: UpdateProgressState::Downloading },
        connected_clients,
    )
    .await;

    let (asset_path, sig_path) = download_both(client, &asset_url, &sig_url).await?;

    broadcast(
        &ServerMessage::UpdateProgress { state: UpdateProgressState::Verifying },
        connected_clients,
    )
    .await;

    verify_signature(&asset_path, &sig_path)?;

    broadcast(
        &ServerMessage::UpdateProgress { state: UpdateProgressState::Installing },
        connected_clients,
    )
    .await;

    let hot_reload_succeeded = install_update(&asset_path)?;

    Ok((version, hot_reload_succeeded))
}

// ── Core update logic ─────────────────────────────────────────────

/// Fetches and deserialises the latest GitHub release.
async fn fetch_latest_release(client: &reqwest::Client) -> Result<GhRelease, ScribeError> {
    client
        .get(api_url())
        .send()
        .await
        .map_err(|e| ScribeError::UpdateCheckFailed { reason: format!("{e}") })?
        .error_for_status()
        .map_err(|e| ScribeError::UpdateCheckFailed { reason: format!("{e}") })?
        .json()
        .await
        .map_err(|e| ScribeError::UpdateCheckFailed { reason: format!("{e}") })
}

/// Checks GitHub releases API. Returns `Some((version, release_url))` if
/// a release newer than the running binary is available.
async fn check_for_update(
    client: &reqwest::Client,
    channel: UpdateChannel,
) -> Result<Option<(String, String)>, ScribeError> {
    if current_identity().is_dev() {
        return Ok(None);
    }

    let release = fetch_latest_release(client).await?;

    if release.draft || (release.prerelease && channel == UpdateChannel::Stable) {
        return Ok(None);
    }

    let remote_ver = parse_version(&release.tag_name)?;
    let local_ver = current_version()?;

    if remote_ver > local_ver {
        Ok(Some((release.tag_name.trim_start_matches('v').to_owned(), release.html_url)))
    } else {
        Ok(None)
    }
}

/// Fetches the latest release and returns `(asset_url, sig_url, version)`.
async fn fetch_asset_urls(
    client: &reqwest::Client,
) -> Result<(String, String, String), ScribeError> {
    let Some(asset_suffix) = asset_suffix() else {
        return Err(ScribeError::UpdateInstallFailed {
            reason: String::from("auto-update is disabled for scribe-dev installs"),
        });
    };

    let release = fetch_latest_release(client).await?;

    let asset = find_asset(&release.assets, asset_suffix).ok_or_else(|| {
        ScribeError::UpdateInstallFailed {
            reason: format!("no asset matching '{asset_suffix}' in release"),
        }
    })?;

    let sig = find_signature(&release.assets, &asset.name).ok_or_else(|| {
        ScribeError::UpdateInstallFailed {
            reason: format!("no .minisig for asset '{}'", asset.name),
        }
    })?;

    let version = release.tag_name.trim_start_matches('v').to_owned();
    Ok((asset.browser_download_url.clone(), sig.browser_download_url.clone(), version))
}

fn find_asset<'a>(assets: &'a [GhAsset], asset_suffix: &str) -> Option<&'a GhAsset> {
    assets.iter().find(|a| a.name.ends_with(asset_suffix))
}

fn find_signature<'a>(assets: &'a [GhAsset], asset_name: &str) -> Option<&'a GhAsset> {
    let sig_name = format!("{asset_name}.minisig");
    assets.iter().find(|a| a.name == sig_name)
}

/// Downloads a URL to a temp file and returns the path.
async fn download_asset(client: &reqwest::Client, url: &str) -> Result<PathBuf, ScribeError> {
    use futures_util::StreamExt as _;
    use tokio::io::AsyncWriteExt as _;

    let tmp_dir = std::env::temp_dir();
    let filename = url
        .rsplit('/')
        .next()
        .ok_or_else(|| ScribeError::UpdateInstallFailed { reason: "empty asset URL".into() })?;
    let dest = tmp_dir.join(filename);

    let response = client
        .get(url)
        .send()
        .await
        .map_err(|e| ScribeError::UpdateInstallFailed { reason: format!("{e}") })?
        .error_for_status()
        .map_err(|e| ScribeError::UpdateInstallFailed { reason: format!("{e}") })?;

    let mut file =
        tokio::fs::File::create(&dest).await.map_err(|e| ScribeError::Io { source: e })?;

    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk =
            chunk.map_err(|e| ScribeError::UpdateInstallFailed { reason: format!("{e}") })?;
        file.write_all(&chunk).await.map_err(|e| ScribeError::Io { source: e })?;
    }

    file.flush().await.map_err(|e| ScribeError::Io { source: e })?;

    Ok(dest)
}

/// Download the asset and its signature file concurrently.
async fn download_both(
    client: &reqwest::Client,
    asset_url: &str,
    sig_url: &str,
) -> Result<(PathBuf, PathBuf), ScribeError> {
    let (asset_res, sig_res) =
        tokio::join!(download_asset(client, asset_url), download_asset(client, sig_url));
    Ok((asset_res?, sig_res?))
}

fn verify_signature(asset_path: &Path, sig_path: &Path) -> Result<(), ScribeError> {
    let pk = minisign_verify::PublicKey::from_base64(MINISIGN_PUBLIC_KEY)
        .map_err(|e| ScribeError::UpdateInstallFailed { reason: format!("bad public key: {e}") })?;

    let sig_bytes = std::fs::read(sig_path).map_err(|e| ScribeError::Io { source: e })?;
    let sig =
        minisign_verify::Signature::decode(&String::from_utf8_lossy(&sig_bytes)).map_err(|e| {
            ScribeError::UpdateInstallFailed { reason: format!("bad signature file: {e}") }
        })?;

    let asset_bytes = std::fs::read(asset_path).map_err(|e| ScribeError::Io { source: e })?;
    pk.verify(&asset_bytes, &sig, false).map_err(|e| ScribeError::UpdateInstallFailed {
        reason: format!("signature mismatch: {e}"),
    })
}

#[cfg(target_os = "linux")]
fn install_update(asset_path: &Path) -> Result<bool, ScribeError> {
    let path_str = asset_path.to_string_lossy();
    let status = std::process::Command::new("pkexec")
        .args(["dpkg", "-i", &path_str])
        .status()
        .map_err(|e| ScribeError::UpdateInstallFailed {
            reason: format!("failed to launch pkexec dpkg: {e}"),
        })?;

    if status.success() {
        Ok(true)
    } else {
        Err(ScribeError::UpdateInstallFailed {
            reason: format!(
                "pkexec dpkg -i exited with {status}; \
                 ensure policykit is installed and the user is authorized"
            ),
        })
    }
}

#[cfg(target_os = "macos")]
fn install_update(asset_path: &Path) -> Result<bool, ScribeError> {
    use scribe_common::socket::handoff_socket_path;
    use std::collections::HashMap;
    use std::process::Stdio;

    let app_bundle_path = current_app_bundle_path()?;
    let prev_path = app_bundle_path.with_extension("app.prev");
    let path_str = asset_path.to_string_lossy();

    // Attach the DMG.
    let attach = std::process::Command::new("hdiutil")
        .args(["attach", "-nobrowse", "-quiet", &path_str])
        .output()
        .map_err(|e| ScribeError::UpdateInstallFailed {
            reason: format!("hdiutil attach failed: {e}"),
        })?;

    if !attach.status.success() {
        return Err(ScribeError::UpdateInstallFailed {
            reason: format!(
                "hdiutil attach exited with {}: {}",
                attach.status,
                String::from_utf8_lossy(&attach.stderr)
            ),
        });
    }

    // The mount point is the last whitespace-separated token on the last line.
    let stdout = String::from_utf8_lossy(&attach.stdout);
    let mount_point = stdout
        .lines()
        .last()
        .and_then(|l| l.split_whitespace().last())
        .ok_or_else(|| ScribeError::UpdateInstallFailed {
            reason: "could not parse hdiutil mount point".into(),
        })?
        .to_owned();

    // Capture which client processes are running before the update.
    let is_running = |name: &str| -> bool {
        std::process::Command::new("pgrep")
            .args(["-x", name])
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    };
    let client_was_running = is_running("scribe-client");
    let settings_was_running = is_running("scribe-settings");

    // Remove any stale .app.prev left by a previous failed update so the
    // upcoming rename doesn't collide with it.
    if prev_path.exists() {
        let _ = std::fs::remove_dir_all(&prev_path);
    }

    // Rename existing app to .app.prev for rollback (O(rename), same filesystem).
    // If it doesn't exist (fresh install), continue without backup.
    let backup_existed = std::fs::rename(&app_bundle_path, &prev_path).is_ok();

    let app_src = Path::new(&mount_point).join(current_identity().app_bundle_name());
    let ditto_result = std::process::Command::new("ditto")
        .arg(&app_src)
        .arg(&app_bundle_path)
        .status()
        .map_err(|e| ScribeError::UpdateInstallFailed { reason: format!("ditto failed: {e}") });

    // Always attempt to detach, even if ditto failed.
    let detach =
        std::process::Command::new("hdiutil").args(["detach", "-quiet", &mount_point]).status();
    if let Err(ref e) = detach {
        warn!("hdiutil detach failed: {e}");
    }

    let ditto_status = match ditto_result {
        Err(e) => {
            // ditto could not be launched — restore backup if we have one.
            if backup_existed {
                if let Err(re) = std::fs::rename(&prev_path, &app_bundle_path) {
                    warn!("rollback rename failed: {re}");
                }
            }
            return Err(e);
        }
        Ok(s) => s,
    };

    if !ditto_status.success() {
        // ditto ran but failed — restore backup if we have one.
        if backup_existed {
            if let Err(re) = std::fs::rename(&prev_path, &app_bundle_path) {
                warn!("rollback rename failed: {re}");
            }
        }
        return Err(ScribeError::UpdateInstallFailed {
            reason: format!("ditto exited with {ditto_status}"),
        });
    }

    // Compare old and new binaries to determine which components need restart.
    // If no backup existed (fresh install), treat all as changed.
    let binaries = ["scribe-server", "scribe-client", "scribe-settings"];
    let mut changed: HashMap<&str, bool> = HashMap::new();
    for name in &binaries {
        let differs = if backup_existed {
            let old_path = prev_path.join("Contents/MacOS").join(name);
            let new_path = app_bundle_path.join("Contents/MacOS").join(name);
            file_hash_differs(&old_path, &new_path)
        } else {
            true
        };
        changed.insert(name, differs);
    }

    // Remove the backup now that hash comparison is complete (best-effort).
    if backup_existed {
        if let Err(e) = std::fs::remove_dir_all(&prev_path) {
            warn!("failed to remove .app.prev backup: {e}");
        }
    }

    // Restart the server: try launchctl kickstart first, fall back to direct spawn.
    let handoff_path = handoff_socket_path();

    let wait_for_handoff = || -> bool {
        let deadline = std::time::Instant::now() + HOT_RELOAD_HANDOFF_TIMEOUT;
        let mut handed_off = false;
        while std::time::Instant::now() < deadline {
            if !handoff_path.exists() {
                handed_off = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(500));
        }
        if !handed_off {
            warn!(
                timeout_secs = HOT_RELOAD_HANDOFF_TIMEOUT.as_secs(),
                "hot-reload handoff timed out"
            );
        }
        handed_off
    };

    let server_changed = *changed.get("scribe-server").unwrap_or(&true);
    let hot_reload_succeeded = if server_changed {
        let uid = scribe_common::socket::current_uid();
        let service_target = format!("user/{uid}/com.scribe.server");

        let launchctl_ok = std::process::Command::new("launchctl")
            .args(["kickstart", "-k", &service_target])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);

        if launchctl_ok {
            info!("launchctl kickstart succeeded — waiting for handoff");
            wait_for_handoff()
        } else {
            info!("launchctl kickstart unavailable — falling back to direct --upgrade spawn");
            match std::env::current_exe() {
                Err(e) => {
                    warn!("could not determine current exe path for --upgrade spawn: {e}");
                    false
                }
                Ok(exe) => {
                    match std::process::Command::new(&exe)
                        .arg("--upgrade")
                        .stdin(Stdio::null())
                        .stdout(Stdio::null())
                        .stderr(Stdio::null())
                        .spawn()
                    {
                        Err(e) => {
                            warn!("failed to spawn new server with --upgrade: {e}");
                            false
                        }
                        Ok(_child) => wait_for_handoff(),
                    }
                }
            }
        }
    } else {
        info!("server binary unchanged — skipping server restart");
        true
    };

    // Restart client binaries that changed and were running before the update.
    let macos_dir = app_bundle_path.join("Contents/MacOS");
    for &name in &["scribe-client", "scribe-settings"] {
        if !changed.get(name).unwrap_or(&true) {
            info!("{name} binary unchanged — skipping restart");
            continue;
        }
        let was_running = match name {
            "scribe-client" => client_was_running,
            "scribe-settings" => settings_was_running,
            _ => false,
        };
        if !was_running {
            continue;
        }
        // Kill the old process (best-effort, it may not be running).
        let _ = std::process::Command::new("pkill").args(["-x", name]).status();
        // Brief wait for singleton socket release (settings).
        if name != "scribe-client" {
            std::thread::sleep(std::time::Duration::from_millis(500));
        }
        // Relaunch.
        let bin_path = macos_dir.join(name);
        match std::process::Command::new(&bin_path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(_) => info!("relaunched {name}"),
            Err(e) => warn!("failed to relaunch {name}: {e}"),
        }
    }

    Ok(hot_reload_succeeded)
}

/// Returns `true` if the two files have different content (or if either cannot be read).
/// Safety-first: any failure to compare is treated as "changed".
#[cfg(target_os = "macos")]
fn file_hash_differs(old_path: &Path, new_path: &Path) -> bool {
    use sha2::{Digest, Sha256};
    let hash_file = |path: &Path| -> Option<[u8; 32]> {
        let data = std::fs::read(path).ok()?;
        Some(Sha256::digest(&data).into())
    };
    match (hash_file(old_path), hash_file(new_path)) {
        (Some(old), Some(new)) => old != new,
        _ => true,
    }
}

#[cfg(target_os = "macos")]
fn current_app_bundle_path() -> Result<PathBuf, ScribeError> {
    let exe = std::env::current_exe().map_err(|e| ScribeError::UpdateInstallFailed {
        reason: format!("failed to resolve current executable path: {e}"),
    })?;
    exe.ancestors()
        .find(|path| path.extension().is_some_and(|ext| ext == "app"))
        .map(Path::to_path_buf)
        .ok_or_else(|| ScribeError::UpdateInstallFailed {
            reason: format!("current executable is not inside an app bundle: {}", exe.display()),
        })
}

fn parse_version(tag: &str) -> Result<semver::Version, ScribeError> {
    let stripped = tag.trim_start_matches('v');
    semver::Version::parse(stripped)
        .map_err(|e| ScribeError::UpdateCheckFailed { reason: format!("bad version '{tag}': {e}") })
}

fn current_version() -> Result<semver::Version, ScribeError> {
    semver::Version::parse(env!("CARGO_PKG_VERSION")).map_err(|error| {
        ScribeError::UpdateCheckFailed {
            reason: format!("invalid CARGO_PKG_VERSION '{}': {error}", env!("CARGO_PKG_VERSION")),
        }
    })
}

// ── Broadcast helper ──────────────────────────────────────────────

async fn broadcast(msg: &ServerMessage, connected_clients: &ConnectedClients) {
    use scribe_common::framing::write_message;
    let clients = connected_clients.read().await;
    for writer in clients.values() {
        let mut w = writer.lock().await;
        if let Err(e) = write_message(&mut *w, msg).await {
            warn!("failed to broadcast update message to client: {e}");
        }
    }
}
