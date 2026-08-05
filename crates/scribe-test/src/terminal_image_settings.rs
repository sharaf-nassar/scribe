//! Production-path evidence for the terminal-image master switch, its
//! diagnostics, and the settings surface that writes it.
//!
//! Every case drives shipped code: the shared `ScribeConfig` schema, the
//! client's settings model/read/apply path, the server's capability latch and
//! reply planner, the server's session image seam, the client's GPUI error
//! taxonomy, and the client's live scene. Nothing here re-implements a
//! decision the product makes elsewhere.

use std::collections::BTreeMap;
use std::path::Path;

use scribe_client::gpui_image_lifecycle::GpuiImageError;
use scribe_client::settings::model::{ControlKind, SettingsPage, page_controls};
use scribe_client::settings::values::current_value;
use scribe_client::terminal_image_scene::{CommittedImageScene, LiveImageScene};
use scribe_common::config::{ScribeConfig, load_config};
use scribe_common::terminal_images::{
    ImageBoundError, ImageLimitName, TerminalImageAction, TerminalImageCapabilities,
    TerminalImageGeneration, TerminalImageLiveMessage, TerminalImageProtocol,
    TerminalImageRejection, TerminalImageRejectionReason, TerminalImageUpdate,
    TerminalOutputSequence,
};
use scribe_server::terminal_image_sharing::{
    KillSwitchTransition, SessionImageSharing, augment_device_attributes,
    effective_connection_subset, plan_pty_replies,
};
use serde::Serialize;

use crate::framing_probe::read_hex;
use crate::terminal_image_replies_sharing::{Probe, RGB_CLASSIC_FIXTURE, write_probe_evidence};

/// The viewer capability a shipped image-capable client advertises.
fn capable_viewer() -> TerminalImageCapabilities {
    TerminalImageCapabilities::V1
}

/// Settings key of the master switch, exactly as the settings window writes it.
const IMAGES_KEY: &str = "terminal.images.enabled";

/// Text an application prints around an image, standing in for the textual
/// fallback an application owns.
const LEADING_TEXT: &[u8] = b"before ";
const TRAILING_TEXT: &[u8] = b" after";

#[derive(Serialize)]
struct Evidence<'a> {
    schema_version: u32,
    status: &'a str,
    engine: &'a str,
    payload_free: bool,
    settings: SettingsEvidence,
    release: ReleaseEvidence,
    advertising: AdvertisingEvidence,
    renderer: RendererEvidence,
    diagnostics: DiagnosticsEvidence,
    cases: BTreeMap<&'a str, &'a str>,
}

/// What the shipped settings surface exposes and what a write leaves on disk.
#[derive(Serialize)]
struct SettingsEvidence {
    /// The dotted key the settings window writes.
    control_key: String,
    /// The human-readable label the settings window renders. Localizing Scribe
    /// translates this string; it never interpolates runtime data.
    control_label: String,
    /// The control is a plain toggle, not a free-text or numeric field.
    control_kind: &'static str,
    /// The observed default, disable, and re-enable outcomes, in order.
    switch_round_trip: String,
    /// The exact TOML line the disable wrote under `[terminal.images]`.
    disabled_toml_line: String,
    /// The exact TOML line the re-enable wrote under `[terminal.images]`.
    reenabled_toml_line: String,
    /// Bytes of the saved config file that matched an image payload pattern.
    payload_bytes_on_disk: usize,
}

/// What one disable transition frees in a session that had latched.
#[derive(Serialize)]
struct ReleaseEvidence {
    transitions: String,
    releases_state: bool,
    definitions_before: usize,
    placements_before: usize,
    session_requested_bytes_before: u64,
    definitions_after: usize,
    placements_after: usize,
    session_requested_bytes_after: u64,
    session_observed_bytes_after: u64,
    pending_transfer_after: bool,
    /// Visible terminal text before and after the release, which must match.
    text_before: String,
    text_after: String,
    /// What the release did to the terminal's own text.
    text_outcome: String,
    /// What a second release on an already-released session did.
    second_release: String,
}

