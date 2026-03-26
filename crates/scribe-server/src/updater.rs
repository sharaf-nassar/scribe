use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use tokio::sync::RwLock;
use tracing::{error, info, warn};

use scribe_common::error::ScribeError;
use scribe_common::protocol::{ServerMessage, UpdateProgressState};

use crate::ipc_server::ConnectedClients;

const CHECK_INTERVAL: Duration = Duration::from_secs(86_400);
const INITIAL_DELAY: Duration = Duration::from_secs(30);
const GITHUB_API_URL: &str = "https://api.github.com/repos/sharaf-nassar/scribe/releases/latest";
/// Minisign public key for verifying release signatures.
/// Generate a keypair with `minisign -G` and replace this value with the public key.
/// The corresponding secret key goes into the `MINISIGN_SECRET_KEY` GitHub secret.
const MINISIGN_PUBLIC_KEY: &str = "RWXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXX";

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
const ASSET_SUFFIX: &str = "linux-x86_64.deb";

#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
const ASSET_SUFFIX: &str = "linux-arm64.deb";

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
const ASSET_SUFFIX: &str = "macos-arm64.dmg";

#[cfg(all(target_os = "macos", target_arch = "x86_64"))]
const ASSET_SUFFIX: &str = "macos-x86_64.dmg";

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

/// Spawn the background updater task and return a handle for IPC control.
pub fn spawn_updater(connected_clients: ConnectedClients) -> UpdaterHandle {
    let (trigger_tx, trigger_rx) = tokio::sync::mpsc::channel(1);
    let (dismiss_tx, dismiss_rx) = tokio::sync::mpsc::channel(1);

    tokio::spawn(run_updater_loop(connected_clients, trigger_rx, dismiss_rx));

    UpdaterHandle { trigger_tx, dismiss_tx }
}

// ── Background loop ───────────────────────────────────────────────

async fn run_updater_loop(
    connected_clients: ConnectedClients,
    mut trigger_rx: tokio::sync::mpsc::Receiver<()>,
    mut dismiss_rx: tokio::sync::mpsc::Receiver<()>,
) {
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

    let mut interval = tokio::time::interval(CHECK_INTERVAL);
    // The first tick fires immediately; we handle the initial check inline
    // after INITIAL_DELAY has already elapsed, so skip that first tick.
    interval.tick().await;

    loop {
        tokio::select! {
            _ = interval.tick() => {
                run_check(&http, &connected_clients, &dismissed).await;
            }
            Some(()) = trigger_rx.recv() => {
                run_install(&http, &connected_clients).await;
            }
            Some(()) = dismiss_rx.recv() => {
                info!("update notification dismissed by user");
                // Nothing to do — the dismissed version is cleared on next check.
            }
        }
    }
}

