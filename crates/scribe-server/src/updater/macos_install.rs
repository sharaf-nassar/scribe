//! Mount-and-swap step of the macOS update install.
//!
//! Split out from [`super::install_update`] so the parts that decide *what*
//! happens — where the DMG mounts, when the previous bundle is moved aside,
//! which failures roll back, and the guarantee that the mount is always
//! released — can be exercised without a real DMG or a real `/Applications`.
//! Only [`SystemHost`] shells out, and only it is macOS-gated; everything
//! else compiles and tests on every platform, including Linux CI.
//!
//! The invariant this module exists to protect: the mount point is *chosen by
//! the caller and passed to `hdiutil`*, never recovered by parsing `hdiutil`'s
//! output. Parsing was the original bug — `-quiet` suppresses the mount table
//! entirely, so the parse could never succeed — and it is also unsound even
//! without `-quiet`, because a second mount of the same volume name lands at
//! `/Volumes/<name> 1` and a whitespace-split reads back `1`.

use std::path::Path;

use tracing::warn;

/// Command line for attaching `dmg` at `mount_point`.
///
/// `-mountpoint` pins the location, so no output parsing is needed and
/// `/Volumes` name collisions cannot misdirect the copy.
#[must_use]
pub fn attach_args(dmg: &Path, mount_point: &Path) -> Vec<String> {
    vec![
        "attach".to_owned(),
        "-nobrowse".to_owned(),
        "-readonly".to_owned(),
        "-quiet".to_owned(),
        "-mountpoint".to_owned(),
        mount_point.to_string_lossy().into_owned(),
        dmg.to_string_lossy().into_owned(),
    ]
}

/// Command line for releasing the mount created by [`attach_args`], or any
/// device node reported by [`attached_devices_for`].
#[must_use]
pub fn detach_args(target: &Path) -> Vec<String> {
    vec!["detach".to_owned(), "-quiet".to_owned(), target.to_string_lossy().into_owned()]
}

/// `hdiutil info -plist`, as much of it as the teardown needs.
#[derive(serde::Deserialize)]
struct HdiutilInfo {
    #[serde(default)]
    images: Vec<HdiutilImage>,
}

#[derive(serde::Deserialize)]
struct HdiutilImage {
    #[serde(rename = "image-path")]
    image_path: String,
    #[serde(rename = "system-entities", default)]
    system_entities: Vec<HdiutilEntity>,
}

#[derive(serde::Deserialize)]
struct HdiutilEntity {
    #[serde(rename = "dev-entry")]
    dev_entry: Option<String>,
}

/// Whole-disk device nodes `hdiutil` reports as attached for `dmg`.
///
/// Parses the `-plist` form, hdiutil's documented machine-readable interface.
/// The first entity of an image is its whole-disk device, and detaching that
/// tears down the partition and container nodes with it. Unparsable output
/// yields no devices rather than an error — this backs best-effort cleanup,
/// and failing to clean up must never mask the failure that triggered it.
#[must_use]
pub fn attached_devices_for(info_plist: &[u8], dmg: &Path) -> Vec<String> {
    let info: HdiutilInfo = match plist::from_bytes(info_plist) {
        Ok(info) => info,
        Err(e) => {
            warn!("could not parse `hdiutil info -plist` output: {e}");
            return Vec::new();
        }
    };

    info.images
        .iter()
        .filter(|image| Path::new(&image.image_path) == dmg)
        .filter_map(|image| image.system_entities.iter().find_map(|e| e.dev_entry.clone()))
        .collect()
}

/// The external effects the bundle swap needs. Implemented by [`SystemHost`]
/// in production and by fakes in tests.
pub trait BundleSwapHost {
    /// Mount `dmg` at `mount_point`.
    fn attach(&mut self, dmg: &Path, mount_point: &Path) -> Result<(), String>;
    /// Release the mount at `mount_point`.
    fn detach(&mut self, mount_point: &Path) -> Result<(), String>;
    /// Best-effort teardown of every device still attached for `dmg`,
    /// whatever its mount state.
    ///
    /// `hdiutil attach` can fail at the mount step *after* attaching the
    /// image's devices, leaving them with no mount point for
    /// [`BundleSwapHost::detach`] to target. This is the only cleanup that
    /// reaches those.
    fn release_image(&mut self, dmg: &Path);
    /// Copy the bundle tree at `src` to `dest`.
    fn copy_bundle(&mut self, src: &Path, dest: &Path) -> Result<(), String>;
    /// Rename `from` to `to`. Returns `false` when there was nothing to move.
    fn move_aside(&mut self, from: &Path, to: &Path) -> bool;
    /// Rename `from` back to `to`, undoing [`BundleSwapHost::move_aside`].
    fn restore(&mut self, from: &Path, to: &Path) -> Result<(), String>;
    /// Best-effort recursive delete.
    fn discard(&mut self, path: &Path);
    /// Whether `path` exists.
    fn exists(&self, path: &Path) -> bool;
}

