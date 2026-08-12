//! launchd contract for the two-slot macOS server service.
//!
//! A single launchd job cannot overlap its old and new process, so a warm
//! handoff alternates between two jobs. The inactive job starts with
//! `--upgrade`, receives the live server state, and remains the supervised job
//! after the old slot exits successfully.

use std::path::{Path, PathBuf};

use crate::app::AppIdentity;

/// Command-line prefix that identifies a launchd-managed server instance.
pub const SLOT_ARG_PREFIX: &str = "--launchd-slot=";
/// One-shot client mode that replaces the named active launchd slot.
pub const REGISTER_REPLACEMENT_ARG_PREFIX: &str = "--register-launchd-replacement-for=";
/// One-shot client mode that removes the job opposite the named active slot.
pub const UNREGISTER_INACTIVE_ARG_PREFIX: &str = "--unregister-launchd-inactive-for=";
/// One-shot new-bundle client mode that relaunches old clients after handoff.
pub const RELAUNCH_CLIENTS_ARG_PREFIX: &str = "--relaunch-clients-after-server=";

/// Stable identity of one pre-install client process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TrackedClient {
    pub pid: u32,
    pub start_time_secs: u64,
}

/// One of the two launchd jobs that can own the server process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaunchdSlot {
    Primary,
    Alternate,
}

/// Filesystem identity of the executable that reached serving readiness.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BinaryIdentity {
    pub device: u64,
    pub inode: u64,
}

/// Device/inode identity for a shipped executable.
#[must_use]
pub fn binary_identity(path: &Path) -> Option<BinaryIdentity> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;

        let metadata = std::fs::metadata(path).ok()?;
        Some(BinaryIdentity { device: metadata.dev(), inode: metadata.ino() })
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        None
    }
}

impl LaunchdSlot {
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Primary => "primary",
            Self::Alternate => "alternate",
        }
    }

    #[must_use]
    pub const fn other(self) -> Self {
        match self {
            Self::Primary => Self::Alternate,
            Self::Alternate => Self::Primary,
        }
    }

    #[must_use]
    pub fn argument(self) -> String {
        format!("{SLOT_ARG_PREFIX}{}", self.name())
    }

    #[must_use]
    pub fn from_argument(argument: &str) -> Option<Self> {
        match argument.strip_prefix(SLOT_ARG_PREFIX)? {
            "primary" => Some(Self::Primary),
            "alternate" => Some(Self::Alternate),
            _ => None,
        }
    }

    #[must_use]
    pub fn from_args(args: impl IntoIterator<Item = impl AsRef<str>>) -> Option<Self> {
        args.into_iter().find_map(|argument| Self::from_argument(argument.as_ref()))
    }

    #[must_use]
    pub fn registration_argument(self) -> String {
        format!("{REGISTER_REPLACEMENT_ARG_PREFIX}{}", self.name())
    }

    #[must_use]
    pub fn registration_from_args(args: impl IntoIterator<Item = impl AsRef<str>>) -> Option<Self> {
        args.into_iter().find_map(|argument| {
            let name = argument.as_ref().strip_prefix(REGISTER_REPLACEMENT_ARG_PREFIX)?;
            Self::from_argument(&format!("{SLOT_ARG_PREFIX}{name}"))
        })
    }

    #[must_use]
    pub fn inactive_unregistration_argument(self) -> String {
        format!("{UNREGISTER_INACTIVE_ARG_PREFIX}{}", self.name())
    }

    #[must_use]
    pub fn inactive_unregistration_from_args(
        args: impl IntoIterator<Item = impl AsRef<str>>,
    ) -> Option<Self> {
        args.into_iter().find_map(|argument| {
            let name = argument.as_ref().strip_prefix(UNREGISTER_INACTIVE_ARG_PREFIX)?;
            Self::from_argument(&format!("{SLOT_ARG_PREFIX}{name}"))
        })
    }
}

/// launchd label for one slot and install flavor.
#[must_use]
pub fn label(identity: AppIdentity, slot: LaunchdSlot) -> String {
    match slot {
        LaunchdSlot::Primary => identity.launchd_label().to_owned(),
        LaunchdSlot::Alternate => format!("{}.alternate", identity.launchd_label()),
    }
}