async fn run_check(
    client: &reqwest::Client,
    connected_clients: &ConnectedClients,
    dismissed: &Arc<RwLock<Option<String>>>,
) {
    match check_for_update(client).await {
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
        Ok(version) => {
            info!(%version, "update installed successfully");
            let msg =
                ServerMessage::UpdateProgress { state: UpdateProgressState::Completed { version } };
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

/// Runs all download/verify/install steps and returns the installed version string.
/// Broadcasts progress messages along the way but returns errors to the caller.
async fn try_install(
    client: &reqwest::Client,
    connected_clients: &ConnectedClients,
) -> Result<String, ScribeError> {
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

    install_update(&asset_path)?;

    Ok(version)
}

// ── Core update logic ─────────────────────────────────────────────

/// Checks GitHub releases API. Returns `Some((version, release_url))` if
/// a release newer than the running binary is available.
async fn check_for_update(
    client: &reqwest::Client,
) -> Result<Option<(String, String)>, ScribeError> {
    let release: GhRelease = client
        .get(GITHUB_API_URL)
        .send()
        .await
        .map_err(|e| ScribeError::UpdateCheckFailed { reason: format!("{e}") })?
        .error_for_status()
        .map_err(|e| ScribeError::UpdateCheckFailed { reason: format!("{e}") })?
        .json()
        .await
        .map_err(|e| ScribeError::UpdateCheckFailed { reason: format!("{e}") })?;

    if release.draft || release.prerelease {
        return Ok(None);
    }

    let remote_ver = parse_version(&release.tag_name)?;
    let local_ver = current_version();

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
    let release: GhRelease = client
        .get(GITHUB_API_URL)
        .send()
        .await
        .map_err(|e| ScribeError::UpdateCheckFailed { reason: format!("{e}") })?
        .error_for_status()
        .map_err(|e| ScribeError::UpdateCheckFailed { reason: format!("{e}") })?
        .json()
        .await
        .map_err(|e| ScribeError::UpdateCheckFailed { reason: format!("{e}") })?;

    let asset = find_asset(&release.assets).ok_or_else(|| ScribeError::UpdateInstallFailed {
        reason: format!("no asset matching '{ASSET_SUFFIX}' in release"),
    })?;

    let sig = find_signature(&release.assets, &asset.name).ok_or_else(|| {
        ScribeError::UpdateInstallFailed {
            reason: format!("no .minisig for asset '{}'", asset.name),
        }
    })?;

    let version = release.tag_name.trim_start_matches('v').to_owned();
    Ok((asset.browser_download_url.clone(), sig.browser_download_url.clone(), version))
}

fn find_asset(assets: &[GhAsset]) -> Option<&GhAsset> {
    assets.iter().find(|a| a.name.ends_with(ASSET_SUFFIX))
}

fn find_signature<'a>(assets: &'a [GhAsset], asset_name: &str) -> Option<&'a GhAsset> {
    let sig_name = format!("{asset_name}.minisig");
    assets.iter().find(|a| a.name == sig_name)
}

/// Downloads a URL to a temp file and returns the path.
async fn download_asset(client: &reqwest::Client, url: &str) -> Result<PathBuf, ScribeError> {
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

    let bytes = response
        .bytes()
        .await
        .map_err(|e| ScribeError::UpdateInstallFailed { reason: format!("{e}") })?;

    let mut file =
        tokio::fs::File::create(&dest).await.map_err(|e| ScribeError::Io { source: e })?;
    file.write_all(&bytes).await.map_err(|e| ScribeError::Io { source: e })?;

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
    let pk = minisign_verify::PublicKey::decode(MINISIGN_PUBLIC_KEY)
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
fn install_update(asset_path: &Path) -> Result<(), ScribeError> {
    let path_str = asset_path.to_string_lossy();
    let status = std::process::Command::new("pkexec")
        .args(["dpkg", "-i", &path_str])
        .status()
        .map_err(|e| ScribeError::UpdateInstallFailed {
            reason: format!("failed to launch pkexec dpkg: {e}"),
        })?;

    if status.success() {
        Ok(())
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
fn install_update(asset_path: &Path) -> Result<(), ScribeError> {
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

    let app_src = format!("{mount_point}/Scribe.app");
    let result = std::process::Command::new("ditto")
        .args([&app_src, "/Applications/Scribe.app"])
        .status()
        .map_err(|e| ScribeError::UpdateInstallFailed { reason: format!("ditto failed: {e}") });

    // Always attempt to detach, even if ditto failed.
    let detach =
        std::process::Command::new("hdiutil").args(["detach", "-quiet", &mount_point]).status();
    if let Err(ref e) = detach {
        warn!("hdiutil detach failed: {e}");
    }

    let status = result?;
    if status.success() {
        Ok(())
    } else {
        Err(ScribeError::UpdateInstallFailed { reason: format!("ditto exited with {status}") })
    }
}

fn parse_version(tag: &str) -> Result<semver::Version, ScribeError> {
    let stripped = tag.trim_start_matches('v');
    semver::Version::parse(stripped)
        .map_err(|e| ScribeError::UpdateCheckFailed { reason: format!("bad version '{tag}': {e}") })
}

fn current_version() -> semver::Version {
    #[allow(clippy::unwrap_used, reason = "CARGO_PKG_VERSION is always valid semver set by Cargo")]
    semver::Version::parse(env!("CARGO_PKG_VERSION")).unwrap()
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
