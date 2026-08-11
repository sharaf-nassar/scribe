//! Executes the real macOS bundle swap against a real disk image.
//!
//! Every defect this feature has shipped was a runtime one — a flag that
//! contradicted a parse, a `/Volumes` name collision, a leaked attachment —
//! and none of them were reachable by a lint or a unit test over pure
//! functions. This drives the production [`SystemHost`] through `hdiutil` and
//! `ditto` for real, so those defects fail a test instead of a user.
//!
//! Hermetic: it builds its own image, installs into a temp directory, and
//! never touches `/Applications` or a live server. It deliberately does NOT
//! call `install_update`, whose restart tail would `pkill` the developer's
//! client.
#![cfg(target_os = "macos")]

use std::path::{Path, PathBuf};
use std::process::Command;

use scribe_server::updater::macos_install::{
    SwapPaths, SystemHost, attached_devices_for, swap_bundle_from_dmg,
};

/// Detaches everything attached for an image when it goes out of scope, so a
/// panicking test cannot leak a mount — the exact bug this suite exists for.
struct AttachedImage(PathBuf);

impl Drop for AttachedImage {
    fn drop(&mut self) {
        let info = Command::new("/usr/bin/hdiutil")
            .args(["info", "-plist"])
            .output()
            .map(|o| o.stdout)
            .unwrap_or_default();
        for dev in attached_devices_for(&info, &self.0) {
            drop(Command::new("/usr/bin/hdiutil").args(["detach", "-quiet", &dev]).status());
        }
    }
}

fn run(program: &str, args: &[&str]) -> Result<(), String> {
    let out = Command::new(program)
        .args(args)
        .output()
        .map_err(|e| format!("failed to launch {program}: {e}"))?;
    if out.status.success() {
        return Ok(());
    }
    Err(format!("{program} {args:?} failed: {}", String::from_utf8_lossy(&out.stderr)))
}

/// Builds a DMG whose volume is named `Scribe`, containing `Scribe.app` whose
/// server binary holds `marker`.
fn build_dmg(root: &Path, name: &str, marker: &[u8]) -> Result<PathBuf, String> {
    let src = root.join(format!("{name}-src"));
    let macos = src.join("Scribe.app/Contents/MacOS");
    std::fs::create_dir_all(&macos).map_err(|e| format!("mkdir src: {e}"))?;
    std::fs::write(macos.join("scribe-server"), marker)
        .map_err(|e| format!("write marker: {e}"))?;

    let dmg = root.join(format!("{name}.dmg"));
    run(
        "/usr/bin/hdiutil",
        &[
            "create",
            "-quiet",
            "-srcfolder",
            &src.to_string_lossy(),
            "-volname",
            "Scribe",
            &dmg.to_string_lossy(),
        ],
    )?;
    Ok(dmg)
}

fn attached_for(dmg: &Path) -> Vec<String> {
    let info = Command::new("/usr/bin/hdiutil")
        .args(["info", "-plist"])
        .output()
        .map(|o| o.stdout)
        .unwrap_or_default();
    attached_devices_for(&info, dmg)
}

// @lat: [[test#Test Harness#macOS bundle swap contract#Swap installs the bundle with /Volumes occupied]]
#[test]
fn swap_installs_the_bundle_with_volumes_occupied() {
    let root = std::env::temp_dir().join(format!("scribe-dmg-contract-{}", std::process::id()));
    drop(std::fs::remove_dir_all(&root));
    std::fs::create_dir_all(&root).expect("mkdir root");

    // A decoy with the SAME volume name, mounted the old way, so `/Volumes/Scribe`
    // is taken. This is the state that made the shipped parser read back "1".
    let decoy = build_dmg(&root, "decoy", b"decoy").expect("build decoy image");
    let _decoy_guard = AttachedImage(decoy.clone());
    run("/usr/bin/hdiutil", &["attach", "-nobrowse", "-quiet", &decoy.to_string_lossy()])
        .expect("occupy /Volumes/Scribe with the decoy");

    let dmg = build_dmg(&root, "update", b"new-server").expect("build update image");
    let _guard = AttachedImage(dmg.clone());

    // Stand-in for /Applications/Scribe.app, holding an "old" binary.
    let app = root.join("dest/Scribe.app");
    let prev = root.join("dest/Scribe.app.prev");
    std::fs::create_dir_all(app.join("Contents/MacOS")).expect("mkdir dest");
    std::fs::write(app.join("Contents/MacOS/scribe-server"), b"old-server").expect("write old");

    let mount = root.join("mnt");
    std::fs::create_dir_all(&mount).expect("mkdir mount");

    let outcome = swap_bundle_from_dmg(
        &mut SystemHost,
        &SwapPaths {
            dmg: &dmg,
            mount_point: &mount,
            bundle_name: "Scribe.app",
            app_bundle: &app,
            prev_bundle: &prev,
        },
    )
    .expect("the swap must succeed even when /Volumes/Scribe is taken");

    assert!(outcome.backup_existed, "the existing bundle was moved aside for rollback");
    assert_eq!(
        std::fs::read(app.join("Contents/MacOS/scribe-server")).expect("installed binary"),
        b"new-server",
        "the DMG's bundle replaced the old one"
    );
    assert!(attached_for(&dmg).is_empty(), "a successful swap must leave nothing attached");
    assert!(
        std::fs::read_dir(&mount).is_ok_and(|mut d| d.next().is_none()),
        "the pinned mount point is empty again"
    );

    drop(std::fs::remove_dir_all(&root));
}

// @lat: [[test#Test Harness#macOS bundle swap contract#Copy failure restores the bundle and detaches]]
#[test]
fn copy_failure_restores_the_bundle_and_detaches() {
    let root = std::env::temp_dir().join(format!("scribe-dmg-rollback-{}", std::process::id()));
    drop(std::fs::remove_dir_all(&root));
    std::fs::create_dir_all(&root).expect("mkdir root");

    let dmg = build_dmg(&root, "update", b"new-server").expect("build update image");
    let _guard = AttachedImage(dmg.clone());

    let app = root.join("dest/Scribe.app");
    let prev = root.join("dest/Scribe.app.prev");
    std::fs::create_dir_all(app.join("Contents/MacOS")).expect("mkdir dest");
    std::fs::write(app.join("Contents/MacOS/scribe-server"), b"old-server").expect("write old");

    let mount = root.join("mnt");
    std::fs::create_dir_all(&mount).expect("mkdir mount");

    // A bundle name that is not on the image makes `ditto` fail, exercising
    // the rollback path against real tools rather than a fake.
    let result = swap_bundle_from_dmg(
        &mut SystemHost,
        &SwapPaths {
            dmg: &dmg,
            mount_point: &mount,
            bundle_name: "NotThere.app",
            app_bundle: &app,
            prev_bundle: &prev,
        },
    );

    assert!(result.is_err(), "a missing source bundle must fail the swap");
    assert_eq!(
        std::fs::read(app.join("Contents/MacOS/scribe-server")).expect("bundle restored"),
        b"old-server",
        "the machine keeps a working install when the copy fails"
    );
    assert!(attached_for(&dmg).is_empty(), "a failed swap must still release the image");

    drop(std::fs::remove_dir_all(&root));
}