/// What a disabled Scribe tells applications and viewers about itself.
#[derive(Serialize)]
struct AdvertisingEvidence {
    enabled_device_attributes: String,
    disabled_device_attributes: String,
    enabled_kitty_replies: usize,
    disabled_kitty_replies: usize,
    enabled_connection_subset_features: u16,
    disabled_connection_subset_features: u16,
    /// Everything a disabled Scribe declined to claim.
    disabled_advertising: String,
    relatched_after_reenable: bool,
}

/// How a renderer failure is classified apart from a bounded rejection.
#[derive(Serialize)]
struct RendererEvidence {
    /// Failures classified as the renderer itself being unusable.
    renderer_failures: String,
    /// How a bounded per-image rejection is classified instead.
    bounded_rejection: String,
    paint_failure_reason: String,
    limit_rejection_reason: String,
    /// Localized notice a pane shows after a renderer failure.
    scene_notice: String,
    /// Placements the pane still holds while the notice is shown, proving the
    /// notice is additive rather than a scene reset.
    placements_with_notice: usize,
}

/// The localization catalog and the payload-free shape of a diagnostic record.
#[derive(Serialize)]
struct DiagnosticsEvidence {
    reason_count: usize,
    distinct_messages: usize,
    policy_disabled_message: String,
    renderer_unavailable_message: String,
    /// Messages containing an interpolation placeholder or a control byte.
    unsafe_messages: usize,
    /// Serialized diagnostic record, which has no string or byte payload field.
    serialized_rejection: String,
}

pub fn run(fixtures: &Path, evidence_path: &Path) -> Result<(), String> {
    let mut cases: BTreeMap<&str, &str> = BTreeMap::new();

    let settings = verify_settings_surface()?;
    cases.insert("default_on", "pass");
    cases.insert("settings_toggle_present", "pass");
    cases.insert("disable_then_reenable", "pass");
    cases.insert("no_payload_on_disk", "pass");

    let release = verify_release(fixtures)?;
    cases.insert("resource_release", "pass");
    cases.insert("text_fallback_preserved", "pass");

    let advertising = verify_advertising(fixtures)?;
    cases.insert("no_false_kitty_claim", "pass");
    cases.insert("no_false_da_claim", "pass");

    let renderer = verify_renderer_failure(fixtures)?;
    cases.insert("renderer_failure_cleanup", "pass");

    let diagnostics = verify_diagnostics()?;
    cases.insert("localized_diagnostics", "pass");
    cases.insert("payload_free_diagnostics", "pass");

    let evidence = Evidence {
        schema_version: 1,
        status: "pass",
        engine: "scribe image master switch, diagnostics, and settings",
        payload_free: true,
        settings,
        release,
        advertising,
        renderer,
        diagnostics,
        cases,
    };
    write_probe_evidence(evidence_path, &evidence)
}