/// `LaunchAgent` plist filename for one slot and install flavor.
#[must_use]
pub fn plist_name(identity: AppIdentity, slot: LaunchdSlot) -> String {
    format!("{}.plist", label(identity, slot))
}

/// Historical launchd targets that may contain a legacy per-user job.
#[must_use]
pub fn service_targets(identity: AppIdentity, slot: LaunchdSlot, uid: u32) -> [String; 2] {
    let label = label(identity, slot);
    [format!("gui/{uid}/{label}"), format!("user/{uid}/{label}")]
}

/// Bundled `LaunchAgent` plist for a supervised slot.
#[must_use]
pub fn plist_contents(identity: AppIdentity, slot: LaunchdSlot) -> String {
    let label = escape_plist_value(&label(identity, slot));
    let server_binary = identity.server_binary_name();
    let slot_arg = slot.argument();
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key>
	<string>{label}</string>

	<key>BundleProgram</key>
	<string>Contents/MacOS/{server_binary}</string>

	<key>ProgramArguments</key>
	<array>
		<string>{server_binary}</string>
		<string>--upgrade</string>
		<string>{slot_arg}</string>
	</array>

	<key>EnvironmentVariables</key>
	<dict>
		<key>PATH</key>
		<string>/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin</string>
	</dict>

	<key>KeepAlive</key>
	<dict>
		<key>SuccessfulExit</key>
		<false/>
	</dict>

	<key>ProcessType</key>
	<string>Background</string>

	<key>ThrottleInterval</key>
	<integer>1</integer>

	<key>StandardOutPath</key>
	<string>/dev/null</string>

	<key>StandardErrorPath</key>
	<string>/dev/null</string>
</dict>
</plist>
"#
    )
}