/// Locations the swap operates on.
pub struct SwapPaths<'a> {
    /// The verified DMG to mount.
    pub dmg: &'a Path,
    /// Where to mount it — a caller-owned directory, never `/Volumes`.
    pub mount_point: &'a Path,
    /// Bundle directory name inside the mounted volume, e.g. `Scribe.app`.
    pub bundle_name: &'a str,
    /// The installed bundle to replace.
    pub app_bundle: &'a Path,
    /// Adjacent `.app.prev` rollback backup.
    pub prev_bundle: &'a Path,
}

/// Result of a successful swap.
#[derive(Debug)]
pub struct SwapOutcome {
    /// Whether a previous bundle was moved aside and is still on disk at
    /// `prev_bundle`. The caller compares binaries against it, then discards it.
    pub backup_existed: bool,
}

/// Mounts the DMG, replaces the installed bundle, and always releases the mount.
///
/// On copy failure any moved-aside bundle is restored before returning. The
/// mount is detached on every path out of this function once `attach` has
/// succeeded, so no error path can leak a mounted volume.
pub fn swap_bundle_from_dmg<H: BundleSwapHost>(
    host: &mut H,
    paths: &SwapPaths<'_>,
) -> Result<SwapOutcome, String> {
    // A failed attach is not necessarily a no-op: `hdiutil` can attach the
    // image's devices and then fail at the mount step, leaving them with no
    // mount point to detach. Tear the image down before giving up.
    if let Err(e) = host.attach(paths.dmg, paths.mount_point) {
        host.release_image(paths.dmg);
        return Err(e);
    }

    let swapped = swap_from_mounted_volume(host, paths);

    // Unconditional: the mount is released whether the swap succeeded or not.
    if let Err(e) = host.detach(paths.mount_point) {
        warn!(mount_point = %paths.mount_point.display(), "failed to detach update DMG: {e}");
        // Detaching by mount point is the cheap path; if it fails, fall back
        // to tearing the whole image down so the attachment cannot outlive us.
        host.release_image(paths.dmg);
    }

    swapped
}

/// The swap itself, with the volume already mounted.
fn swap_from_mounted_volume<H: BundleSwapHost>(
    host: &mut H,
    paths: &SwapPaths<'_>,
) -> Result<SwapOutcome, String> {
    // A backup left behind by an earlier failed update would collide with the
    // rename below, so clear it first.
    if host.exists(paths.prev_bundle) {
        host.discard(paths.prev_bundle);
    }

    let backup_existed = host.move_aside(paths.app_bundle, paths.prev_bundle);

    let src = paths.mount_point.join(paths.bundle_name);
    match host.copy_bundle(&src, paths.app_bundle) {
        Ok(()) => Ok(SwapOutcome { backup_existed }),
        Err(e) => {
            if backup_existed && let Err(re) = host.restore(paths.prev_bundle, paths.app_bundle) {
                warn!(bundle = %paths.app_bundle.display(), "rollback rename failed: {re}");
            }
            Err(e)
        }
    }
}

/// Production [`BundleSwapHost`]: `hdiutil`, `ditto`, and `std::fs`.
#[cfg(target_os = "macos")]
pub struct SystemHost;

#[cfg(target_os = "macos")]
impl BundleSwapHost for SystemHost {
    fn attach(&mut self, dmg: &Path, mount_point: &Path) -> Result<(), String> {
        run_tool("hdiutil", &attach_args(dmg, mount_point))
    }

    fn detach(&mut self, mount_point: &Path) -> Result<(), String> {
        run_tool("hdiutil", &detach_args(mount_point))
    }

    fn release_image(&mut self, dmg: &Path) {
        let info = match std::process::Command::new("hdiutil").args(["info", "-plist"]).output() {
            Ok(output) if output.status.success() => output.stdout,
            Ok(output) => {
                warn!(status = %output.status, "hdiutil info failed — cannot release image");
                return;
            }
            Err(e) => {
                warn!("failed to launch hdiutil info: {e}");
                return;
            }
        };

        for device in attached_devices_for(&info, dmg) {
            if let Err(e) = run_tool("hdiutil", &detach_args(Path::new(&device))) {
                warn!(%device, "failed to release attached update image: {e}");
            }
        }
    }