/// The shipped settings surface offers exactly one default-on toggle, and
/// writing it through the production apply path round-trips on disk without
/// leaving anything but a boolean behind.
fn verify_settings_surface() -> Result<SettingsEvidence, String> {
    let control = page_controls(SettingsPage::Terminal)
        .into_iter()
        .find(|control| control.key == IMAGES_KEY)
        .ok_or_else(|| format!("the Terminal settings page has no {IMAGES_KEY} control"))?;
    if !matches!(control.kind, ControlKind::Toggle) {
        return Err(format!("{IMAGES_KEY} is not a toggle"));
    }
    if control.label.trim().is_empty() || control.label == IMAGES_KEY {
        return Err(format!("{IMAGES_KEY} has no human-readable label"));
    }

    // A config that never mentioned images is enabled: the switch is default-on
    // and rollback is an explicit user action.
    let default_value = current_value(&ScribeConfig::default(), IMAGES_KEY)
        .as_bool()
        .ok_or_else(|| format!("{IMAGES_KEY} did not read back as a boolean"))?;
    if !default_value {
        return Err(String::from("terminal images are not default-on"));
    }
    let absent: ScribeConfig = toml::from_str("[terminal]\nscrollback_lines = 1000\n")
        .map_err(|error| format!("parse config without an images table: {error}"))?;
    if !absent.terminal.images.enabled {
        return Err(String::from("a config without an images table defaulted to disabled"));
    }

    let disabled_value = write_switch(false)?;
    let disabled_toml = read_saved_config()?;
    let disabled_toml_line = images_switch_line(&disabled_toml)?;
    let payload_bytes_on_disk = payload_matches(&disabled_toml);

    let reenabled_value = write_switch(true)?;
    let reenabled_toml = read_saved_config()?;
    let reenabled_toml_line = images_switch_line(&reenabled_toml)?;

    if disabled_value || !reenabled_value {
        return Err(String::from("the settings write path did not round-trip the switch"));
    }
    if disabled_toml_line != "enabled = false" || reenabled_toml_line != "enabled = true" {
        return Err(String::from("the switch is not a plain boolean under [terminal.images]"));
    }
    if payload_bytes_on_disk != 0 || payload_matches(&reenabled_toml) != 0 {
        return Err(String::from("the saved config contains image payload data"));
    }

    Ok(SettingsEvidence {
        control_key: control.key,
        control_label: control.label,
        control_kind: "toggle",
        switch_round_trip: String::from("default_on,disabled_off,reenabled_on"),
        disabled_toml_line,
        reenabled_toml_line,
        payload_bytes_on_disk,
    })
}

/// The `enabled` line inside the saved config's `[terminal.images]` table.
///
/// Scoping to the table matters: several unrelated tables also carry an
/// `enabled` key, and a gate that matched any of them would pass while the
/// image switch stayed wrong.
fn images_switch_line(saved: &str) -> Result<String, String> {
    let mut in_images = false;
    for line in saved.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_images = trimmed == "[terminal.images]";
        } else if in_images && trimmed.starts_with("enabled") {
            return Ok(trimmed.to_string());
        }
    }
    Err(String::from("the saved config has no [terminal.images] switch"))
}