fn escape_plist_value(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// Runtime marker used to prove which slot owns a connected server PID.
#[must_use]
pub fn active_slot_path(identity: AppIdentity) -> Option<PathBuf> {
    identity.state_dir().map(|dir| dir.join("launchd-active-slot"))
}

/// Read the active slot only when the marker belongs to `expected_pid`.
#[must_use]
pub fn active_slot_for_pid(identity: AppIdentity, expected_pid: u32) -> Option<LaunchdSlot> {
    active_slot_record_for_pid(identity, expected_pid).map(|(slot, _)| slot)
}

/// Active slot and executable identity only when the marker owns `expected_pid`.
#[must_use]
pub fn active_slot_record_for_pid(
    identity: AppIdentity,
    expected_pid: u32,
) -> Option<(LaunchdSlot, Option<BinaryIdentity>)> {
    let record = active_slot_record(identity)?;
    (record.pid == expected_pid).then_some((record.slot, record.binary))
}

/// PID that most recently reached serving readiness in `expected_slot`.
#[must_use]
pub fn active_slot_owner(identity: AppIdentity, expected_slot: LaunchdSlot) -> Option<u32> {
    let record = active_slot_record(identity)?;
    (record.slot == expected_slot).then_some(record.pid)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ActiveSlotRecord {
    pid: u32,
    slot: LaunchdSlot,
    binary: Option<BinaryIdentity>,
}

fn parse_active_slot_record(contents: &str) -> Option<ActiveSlotRecord> {
    let mut fields = contents.split_whitespace();
    let pid = fields.next()?.parse::<u32>().ok()?;
    let slot = LaunchdSlot::from_argument(&format!("{SLOT_ARG_PREFIX}{}", fields.next()?))?;
    let binary = match (fields.next(), fields.next()) {
        (None, None) => None,
        (Some(device), Some(inode)) => Some(BinaryIdentity {
            device: device.parse::<u64>().ok()?,
            inode: inode.parse::<u64>().ok()?,
        }),
        _ => return None,
    };
    fields.next().is_none().then_some(ActiveSlotRecord { pid, slot, binary })
}

fn active_slot_record(identity: AppIdentity) -> Option<ActiveSlotRecord> {
    let contents = std::fs::read_to_string(active_slot_path(identity)?).ok()?;
    parse_active_slot_record(&contents)
}

/// Atomically record the launchd slot that reached serving readiness.
pub fn record_active_slot(identity: AppIdentity, slot: LaunchdSlot) -> Result<(), String> {
    let path =
        active_slot_path(identity).ok_or_else(|| String::from("state directory unavailable"))?;
    let parent = path.parent().ok_or_else(|| String::from("active-slot path has no parent"))?;
    std::fs::create_dir_all(parent)
        .map_err(|error| format!("failed to create active-slot directory: {error}"))?;
    let temporary = path.with_extension(format!("tmp-{}", std::process::id()));
    let binary = std::env::current_exe().ok().as_deref().and_then(binary_identity);
    let contents = binary.map_or_else(
        || format!("{} {}\n", std::process::id(), slot.name()),
        |binary| {
            format!("{} {} {} {}\n", std::process::id(), slot.name(), binary.device, binary.inode)
        },
    );
    std::fs::write(&temporary, contents)
        .map_err(|error| format!("failed to write active-slot marker: {error}"))?;
    std::fs::rename(&temporary, &path)
        .map_err(|error| format!("failed to publish active-slot marker: {error}"))
}

#[cfg(target_os = "macos")]
fn legacy_plist_path(identity: AppIdentity, slot: LaunchdSlot) -> Result<PathBuf, String> {
    let home = std::env::var("HOME").map_err(|error| format!("HOME not set: {error}"))?;
    Ok(PathBuf::from(home).join("Library/LaunchAgents").join(plist_name(identity, slot)))
}

#[cfg(target_os = "macos")]
pub type LaunchdSlotGuard = nix::fcntl::Flock<std::fs::File>;

#[cfg(target_os = "macos")]
fn acquire_lock(
    identity: AppIdentity,
    name: &str,
    argument: nix::fcntl::FlockArg,
) -> Result<LaunchdSlotGuard, String> {
    let directory =
        identity.state_dir().ok_or_else(|| String::from("state directory unavailable"))?;
    std::fs::create_dir_all(&directory)
        .map_err(|error| format!("failed to create launchd lock directory: {error}"))?;
    let path = directory.join(name);
    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&path)
        .map_err(|error| format!("failed to open launchd lock {}: {error}", path.display()))?;
    nix::fcntl::Flock::lock(file, argument)
        .map_err(|(_, error)| format!("failed to acquire launchd lock {}: {error}", path.display()))
}

/// Hold the process lock for a launchd-managed server's entire lifetime.
#[cfg(target_os = "macos")]
pub fn acquire_slot_guard(
    identity: AppIdentity,
    slot: LaunchdSlot,
) -> Result<LaunchdSlotGuard, String> {
    acquire_lock(
        identity,
        &format!("launchd-{}.lock", slot.name()),
        nix::fcntl::FlockArg::LockExclusiveNonblock,
    )
}

#[cfg(target_os = "macos")]
fn acquire_registration_guard(identity: AppIdentity) -> Result<LaunchdSlotGuard, String> {
    acquire_lock(identity, "launchd-registration.lock", nix::fcntl::FlockArg::LockExclusive)
}

#[cfg(target_os = "macos")]
fn slot_is_running(identity: AppIdentity, slot: LaunchdSlot) -> Result<bool, String> {
    let directory =
        identity.state_dir().ok_or_else(|| String::from("state directory unavailable"))?;
    std::fs::create_dir_all(&directory)
        .map_err(|error| format!("failed to create launchd lock directory: {error}"))?;
    let path = directory.join(format!("launchd-{}.lock", slot.name()));
    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&path)
        .map_err(|error| format!("failed to open launchd lock {}: {error}", path.display()))?;
    match nix::fcntl::Flock::lock(file, nix::fcntl::FlockArg::LockExclusiveNonblock) {
        Ok(guard) => {
            drop(guard);
            Ok(false)
        }
        Err((_, error)) if error == nix::errno::Errno::EWOULDBLOCK => Ok(true),
        Err((_, error)) => {
            Err(format!("failed to inspect launchd lock {}: {error}", path.display()))
        }
    }
}

#[cfg(target_os = "macos")]
fn slot_is_ready(identity: AppIdentity, slot: LaunchdSlot) -> bool {
    let Some(pid) = active_slot_owner(identity, slot) else {
        return false;
    };
    let output = std::process::Command::new("/bin/ps")
        .args(["-o", "command=", "-p", &pid.to_string()])
        .output()
        .ok()
        .filter(|output| output.status.success());
    output
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .and_then(|command| LaunchdSlot::from_args(command.split_whitespace()))
        == Some(slot)
}