    fn copy_bundle(&mut self, src: &Path, dest: &Path) -> Result<(), String> {
        run_tool(
            "ditto",
            &[src.to_string_lossy().into_owned(), dest.to_string_lossy().into_owned()],
        )
    }

    fn move_aside(&mut self, from: &Path, to: &Path) -> bool {
        std::fs::rename(from, to).is_ok()
    }

    fn restore(&mut self, from: &Path, to: &Path) -> Result<(), String> {
        std::fs::rename(from, to).map_err(|e| format!("{e}"))
    }

    fn discard(&mut self, path: &Path) {
        drop(std::fs::remove_dir_all(path));
    }

    fn exists(&self, path: &Path) -> bool {
        path.exists()
    }
}

#[cfg(target_os = "macos")]
fn run_tool(tool: &str, args: &[String]) -> Result<(), String> {
    let output = std::process::Command::new(tool)
        .args(args)
        .output()
        .map_err(|e| format!("failed to launch {tool}: {e}"))?;
    if output.status.success() {
        return Ok(());
    }
    Err(format!(
        "{tool} exited with {}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// Records every effect so a test can assert on ordering and completeness.
    #[derive(Debug, PartialEq, Eq)]
    enum Call {
        Attach(PathBuf, PathBuf),
        Detach(PathBuf),
        Copy(PathBuf, PathBuf),
        MoveAside(PathBuf, PathBuf),
        Restore(PathBuf, PathBuf),
        Discard(PathBuf),
        ReleaseImage(PathBuf),
    }

    struct FakeHost {
        calls: Vec<Call>,
        existing: Vec<PathBuf>,
        attach_result: Result<(), String>,
        detach_result: Result<(), String>,
        copy_result: Result<(), String>,
        bundle_present: bool,
    }

    impl FakeHost {
        fn new() -> Self {
            Self {
                calls: Vec::new(),
                existing: Vec::new(),
                attach_result: Ok(()),
                detach_result: Ok(()),
                copy_result: Ok(()),
                bundle_present: true,
            }
        }

        fn detached(&self) -> bool {
            self.calls.iter().any(|c| matches!(c, Call::Detach(_)))
        }
    }

    impl BundleSwapHost for FakeHost {
        fn attach(&mut self, dmg: &Path, mount_point: &Path) -> Result<(), String> {
            self.calls.push(Call::Attach(dmg.to_path_buf(), mount_point.to_path_buf()));
            self.attach_result.clone()
        }

        fn detach(&mut self, mount_point: &Path) -> Result<(), String> {
            self.calls.push(Call::Detach(mount_point.to_path_buf()));
            self.detach_result.clone()
        }

        fn release_image(&mut self, dmg: &Path) {
            self.calls.push(Call::ReleaseImage(dmg.to_path_buf()));
        }

        fn copy_bundle(&mut self, src: &Path, dest: &Path) -> Result<(), String> {
            self.calls.push(Call::Copy(src.to_path_buf(), dest.to_path_buf()));
            self.copy_result.clone()
        }

        fn move_aside(&mut self, from: &Path, to: &Path) -> bool {
            self.calls.push(Call::MoveAside(from.to_path_buf(), to.to_path_buf()));
            self.bundle_present
        }

        fn restore(&mut self, from: &Path, to: &Path) -> Result<(), String> {
            self.calls.push(Call::Restore(from.to_path_buf(), to.to_path_buf()));
            Ok(())
        }

        fn discard(&mut self, path: &Path) {
            self.calls.push(Call::Discard(path.to_path_buf()));
        }

        fn exists(&self, path: &Path) -> bool {
            self.existing.iter().any(|p| p == path)
        }
    }

    fn paths() -> (PathBuf, PathBuf, PathBuf, PathBuf) {
        (
            PathBuf::from("/stage/Scribe_v1.dmg"),
            PathBuf::from("/stage/mnt"),
            PathBuf::from("/Applications/Scribe.app"),
            PathBuf::from("/Applications/Scribe.app.prev"),
        )
    }

    fn swap_paths<'a>(
        dmg: &'a Path,
        mount: &'a Path,
        app: &'a Path,
        prev: &'a Path,
    ) -> SwapPaths<'a> {
        SwapPaths {
            dmg,
            mount_point: mount,
            bundle_name: "Scribe.app",
            app_bundle: app,
            prev_bundle: prev,
        }
    }

    /// Shape of a real `hdiutil info -plist`, trimmed to the keys the teardown
    /// reads: two attached images, one of them ours, plus an image with no
    /// entities at all.
    const INFO_PLIST: &[u8] = br#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>images</key>
  <array>
    <dict>
      <key>image-path</key><string>/stage/Scribe_v1.dmg</string>
      <key>system-entities</key>
      <array>
        <dict><key>dev-entry</key><string>/dev/disk4</string></dict>
        <dict><key>dev-entry</key><string>/dev/disk4s1</string></dict>
        <dict>
          <key>dev-entry</key><string>/dev/disk5s1</string>
          <key>mount-point</key><string>/Volumes/Scribe</string>
        </dict>
      </array>
    </dict>
    <dict>
      <key>image-path</key><string>/elsewhere/other.dmg</string>
      <key>system-entities</key>
      <array><dict><key>dev-entry</key><string>/dev/disk9</string></dict></array>
    </dict>
    <dict>
      <key>image-path</key><string>/stage/no-entities.dmg</string>
    </dict>
  </array>
</dict>
</plist>"#;

    // @lat: [[test#Test Harness#macOS updater bundle swap#Teardown targets only the requested image]]
    #[test]
    fn teardown_targets_only_the_requested_image() {
        let devices = attached_devices_for(INFO_PLIST, Path::new("/stage/Scribe_v1.dmg"));

        assert_eq!(devices, vec!["/dev/disk4".to_owned()], "the whole-disk node, once");
    }

    // @lat: [[test#Test Harness#macOS updater bundle swap#Teardown skips an image with no devices]]
    #[test]
    fn teardown_skips_an_image_with_no_devices() {
        let devices = attached_devices_for(INFO_PLIST, Path::new("/stage/no-entities.dmg"));

        assert!(devices.is_empty(), "an image with no system-entities yields nothing to detach");
    }

    // @lat: [[test#Test Harness#macOS updater bundle swap#Unreadable hdiutil output yields no devices]]
    #[test]
    fn unreadable_hdiutil_output_yields_no_devices() {
        let devices =
            attached_devices_for(b"this is not a plist", Path::new("/stage/Scribe_v1.dmg"));

        assert!(devices.is_empty(), "best-effort cleanup degrades to a no-op, never a panic");
    }

    // @lat: [[test#Test Harness#macOS updater bundle swap#Attach pins the mount point]]
    #[test]
    fn attach_pins_the_mount_point() {
        let args = attach_args(Path::new("/stage/Scribe.dmg"), Path::new("/stage/mnt"));

        let flag = args.iter().position(|a| a == "-mountpoint").expect("-mountpoint is passed");
        assert_eq!(args[flag + 1], "/stage/mnt", "the mount point is pinned by the caller");
        assert_eq!(args.last().unwrap(), "/stage/Scribe.dmg", "the image is the final argument");
    }

    // @lat: [[test#Test Harness#macOS updater bundle swap#Detach targets the pinned mount point]]
    #[test]
    fn detach_targets_the_pinned_mount_point() {
        let args = detach_args(Path::new("/stage/mnt"));

        assert_eq!(args[0], "detach");
        assert_eq!(args.last().unwrap(), "/stage/mnt");
    }

    // @lat: [[test#Test Harness#macOS updater bundle swap#Successful swap releases the mount]]
    #[test]
    fn successful_swap_releases_the_mount() {
        let (dmg, mount, app, prev) = paths();
        let mut host = FakeHost::new();

        let outcome = swap_bundle_from_dmg(&mut host, &swap_paths(&dmg, &mount, &app, &prev))
            .expect("swap succeeds");

        assert!(outcome.backup_existed);
        assert!(host.detached(), "the mount is released after a successful swap");
        assert_eq!(
            host.calls.last(),
            Some(&Call::Detach(mount.clone())),
            "detach is the final effect"
        );
    }

    // @lat: [[test#Test Harness#macOS updater bundle swap#Copy failure releases the mount]]
    #[test]
    fn copy_failure_releases_the_mount() {
        let (dmg, mount, app, prev) = paths();
        let mut host = FakeHost::new();
        host.copy_result = Err("ditto boom".to_owned());

        let err = swap_bundle_from_dmg(&mut host, &swap_paths(&dmg, &mount, &app, &prev))
            .expect_err("swap fails");

        assert_eq!(err, "ditto boom");
        assert!(host.detached(), "a failed copy still releases the mount");
    }

    // @lat: [[test#Test Harness#macOS updater bundle swap#Copy failure restores the backup]]
    #[test]
    fn copy_failure_restores_the_backup() {
        let (dmg, mount, app, prev) = paths();
        let mut host = FakeHost::new();
        host.copy_result = Err("ditto boom".to_owned());

        drop(swap_bundle_from_dmg(&mut host, &swap_paths(&dmg, &mount, &app, &prev)));

        assert!(
            host.calls.contains(&Call::Restore(prev.clone(), app.clone())),
            "the moved-aside bundle is renamed back"
        );
    }

    // @lat: [[test#Test Harness#macOS updater bundle swap#Attach failure leaves the install untouched]]
    #[test]
    fn attach_failure_leaves_the_install_untouched() {
        let (dmg, mount, app, prev) = paths();
        let mut host = FakeHost::new();
        host.attach_result = Err("attach boom".to_owned());

        let err = swap_bundle_from_dmg(&mut host, &swap_paths(&dmg, &mount, &app, &prev))
            .expect_err("swap fails");

        assert_eq!(err, "attach boom");
        assert!(
            !host.calls.iter().any(|c| matches!(
                c,
                Call::MoveAside(..) | Call::Copy(..) | Call::Restore(..) | Call::Discard(..)
            )),
            "the installed bundle is never touched when the volume did not mount"
        );
    }

    // @lat: [[test#Test Harness#macOS updater bundle swap#Attach failure releases a partially attached image]]
    #[test]
    fn attach_failure_releases_a_partially_attached_image() {
        let (dmg, mount, app, prev) = paths();
        let mut host = FakeHost::new();
        host.attach_result = Err("attach boom".to_owned());

        drop(swap_bundle_from_dmg(&mut host, &swap_paths(&dmg, &mount, &app, &prev)));

        assert!(
            host.calls.contains(&Call::ReleaseImage(dmg.clone())),
            "a failed attach may still have attached devices, so the image is released"
        );
    }

    // @lat: [[test#Test Harness#macOS updater bundle swap#Failed detach falls back to releasing the image]]
    #[test]
    fn failed_detach_falls_back_to_releasing_the_image() {
        let (dmg, mount, app, prev) = paths();
        let mut host = FakeHost::new();
        host.detach_result = Err("resource busy".to_owned());

        let outcome = swap_bundle_from_dmg(&mut host, &swap_paths(&dmg, &mount, &app, &prev))
            .expect("the swap itself still succeeded");

        assert!(outcome.backup_existed);
        assert!(
            host.calls.contains(&Call::ReleaseImage(dmg.clone())),
            "when detaching the mount point fails, the image teardown is the fallback"
        );
    }

    // @lat: [[test#Test Harness#macOS updater bundle swap#Stale backup is discarded before the swap]]
    #[test]
    fn stale_backup_is_discarded_before_the_swap() {
        let (dmg, mount, app, prev) = paths();
        let mut host = FakeHost::new();
        host.existing.push(prev.clone());

        drop(swap_bundle_from_dmg(&mut host, &swap_paths(&dmg, &mount, &app, &prev)));

        let discarded = host
            .calls
            .iter()
            .position(|c| matches!(c, Call::Discard(p) if *p == prev))
            .expect("the stale backup is discarded");
        let moved = host
            .calls
            .iter()
            .position(|c| matches!(c, Call::MoveAside(..)))
            .expect("the bundle is moved aside");
        assert!(discarded < moved, "the stale backup is cleared before the rename");
    }

    // @lat: [[test#Test Harness#macOS updater bundle swap#Fresh install reports no backup]]
    #[test]
    fn fresh_install_reports_no_backup() {
        let (dmg, mount, app, prev) = paths();
        let mut host = FakeHost::new();
        host.bundle_present = false;

        let outcome = swap_bundle_from_dmg(&mut host, &swap_paths(&dmg, &mount, &app, &prev))
            .expect("swap succeeds");

        assert!(!outcome.backup_existed, "no bundle was moved aside");
    }

    // @lat: [[test#Test Harness#macOS updater bundle swap#Fresh install failure skips rollback]]
    #[test]
    fn fresh_install_failure_skips_rollback() {
        let (dmg, mount, app, prev) = paths();
        let mut host = FakeHost::new();
        host.bundle_present = false;
        host.copy_result = Err("ditto boom".to_owned());

        drop(swap_bundle_from_dmg(&mut host, &swap_paths(&dmg, &mount, &app, &prev)));

        assert!(
            !host.calls.iter().any(|c| matches!(c, Call::Restore(..))),
            "there is no backup to restore"
        );
    }
}