/// Write the master switch through the shipped settings path and read the
/// value back off disk.
fn write_switch(enabled: bool) -> Result<bool, String> {
    let change = format!(r#"{{"key":"{IMAGES_KEY}","value":{enabled}}}"#);
    scribe_client::settings::apply::apply_settings_change(&change)
        .map_err(|error| format!("apply {IMAGES_KEY}={enabled}: {error}"))?;
    let reloaded = load_config().map_err(|error| format!("reload config: {error}"))?;
    Ok(reloaded.terminal.images.enabled)
}

/// Read back the config file the settings path just wrote.
fn read_saved_config() -> Result<String, String> {
    let dir = scribe_common::app::current_config_dir()
        .ok_or_else(|| String::from("no config directory in this environment"))?;
    std::fs::read_to_string(dir.join("config.toml"))
        .map_err(|error| format!("read saved config: {error}"))
}

/// Count occurrences of the pinned fixture's image payload in some artifact.
///
/// The fixture transmits one red pixel as `/wAA`; its raw and hex forms are the
/// only image bytes anything in this probe ever sees, so finding them in a file
/// Scribe wrote is proof of a leak.
fn payload_matches(text: &str) -> usize {
    ["/wAA", "ff0000", "\u{1b}_G"].iter().filter(|needle| text.contains(**needle)).count()
}

/// Disabling a latched session frees its decode admissions, retained buffers,
/// and committed scene while the terminal's text stands untouched.
fn verify_release(fixtures: &Path) -> Result<ReleaseEvidence, String> {
    let mut sharing = SessionImageSharing::new(true);
    sharing.latch(capable_viewer());
    let mut transitions = Vec::new();
    let disable = sharing.set_master_enabled(false);
    transitions.push(describe_transition(disable));
    transitions.push(describe_transition(sharing.set_master_enabled(false)));
    transitions.push(describe_transition(sharing.set_master_enabled(true)));

    let mut probe = Probe::new();
    let mut bytes = LEADING_TEXT.to_vec();
    bytes.extend_from_slice(&read_hex(&fixtures.join(RGB_CLASSIC_FIXTURE))?);
    bytes.extend_from_slice(TRAILING_TEXT);
    let commit = probe.feed(&bytes)?;
    probe
        .images
        .commit_mutations(&commit)
        .map_err(|error| format!("commit fixture mutations: {error}"))?;

    let before = probe.images.state();
    let (session_before, _) =
        probe.images.storage_counters().map_err(|error| format!("counters before: {error}"))?;
    let text_before = probe.visible_text();
    if before.definition_count == 0 || before.placement_count == 0 {
        return Err(String::from("the fixture did not commit an image to release"));
    }
    // The commit's own outputs are charged to the same budget for as long as a
    // reader holds them; production drops them when the read is done, so the
    // release measurement must too.
    drop(commit);

    if probe
        .images
        .release_for_policy_disable()
        .map_err(|error| format!("release after disable: {error}"))?
        .is_none()
    {
        return Err(String::from("a latched session with images released nothing"));
    }
    let after = probe.images.state();
    let (session_after, _) =
        probe.images.storage_counters().map_err(|error| format!("counters after: {error}"))?;
    let text_after = probe.visible_text();

    let idempotent = probe
        .images
        .release_for_policy_disable()
        .map_err(|error| format!("second release after disable: {error}"))?
        .is_none()
        && probe.images.state() == after
        && !probe.images.holds_image_resources();

    if after.definition_count != 0 || after.placement_count != 0 {
        return Err(String::from("disabling left committed image state behind"));
    }
    if after.pending_transfer.is_some() {
        return Err(String::from("disabling left a partial transfer behind"));
    }
    if session_after.requested_current != 0 || session_after.observed_current != 0 {
        return Err(format!(
            "disabling left retained image storage charged: requested={} observed={}",
            session_after.requested_current, session_after.observed_current
        ));
    }
    if text_before != text_after || !text_after.contains("before") || !text_after.contains("after")
    {
        return Err(String::from("disabling images changed the terminal's text"));
    }
    if !idempotent {
        return Err(String::from("a second release after disable was not a no-op"));
    }

    Ok(ReleaseEvidence {
        transitions: transitions.join(","),
        releases_state: disable.releases_state(),
        definitions_before: before.definition_count,
        placements_before: before.placement_count,
        session_requested_bytes_before: session_before.requested_peak,
        definitions_after: after.definition_count,
        placements_after: after.placement_count,
        session_requested_bytes_after: session_after.requested_current,
        session_observed_bytes_after: session_after.observed_current,
        pending_transfer_after: after.pending_transfer.is_some(),
        text_before,
        text_after,
        text_outcome: String::from("preserved,fallback_visible"),
        second_release: String::from("no_op"),
    })
}

fn describe_transition(transition: KillSwitchTransition) -> &'static str {
    match transition {
        KillSwitchTransition::Unchanged => "unchanged",
        KillSwitchTransition::Disabled { cleared_latch: true } => "disabled_cleared_latch",
        KillSwitchTransition::Disabled { cleared_latch: false } => "disabled",
        KillSwitchTransition::Enabled => "enabled",
    }
}