#[cfg(target_os = "macos")]
fn wait_for_slot_ready(identity: AppIdentity, slot: LaunchdSlot) -> Result<(), String> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while slot_is_running(identity, slot)? {
        if slot_is_ready(identity, slot) {
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            return Err(format!(
                "running launchd {} slot did not reach serving readiness within 30 seconds",
                slot.name()
            ));
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    Err(format!("launchd {} slot exited before reaching serving readiness", slot.name()))
}

#[cfg(target_os = "macos")]
fn wait_for_slot_start(identity: AppIdentity, slot: LaunchdSlot) -> Result<(), String> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        if slot_is_running(identity, slot)? {
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            return Err(format!(
                "launchd {} slot did not start within 10 seconds of registration",
                slot.name()
            ));
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
}

#[cfg(target_os = "macos")]
fn wait_for_slot_stop(identity: AppIdentity, slot: LaunchdSlot) -> Result<(), String> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while slot_is_running(identity, slot)? {
        if std::time::Instant::now() >= deadline {
            return Err(format!("launchd {} slot did not stop within 10 seconds", slot.name()));
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    Ok(())
}

#[cfg(target_os = "macos")]
#[allow(unsafe_code, reason = "SMAppService is exposed through Objective-C bindings")]
mod service_management {
    use super::{AppIdentity, LaunchdSlot, plist_name};
    use objc2_service_management::{SMAppService, SMAppServiceStatus};

    pub(super) type AppService = objc2::rc::Retained<SMAppService>;

    pub(super) fn app_service(identity: AppIdentity, slot: LaunchdSlot) -> AppService {
        let name = objc2_foundation::NSString::from_str(&plist_name(identity, slot));
        // SAFETY: The plist name is an owned NSString that remains valid for
        // the duration of the Objective-C call.
        unsafe { SMAppService::agentServiceWithPlistName(&name) }
    }

    pub(super) fn status(service: &SMAppService) -> SMAppServiceStatus {
        // SAFETY: `service` is retained for the duration of the message send.
        unsafe { service.status() }
    }

    pub(super) fn register(service: &SMAppService, service_label: &str) -> Result<(), String> {
        // SAFETY: `service` is retained for the duration of the message send;
        // objc2 owns the returned NSError on the failure path.
        unsafe { service.registerAndReturnError() }
            .map_err(|error| format!("failed to register {service_label}: {error:?}"))
    }

    pub(super) fn unregister_and_wait(
        service: &SMAppService,
        service_label: &str,
    ) -> Result<(), String> {
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        let completion = block2::RcBlock::new(move |error: *mut objc2_foundation::NSError| {
            let result = std::ptr::NonNull::new(error).map(|error| {
                // SAFETY: SMAppService guarantees the NSError remains valid
                // for the duration of its completion callback.
                format!("{:?}", unsafe { error.as_ref() })
            });
            drop(sender.send(result));
        });

        // SAFETY: The copied block remains valid until Service Management
        // invokes it on libdispatch's default queue.
        unsafe { service.unregisterWithCompletionHandler(&completion) };
        match receiver.recv_timeout(std::time::Duration::from_secs(10)) {
            Ok(None) => Ok(()),
            Ok(Some(error)) => Err(format!("failed to unregister {service_label}: {error}")),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                Err(format!("timed out unregistering {service_label}"))
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => Err(format!(
                "unregister callback for {service_label} disconnected without a result"
            )),
        }
    }
}

#[cfg(target_os = "macos")]
fn run_launchctl(arguments: &[&str]) -> Result<(), String> {
    let status = std::process::Command::new("/bin/launchctl")
        .args(arguments)
        .status()
        .map_err(|error| format!("failed to run launchctl {}: {error}", arguments.join(" ")))?;
    status
        .success()
        .then_some(())
        .ok_or_else(|| format!("launchctl {} exited with {status}", arguments.join(" ")))
}

#[cfg(target_os = "macos")]
fn remove_legacy_registration(identity: AppIdentity, slot: LaunchdSlot) -> Result<(), String> {
    let targets = service_targets(identity, slot, crate::socket::current_uid());
    for target in &targets {
        drop(run_launchctl(&["bootout", target]));
    }
    let loaded = targets
        .iter()
        .filter(|target| run_launchctl(&["print", target]).is_ok())
        .cloned()
        .collect::<Vec<_>>();
    if !loaded.is_empty() {
        return Err(format!("legacy launchd jobs remain loaded: {}", loaded.join(", ")));
    }

    let legacy_plist = legacy_plist_path(identity, slot)?;
    if let Err(error) = std::fs::remove_file(&legacy_plist)
        && error.kind() != std::io::ErrorKind::NotFound
    {
        return Err(format!("failed to remove legacy plist {}: {error}", legacy_plist.display()));
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn refuse_active_slot(
    slot: LaunchdSlot,
    declared_active: Option<LaunchdSlot>,
) -> Result<(), String> {
    if declared_active == Some(slot) {
        return Err(format!("refusing to replace declared active launchd {} slot", slot.name()));
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn register_inactive_slot(
    identity: AppIdentity,
    slot: LaunchdSlot,
    declared_active: Option<LaunchdSlot>,
) -> Result<(), String> {
    use objc2_service_management::SMAppServiceStatus;

    let _registration_guard = acquire_registration_guard(identity)?;
    if slot_is_running(identity, slot)? {
        return declared_active.map_or_else(
            || Err(format!("refusing to activate running launchd {} slot", slot.name())),
            |_| wait_for_slot_ready(identity, slot),
        );
    }
    if declared_active.is_none() && slot_is_running(identity, slot.other())? {
        return Err(format!(
            "refusing initial launchd activation while {} slot is running",
            slot.other().name()
        ));
    }
    refuse_active_slot(slot, declared_active)?;

    let service = service_management::app_service(identity, slot);
    match service_management::status(&service) {
        SMAppServiceStatus::Enabled => {
            service_management::unregister_and_wait(&service, &label(identity, slot))?;
        }
        SMAppServiceStatus::RequiresApproval => {
            return Err(format!(
                "{} requires approval in System Settings > General > Login Items",
                label(identity, slot)
            ));
        }
        SMAppServiceStatus::NotRegistered | SMAppServiceStatus::NotFound => {}
        status => return Err(format!("unexpected SMAppService status {}", status.0)),
    }

    // Remove the pre-macOS-13 registration only for the inactive slot the
    // caller selected. Its loaded job shares this label and would otherwise
    // prevent Service Management from registering the bundled replacement.
    remove_legacy_registration(identity, slot)?;

    let app_service = service_management::app_service(identity, slot);
    service_management::register(&app_service, &label(identity, slot))?;
    wait_for_slot_start(identity, slot)
}

/// Register a slot when the caller proved that no server process is alive.
#[cfg(target_os = "macos")]
pub fn activate_initial_slot(identity: AppIdentity, slot: LaunchdSlot) -> Result<(), String> {
    register_inactive_slot(identity, slot, None)
}

/// Re-register and start the job opposite a proven active slot.
#[cfg(target_os = "macos")]
pub fn activate_replacement(identity: AppIdentity, active: LaunchdSlot) -> Result<(), String> {
    register_inactive_slot(identity, active.other(), Some(active))
}

/// Unregister the predecessor after the active slot reaches serving readiness.
#[cfg(target_os = "macos")]
pub fn unregister_inactive_slot(identity: AppIdentity, active: LaunchdSlot) -> Result<(), String> {
    use objc2_service_management::SMAppServiceStatus;

    let _registration_guard = acquire_registration_guard(identity)?;
    let inactive = active.other();
    wait_for_slot_stop(identity, inactive)?;
    let service = service_management::app_service(identity, inactive);
    match service_management::status(&service) {
        SMAppServiceStatus::Enabled => {
            service_management::unregister_and_wait(&service, &label(identity, inactive))?;
        }
        SMAppServiceStatus::NotRegistered | SMAppServiceStatus::NotFound => {}
        SMAppServiceStatus::RequiresApproval => {
            return Err(format!(
                "{} requires approval in System Settings > General > Login Items",
                label(identity, inactive)
            ));
        }
        status => return Err(format!("unexpected SMAppService status {}", status.0)),
    }
    remove_legacy_registration(identity, inactive)
}

/// Unregister and stop both jobs before an explicitly approved cold restart.
#[cfg(target_os = "macos")]
pub fn unregister_all_slots(identity: AppIdentity) -> Result<(), String> {
    use objc2_service_management::SMAppServiceStatus;

    let _registration_guard = acquire_registration_guard(identity)?;
    for slot in [LaunchdSlot::Primary, LaunchdSlot::Alternate] {
        let app_service = service_management::app_service(identity, slot);
        match service_management::status(&app_service) {
            SMAppServiceStatus::Enabled
            | SMAppServiceStatus::NotRegistered
            | SMAppServiceStatus::NotFound => {}
            SMAppServiceStatus::RequiresApproval => {
                return Err(format!(
                    "{} requires approval in System Settings > General > Login Items",
                    label(identity, slot)
                ));
            }
            status => return Err(format!("unexpected SMAppService status {}", status.0)),
        }
    }

    for slot in [LaunchdSlot::Primary, LaunchdSlot::Alternate] {
        let app_service = service_management::app_service(identity, slot);
        match service_management::status(&app_service) {
            SMAppServiceStatus::Enabled => {
                service_management::unregister_and_wait(&app_service, &label(identity, slot))?;
            }
            SMAppServiceStatus::NotRegistered | SMAppServiceStatus::NotFound => {}
            SMAppServiceStatus::RequiresApproval => {
                return Err(format!(
                    "{} requires approval in System Settings > General > Login Items",
                    label(identity, slot)
                ));
            }
            status => return Err(format!("unexpected SMAppService status {}", status.0)),
        }
        remove_legacy_registration(identity, slot)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // @lat: [[test#Test Harness#macOS launchd lifecycle#Warm replacements alternate services]]
    #[test]
    fn alternate_slot_uses_a_distinct_service() {
        assert_eq!(label(AppIdentity::stable(), LaunchdSlot::Primary), "com.scribe.server");
        assert_eq!(
            label(AppIdentity::stable(), LaunchdSlot::Alternate),
            "com.scribe.server.alternate"
        );
        assert_eq!(LaunchdSlot::Primary.other(), LaunchdSlot::Alternate);
        assert_eq!(
            service_targets(AppIdentity::stable(), LaunchdSlot::Primary, 501),
            [String::from("gui/501/com.scribe.server"), String::from("user/501/com.scribe.server"),]
        );
    }

    // @lat: [[test#Test Harness#macOS launchd lifecycle#Bundled agents request managed handoff]]
    #[test]
    fn managed_plist_requests_upgrade_without_force_restart() {
        let plist = plist_contents(AppIdentity::stable(), LaunchdSlot::Primary);
        assert!(plist.contains("<key>BundleProgram</key>"));
        assert!(plist.contains("<string>Contents/MacOS/scribe-server</string>"));
        assert!(plist.contains("<string>--upgrade</string>"));
        assert!(plist.contains("<string>--launchd-slot=primary</string>"));
        assert_eq!(
            LaunchdSlot::Alternate.registration_argument(),
            "--register-launchd-replacement-for=alternate"
        );
        assert_eq!(
            LaunchdSlot::registration_from_args([
                "scribe-client",
                "--register-launchd-replacement-for=alternate",
            ]),
            Some(LaunchdSlot::Alternate)
        );
        assert_eq!(
            LaunchdSlot::Primary.inactive_unregistration_argument(),
            "--unregister-launchd-inactive-for=primary"
        );
    }

    // @lat: [[test#Test Harness#macOS launchd lifecycle#Readiness marker preserves binary identity]]
    #[test]
    fn readiness_marker_preserves_binary_identity() {
        assert_eq!(
            parse_active_slot_record("42 alternate 7 99\n"),
            Some(ActiveSlotRecord {
                pid: 42,
                slot: LaunchdSlot::Alternate,
                binary: Some(BinaryIdentity { device: 7, inode: 99 }),
            })
        );
        assert_eq!(
            parse_active_slot_record("42 primary\n"),
            Some(ActiveSlotRecord { pid: 42, slot: LaunchdSlot::Primary, binary: None })
        );
        assert_eq!(parse_active_slot_record("42 primary 7"), None);
    }
}