/// A disabled Scribe answers no Kitty probe, drops Sixel from its DA1 reply,
/// advertises an empty subset, and refuses to latch — so no application can
/// conclude images work when they do not.
fn verify_advertising(fixtures: &Path) -> Result<AdvertisingEvidence, String> {
    let mut probe = Probe::new();
    let commit = probe.feed(&read_hex(&fixtures.join("kitty-query-order.hex"))?)?;
    let enabled_kitty_replies = plan_pty_replies(&commit, true).len();
    let disabled_kitty_replies = plan_pty_replies(&commit, false).len();
    if enabled_kitty_replies == 0 {
        return Err(String::from("the query fixture owed no reply while enabled"));
    }
    if disabled_kitty_replies != 0 {
        return Err(String::from("a disabled session answered a Kitty probe"));
    }

    let base = "\u{1b}[?6c";
    let enabled_device_attributes = augment_device_attributes(base, true).into_owned();
    let disabled_device_attributes = augment_device_attributes(base, false).into_owned();
    if !enabled_device_attributes.contains(";4") || disabled_device_attributes.contains(";4") {
        return Err(String::from("DA1 Sixel advertising does not follow the master switch"));
    }

    let viewer = capable_viewer();
    let enabled_subset = effective_connection_subset(viewer, true);
    let disabled_subset = effective_connection_subset(viewer, false);
    if disabled_subset.runtime_enabled || disabled_subset.features.bits() != 0 {
        return Err(String::from("a disabled server advertised an image capability"));
    }

    let mut sharing = SessionImageSharing::new(false);
    let disabled_latch_attempt_enabled = sharing.latch(viewer).runtime_enabled;
    if disabled_latch_attempt_enabled {
        return Err(String::from("a disabled session latched a capability"));
    }
    sharing.set_master_enabled(true);
    let relatched_after_reenable = sharing.latch(viewer).runtime_enabled;
    if !relatched_after_reenable {
        return Err(String::from("a capable viewer could not latch after re-enable"));
    }

    Ok(AdvertisingEvidence {
        enabled_device_attributes,
        disabled_device_attributes,
        enabled_kitty_replies,
        disabled_kitty_replies,
        enabled_connection_subset_features: enabled_subset.features.bits(),
        disabled_connection_subset_features: disabled_subset.features.bits(),
        disabled_advertising: String::from("no_runtime,no_features,no_latch"),
        relatched_after_reenable,
    })
}

/// A failed window operation is a renderer failure that releases GPU state and
/// shows a notice; a bounded per-image rejection is neither.
fn verify_renderer_failure(fixtures: &Path) -> Result<RendererEvidence, String> {
    let paint = GpuiImageError::PaintImage(anyhow::anyhow!("window paint refused"));
    let dropped = GpuiImageError::DropImage(anyhow::anyhow!("atlas drop refused"));
    let limited = GpuiImageError::Bound(ImageBoundError::LimitExceeded(
        ImageLimitName::ViewProjectedGpuBytes,
    ));
    if !paint.is_renderer_failure() || !dropped.is_renderer_failure() {
        return Err(String::from("a failed window operation was not treated as renderer failure"));
    }
    if limited.is_renderer_failure() {
        return Err(String::from("a bounded image rejection was treated as renderer failure"));
    }
    if paint.rejection_reason() != TerminalImageRejectionReason::RendererUnavailable {
        return Err(String::from("a renderer failure did not report renderer_unavailable"));
    }

    // The pane keeps its scene and shows the localized notice beside it. The
    // scene comes from the pinned client-scene corpus, so the notice lands on a
    // real published scene rather than an empty one.
    let mut scene = staged_scene(fixtures)?;
    let before = scene.committed();
    if before.definitions.is_empty() || before.placements().is_empty() {
        return Err(String::from("the pinned scene fixture published nothing to preserve"));
    }
    let generation = TerminalImageGeneration(1);
    let sequence = TerminalOutputSequence(2);
    scene
        .apply(TerminalImageLiveMessage::Begin { generation, sequence })
        .map_err(|error| format!("begin renderer-failure burst: {error}"))?;
    scene
        .apply(TerminalImageLiveMessage::Update {
            generation,
            sequence,
            update: TerminalImageUpdate::Rejected {
                rejection: TerminalImageRejection {
                    reason: paint.rejection_reason(),
                    protocol: Some(TerminalImageProtocol::Kitty),
                    action: Some(TerminalImageAction::Render),
                    width: Some(1),
                    height: Some(1),
                    observed: None,
                    limit: None,
                },
            },
        })
        .map_err(|error| format!("record renderer-failure rejection: {error}"))?;
    scene
        .apply(TerminalImageLiveMessage::Commit { generation, sequence })
        .map_err(|error| format!("commit renderer-failure burst: {error}"))?;

    let published = scene.committed();
    let scene_notice = published
        .diagnostic_notice()
        .ok_or_else(|| String::from("a renderer failure produced no pane notice"))?;
    if scene_notice != TerminalImageRejectionReason::RendererUnavailable.localized_message() {
        return Err(String::from("the pane notice is not the localized catalog message"));
    }
    if CommittedImageScene::default().diagnostic_notice().is_some() {
        return Err(String::from("a scene with no rejection still shows a notice"));
    }
    if published.definitions != before.definitions || published.placements() != before.placements()
    {
        return Err(String::from("a renderer-failure notice replaced the pane's scene"));
    }

    Ok(RendererEvidence {
        renderer_failures: String::from("paint,drop"),
        bounded_rejection: String::from("not_renderer_failure"),
        paint_failure_reason: format!("{:?}", paint.rejection_reason()),
        limit_rejection_reason: format!("{:?}", limited.rejection_reason()),
        scene_notice: scene_notice.to_string(),
        placements_with_notice: published.placements().len(),
    })
}

/// Publish the pinned client-scene corpus's first burst into a live scene.
///
/// Reusing the corpus keeps this gate on the same definitions and placements
/// the client-scene gate already pins, so a notice is proven additive against
/// a real published scene instead of an empty one.
fn staged_scene(fixtures: &Path) -> Result<LiveImageScene, String> {
    let path = fixtures.join("client-scene.json");
    let raw = std::fs::read(&path).map_err(|error| format!("read {}: {error}", path.display()))?;
    let fixture: serde_json::Value =
        serde_json::from_slice(&raw).map_err(|error| format!("decode scene fixture: {error}"))?;
    let messages = fixture
        .get("live_messages")
        .ok_or_else(|| String::from("the scene fixture has no live_messages"))?;
    let messages: Vec<TerminalImageLiveMessage> = serde_json::from_value(messages.clone())
        .map_err(|error| format!("decode scene fixture messages: {error}"))?;

    let mut scene = LiveImageScene::default();
    for message in messages {
        let commits = matches!(message, TerminalImageLiveMessage::Commit { .. });
        scene.apply(message).map_err(|error| format!("apply scene fixture: {error}"))?;
        if commits {
            return Ok(scene);
        }
    }
    Err(String::from("the scene fixture never committed a burst"))
}

/// Every frozen rejection category has one distinct, human-readable, and
/// payload-free message, and the record that carries it cannot hold bytes.
fn verify_diagnostics() -> Result<DiagnosticsEvidence, String> {
    let messages: Vec<&'static str> =
        TerminalImageRejectionReason::ALL.iter().map(|reason| reason.localized_message()).collect();
    let distinct: std::collections::BTreeSet<&&str> = messages.iter().collect();
    let unsafe_messages = messages
        .iter()
        .filter(|message| {
            message.trim().is_empty()
                || message.contains('{')
                || message.contains('%')
                || message.chars().any(char::is_control)
                || message.chars().any(|c| c.is_ascii_digit())
        })
        .count();
    if distinct.len() != messages.len() {
        return Err(String::from("two rejection categories share one message"));
    }
    if unsafe_messages != 0 {
        return Err(String::from("a diagnostic message can interpolate runtime data"));
    }

    let rejection = TerminalImageRejection {
        reason: TerminalImageRejectionReason::QuotaExceeded,
        protocol: Some(TerminalImageProtocol::Sixel),
        action: Some(TerminalImageAction::Transmit),
        width: Some(4096),
        height: Some(4096),
        observed: Some(67_108_865),
        limit: Some(ImageLimitName::CanonicalRgbaBytes),
    };
    let serialized = serde_json::to_string(&rejection)
        .map_err(|error| format!("serialize diagnostic record: {error}"))?;
    if payload_matches(&serialized) != 0 {
        return Err(String::from("a diagnostic record carried image payload"));
    }

    Ok(DiagnosticsEvidence {
        reason_count: messages.len(),
        distinct_messages: distinct.len(),
        policy_disabled_message: TerminalImageRejectionReason::PolicyDisabled
            .localized_message()
            .to_string(),
        renderer_unavailable_message: TerminalImageRejectionReason::RendererUnavailable
            .localized_message()
            .to_string(),
        unsafe_messages,
        serialized_rejection: serialized,
    })
}
