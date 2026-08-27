//! Pure classification, annotation placement, and theme-colour derivation for
//! failed link opens.
//!
//! Classification receives only typed opener results and never reads child
//! stdout or stderr. Layout and colour math have no renderer state.

use std::{
    io::ErrorKind,
    ops::RangeInclusive,
    path::{Path, PathBuf},
};

/// Result reported by an attempted system-link opener.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OpenOutcome {
    /// The opener command could not be spawned.
    SpawnError(ErrorKind),
    /// The opener exited; `None` means it ended without an exit code.
    Exited { code: Option<i32> },
}

/// Origin of the target sent to the system opener.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OpenTargetKind {
    /// A URL found by heuristic detection.
    Url,
    /// A file-system path found by heuristic detection.
    Path,
    /// A URI supplied by an OSC 8 hyperlink.
    Osc8,
}

/// Target metadata available to the failed-open classifier.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OpenTarget {
    /// Whether the target came from a URL, path, or OSC 8 span.
    pub kind: OpenTargetKind,
    /// URI scheme, when the target has one.
    pub scheme: Option<String>,
    /// Resolved file-system path for path and `file:` targets.
    pub resolved_path: Option<PathBuf>,
}

/// Narrowest pane that can show a failed-link annotation.
pub const MIN_ANNOTATION_COLS: usize = 12;
/// Upright failure mark painted at the start of the annotation band.
pub const ANNOTATION_MARK: char = '✗';

const BAND_PREFIX_COLS: usize = 2; // mark + space
const HEAD_PREFIX_COLS: usize = 2; // corner + space
const TAIL_SUFFIX_COLS: usize = 3; // space + horizontal + corner
const MIN_MESSAGE_COLS: usize = 1;

/// Whether the annotation row is above or below the clicked run.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AnnotationSide {
    /// Normal placement, on the row above the clicked run.
    Above,
    /// Top-edge placement, on the row below the clicked run.
    Below,
}

/// Which end of the clicked run owns the annotation corner.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AnnotationAnchor {
    /// The corner sits over the run's first column.
    Head,
    /// The corner sits over the run's last column.
    Tail,
}

/// Cell-column placement for one failed-link annotation.
///
/// `band_col` is the upright [`ANNOTATION_MARK`]; the italic message begins two
/// columns later. `joinery_col` is the first cell of [`Self::joinery`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AnnotationLayout {
    /// Viewport row containing the annotation.
    pub row: usize,
    /// First column of the one- or two-cell joinery string.
    pub joinery_col: usize,
    /// First opaque band column, containing [`ANNOTATION_MARK`].
    pub band_col: usize,
    /// Whether the annotation is above or below the clicked run.
    pub side: AnnotationSide,
    /// Whether joinery points to the run head or tail.
    pub anchor: AnnotationAnchor,
    /// Head-preserving, pane-clamped failure message without the mark.
    pub message: String,
}

impl AnnotationLayout {
    /// Lay out a failure message beside a clicked run.
    ///
    /// `run` contains inclusive viewport columns. Invalid run coordinates,
    /// panes below [`MIN_ANNOTATION_COLS`], and panes without an adjacent
    /// annotation row return `None`.
    #[must_use]
    pub fn compute(
        pane_cols: usize,
        pane_rows: usize,
        clicked_row: usize,
        run: RangeInclusive<usize>,
        message: &str,
    ) -> Option<Self> {
        let (run_head, run_tail) = run.into_inner();
        if pane_cols < MIN_ANNOTATION_COLS
            || pane_rows < 2
            || clicked_row >= pane_rows
            || run_head > run_tail
            || run_tail >= pane_cols
        {
            return None;
        }

        let (side, row) = if clicked_row == 0 {
            (AnnotationSide::Below, 1)
        } else {
            (AnnotationSide::Above, clicked_row - 1)
        };
        let message_cols = message.chars().count();
        let head_width = HEAD_PREFIX_COLS + BAND_PREFIX_COLS + message_cols;

        if run_head.saturating_add(head_width) <= pane_cols {
            return Some(Self {
                row,
                joinery_col: run_head,
                band_col: run_head + HEAD_PREFIX_COLS,
                side,
                anchor: AnnotationAnchor::Head,
                message: message.to_owned(),
            });
        }

        let tail_message_cols = (run_tail + 1).saturating_sub(BAND_PREFIX_COLS + TAIL_SUFFIX_COLS);
        if tail_message_cols >= MIN_MESSAGE_COLS {
            let message = truncate_message(message, tail_message_cols);
            let band_cols = BAND_PREFIX_COLS + message.chars().count();
            return Some(Self {
                row,
                joinery_col: run_tail - 1,
                band_col: run_tail + 1 - TAIL_SUFFIX_COLS - band_cols,
                side,
                anchor: AnnotationAnchor::Tail,
                message,
            });
        }

        // A very short run at the left edge cannot hold tail joinery plus even
        // one message cell. A head layout truncated to its remaining columns is
        // the only in-grid representation, so its final geometry does not
        // overrun.
        let head_message_cols = pane_cols - run_head - HEAD_PREFIX_COLS - BAND_PREFIX_COLS;
        Some(Self {
            row,
            joinery_col: run_head,
            band_col: run_head + HEAD_PREFIX_COLS,
            side,
            anchor: AnnotationAnchor::Head,
            message: truncate_message(message, head_message_cols),
        })
    }

    /// Box-drawing cells connecting the annotation row to its clicked run.
    #[must_use]
    pub const fn joinery(&self) -> &'static str {
        match (self.side, self.anchor) {
            (AnnotationSide::Above, AnnotationAnchor::Head) => "┌",
            (AnnotationSide::Above, AnnotationAnchor::Tail) => "─┐",
            (AnnotationSide::Below, AnnotationAnchor::Head) => "└",
            (AnnotationSide::Below, AnnotationAnchor::Tail) => "─┘",
        }
    }

    /// Column containing the vertical corner over the clicked run.
    #[must_use]
    pub const fn corner_col(&self) -> usize {
        match self.anchor {
            AnnotationAnchor::Head => self.joinery_col,
            AnnotationAnchor::Tail => self.joinery_col + 1,
        }
    }

    /// Number of opaque cells in the band, including the mark and following
    /// space.
    #[must_use]
    pub fn band_cols(&self) -> usize {
        BAND_PREFIX_COLS + self.message.chars().count()
    }
}

fn truncate_message(message: &str, max_cols: usize) -> String {
    let count = message.chars().count();
    if count <= max_cols {
        return message.to_owned();
    }
    if max_cols == 0 {
        return String::new();
    }
    let mut truncated: String = message.chars().take(max_cols - 1).collect();
    truncated.push('…');
    truncated
}

/// Theme-derived colours for a failed-link annotation.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AnnotationColors {
    /// Opaque annotation-band fill.
    pub band: [f32; 4],
    /// Italic failure-message text.
    pub text: [f32; 4],
    /// Box-drawing joinery outside the band.
    pub joinery: [f32; 4],
}

impl AnnotationColors {
    /// Derive annotation colours from the active terminal background and ANSI
    /// red, both supplied as sRGB channels.
    #[must_use]
    pub fn from_theme(background: [f32; 4], ansi_red: [f32; 4]) -> Self {
        let background = rgb(background);
        let ansi_red = rgb(ansi_red);
        let [bg_red, bg_green, bg_blue] = background;
        let [red, green, blue] = ansi_red;
        let band = [
            quantize(bg_red.mul_add(0.92, red * 0.08)),
            quantize(bg_green.mul_add(0.92, green * 0.08)),
            quantize(bg_blue.mul_add(0.92, blue * 0.08)),
        ];
        let text = readable_lightened_red(ansi_red, band);
        Self { band: rgba(band, 1.0), text: rgba(text, 1.0), joinery: rgba(ansi_red, 0.6) }
    }
}

fn rgb([red, green, blue, _]: [f32; 4]) -> [f32; 3] {
    [red.clamp(0.0, 1.0), green.clamp(0.0, 1.0), blue.clamp(0.0, 1.0)]
}

fn rgba([red, green, blue]: [f32; 3], alpha: f32) -> [f32; 4] {
    [red, green, blue, alpha]
}

fn quantize(channel: f32) -> f32 {
    (channel.clamp(0.0, 1.0) * 255.0).round() / 255.0
}

fn readable_lightened_red(ansi_red: [f32; 3], band: [f32; 3]) -> [f32; 3] {
    const MIN_CONTRAST: f32 = 4.5;

    let (hue, saturation, lightness) = rgb_to_hsl(ansi_red);
    let candidate_lightness = (lightness + 0.1).min(1.0);
    let candidate = hsl_to_rgb(hue, saturation, candidate_lightness);
    if contrast_ratio(candidate, band) >= MIN_CONTRAST {
        return candidate;
    }

    let dark_contrast = contrast_ratio(hsl_to_rgb(hue, saturation, 0.0), band);
    let light_contrast = contrast_ratio(hsl_to_rgb(hue, saturation, 1.0), band);
    let mut passing = if dark_contrast >= light_contrast { 0.0 } else { 1.0 };
    let mut failing = candidate_lightness;
    for _ in 0..32 {
        let midpoint = f32::midpoint(passing, failing);
        if contrast_ratio(hsl_to_rgb(hue, saturation, midpoint), band) >= MIN_CONTRAST {
            passing = midpoint;
        } else {
            failing = midpoint;
        }
    }
    hsl_to_rgb(hue, saturation, passing)
}

fn rgb_to_hsl([red, green, blue]: [f32; 3]) -> (f32, f32, f32) {
    let max = red.max(green).max(blue);
    let min = red.min(green).min(blue);
    let lightness = f32::midpoint(max, min);
    let delta = max - min;
    if delta <= f32::EPSILON {
        return (0.0, 0.0, lightness);
    }

    let saturation = delta / (1.0 - (2.0 * lightness - 1.0).abs());
    let hue = if red >= green && red >= blue {
        ((green - blue) / delta).rem_euclid(6.0)
    } else if green >= blue {
        (blue - red) / delta + 2.0
    } else {
        (red - green) / delta + 4.0
    } / 6.0;
    (hue, saturation, lightness)
}

fn hsl_to_rgb(hue: f32, saturation: f32, lightness: f32) -> [f32; 3] {
    let chroma = (1.0 - (2.0 * lightness - 1.0).abs()) * saturation;
    let hue = hue * 6.0;
    let second = chroma * (1.0 - (hue.rem_euclid(2.0) - 1.0).abs());
    let (red, green, blue) = if hue < 1.0 {
        (chroma, second, 0.0)
    } else if hue < 2.0 {
        (second, chroma, 0.0)
    } else if hue < 3.0 {
        (0.0, chroma, second)
    } else if hue < 4.0 {
        (0.0, second, chroma)
    } else if hue < 5.0 {
        (second, 0.0, chroma)
    } else {
        (chroma, 0.0, second)
    };
    let match_value = lightness - chroma / 2.0;
    [red, green, blue].map(|channel| quantize(channel + match_value))
}

fn contrast_ratio(left: [f32; 3], right: [f32; 3]) -> f32 {
    let left = relative_luminance(left);
    let right = relative_luminance(right);
    (left.max(right) + 0.05) / (left.min(right) + 0.05)
}

fn relative_luminance([red, green, blue]: [f32; 3]) -> f32 {
    fn linear(channel: f32) -> f32 {
        if channel <= 0.040_45 { channel / 12.92 } else { ((channel + 0.055) / 1.055).powf(2.4) }
    }

    0.2126f32.mul_add(linear(red), 0.7152f32.mul_add(linear(green), 0.0722 * linear(blue)))
}

/// Return the user message for an unsuccessful link open.
///
/// The branches deliberately follow the normative precedence: a missing
/// command, mailto, a missing path/file, a safe non-file scheme, then the
/// exit-code and code-less fallbacks. A successful exit returns no message.
#[must_use]
pub fn classify_open_failure(
    command: &str,
    outcome: OpenOutcome,
    target: &OpenTarget,
) -> Option<String> {
    match outcome {
        OpenOutcome::SpawnError(ErrorKind::NotFound) => Some(format!("{command} is not installed")),
        OpenOutcome::SpawnError(_) => Some(format!("{command} failed")),
        OpenOutcome::Exited { code: Some(0) } => None,
        OpenOutcome::Exited { code } => Some(classify_exit_failure(command, code, target)),
    }
}

fn classify_exit_failure(command: &str, code: Option<i32>, target: &OpenTarget) -> String {
    let Some(code) = code else {
        return format!("{command} failed");
    };
    if target.is_mailto() {
        return "no mail client configured".to_owned();
    }
    if target.is_path_or_file() {
        if target.resolved_path.as_deref().is_some_and(path_is_missing) {
            return "file no longer exists".to_owned();
        }
    } else if let Some(scheme) = safe_scheme(target.scheme.as_deref()) {
        return format!("no application handles {scheme} links");
    }
    format!("{command} exited {code}")
}

impl OpenTarget {
    fn is_mailto(&self) -> bool {
        self.scheme.as_deref().is_some_and(|scheme| scheme.eq_ignore_ascii_case("mailto"))
    }

    fn is_path_or_file(&self) -> bool {
        self.kind == OpenTargetKind::Path
            || self.scheme.as_deref().is_some_and(|scheme| scheme.eq_ignore_ascii_case("file"))
    }
}

fn path_is_missing(path: &Path) -> bool {
    std::fs::metadata(path).is_err()
}

/// Return a scheme that is safe to interpolate into a user-facing message.
fn safe_scheme(scheme: Option<&str>) -> Option<String> {
    let scheme = scheme?;
    if scheme.is_empty() || scheme.len() > 16 || !scheme.is_ascii() {
        return None;
    }
    let mut bytes = scheme.bytes();
    if !bytes.next()?.is_ascii_alphabetic()
        || !bytes.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'-' | b'.'))
    {
        return None;
    }
    Some(scheme.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use std::{io::ErrorKind, path::PathBuf};

    use super::{
        AnnotationAnchor, AnnotationColors, AnnotationLayout, AnnotationSide, OpenOutcome,
        OpenTarget, OpenTargetKind, classify_open_failure, contrast_ratio, rgb,
    };

    fn target(
        kind: OpenTargetKind,
        scheme: Option<&str>,
        resolved_path: Option<PathBuf>,
    ) -> OpenTarget {
        OpenTarget { kind, scheme: scheme.map(str::to_owned), resolved_path }
    }

    fn uri(scheme: &str) -> OpenTarget {
        target(OpenTargetKind::Url, Some(scheme), None)
    }

    // @lat: [[client#Client#URL Detection#Failed link opens]]
    #[test]
    fn classifier_truth_table_covers_all_six_messages() {
        let missing_path = PathBuf::from("/scribe-link-feedback-file-does-not-exist");
        let existing_path = std::env::current_exe().expect("test executable exists");
        let cases = [
            (
                "open",
                OpenOutcome::SpawnError(ErrorKind::NotFound),
                uri("ssh"),
                "open is not installed",
            ),
            (
                "xdg-open",
                OpenOutcome::Exited { code: Some(3) },
                uri("mailto"),
                "no mail client configured",
            ),
            (
                "xdg-open",
                OpenOutcome::Exited { code: Some(4) },
                target(OpenTargetKind::Path, None, Some(missing_path)),
                "file no longer exists",
            ),
            (
                "xdg-open",
                OpenOutcome::Exited { code: Some(5) },
                uri("ssh"),
                "no application handles ssh links",
            ),
            (
                "code",
                OpenOutcome::Exited { code: Some(6) },
                target(OpenTargetKind::Path, None, Some(existing_path)),
                "code exited 6",
            ),
            ("open", OpenOutcome::Exited { code: None }, uri("https"), "open failed"),
        ];

        for (command, outcome, target, expected) in cases {
            assert_eq!(classify_open_failure(command, outcome, &target).as_deref(), Some(expected));
        }
    }

    #[test]
    fn classifier_preserves_precedence_and_stats_file_targets() {
        let missing_file_uri = target(
            OpenTargetKind::Url,
            Some("file"),
            Some(PathBuf::from("/scribe-link-feedback-file-uri-does-not-exist")),
        );
        assert_eq!(
            classify_open_failure(
                "xdg-open",
                OpenOutcome::Exited { code: Some(7) },
                &missing_file_uri
            ),
            Some("file no longer exists".to_owned())
        );

        let existing_file_uri = target(
            OpenTargetKind::Url,
            Some("file"),
            Some(std::env::current_exe().expect("test executable exists")),
        );
        assert_eq!(
            classify_open_failure(
                "xdg-open",
                OpenOutcome::Exited { code: Some(7) },
                &existing_file_uri
            ),
            Some("xdg-open exited 7".to_owned())
        );

        assert_eq!(
            classify_open_failure(
                "xdg-open",
                OpenOutcome::Exited { code: Some(7) },
                &uri("mailto")
            ),
            Some("no mail client configured".to_owned())
        );
    }

    #[test]
    fn classifier_validates_and_caps_scheme() {
        assert_eq!(
            classify_open_failure(
                "xdg-open",
                OpenOutcome::Exited { code: Some(8) },
                &uri("SSH+V2")
            ),
            Some("no application handles ssh+v2 links".to_owned())
        );
        assert_eq!(
            classify_open_failure(
                "xdg-open",
                OpenOutcome::Exited { code: Some(8) },
                &uri("abcdefghijklmnop")
            ),
            Some("no application handles abcdefghijklmnop links".to_owned())
        );
        for scheme in ["abcdefghijklmnopq", "ssh\nwarning", "sshé"] {
            assert_eq!(
                classify_open_failure(
                    "xdg-open",
                    OpenOutcome::Exited { code: Some(8) },
                    &uri(scheme)
                ),
                Some("xdg-open exited 8".to_owned())
            );
        }
    }

    #[test]
    fn classifier_templates_the_final_command_name() {
        let no_scheme = target(OpenTargetKind::Url, None, None);
        assert_eq!(
            classify_open_failure("code", OpenOutcome::SpawnError(ErrorKind::NotFound), &no_scheme),
            Some("code is not installed".to_owned())
        );
        assert_eq!(
            classify_open_failure("code", OpenOutcome::Exited { code: Some(9) }, &no_scheme),
            Some("code exited 9".to_owned())
        );
        assert_eq!(
            classify_open_failure(
                "code",
                OpenOutcome::SpawnError(ErrorKind::PermissionDenied),
                &no_scheme
            ),
            Some("code failed".to_owned())
        );
        assert_eq!(
            classify_open_failure("code", OpenOutcome::Exited { code: None }, &uri("ssh")),
            Some("code failed".to_owned())
        );
        assert_eq!(
            classify_open_failure("code", OpenOutcome::Exited { code: Some(0) }, &no_scheme),
            None
        );
    }

    // @lat: [[client#Client#URL Detection#Failed link opens]]
    #[test]
    fn annotation_head_anchors_above_the_run() {
        let layout = AnnotationLayout::compute(80, 24, 9, 18..=36, "no mail client configured")
            .expect("layout");

        assert_eq!(layout.row, 8);
        assert_eq!(layout.side, AnnotationSide::Above);
        assert_eq!(layout.anchor, AnnotationAnchor::Head);
        assert_eq!(layout.joinery_col, 18);
        assert_eq!(layout.corner_col(), 18);
        assert_eq!(layout.band_col, 20);
        assert_eq!(layout.joinery(), "┌");
    }

    #[test]
    fn annotation_switches_to_tail_only_past_the_head_fit_threshold() {
        let message = "no mail client configured";
        let message_cols = message.chars().count();
        let last_head_col = 40 - 4 - message_cols;
        let head = AnnotationLayout::compute(40, 10, 5, last_head_col..=39, message)
            .expect("threshold layout");
        let tail = AnnotationLayout::compute(40, 10, 5, last_head_col + 1..=39, message)
            .expect("clamped layout");

        assert_eq!(head.anchor, AnnotationAnchor::Head);
        assert_eq!(tail.anchor, AnnotationAnchor::Tail);
        assert_eq!(tail.corner_col(), 39);
        assert_eq!(tail.joinery(), "─┐");
    }

    #[test]
    fn annotation_flips_below_only_at_the_top_edge() {
        let below = AnnotationLayout::compute(80, 24, 0, 18..=36, "opener failed")
            .expect("top-edge layout");
        let above = AnnotationLayout::compute(80, 24, 1, 18..=36, "opener failed")
            .expect("second-row layout");

        assert_eq!((below.side, below.row, below.joinery()), (AnnotationSide::Below, 1, "└"));
        assert_eq!((above.side, above.row, above.joinery()), (AnnotationSide::Above, 0, "┌"));
    }

    #[test]
    fn annotation_truncates_with_an_ellipsis_to_the_pane() {
        let layout = AnnotationLayout::compute(12, 4, 2, 2..=11, "opener could not be started")
            .expect("narrow layout");

        assert_eq!(layout.anchor, AnnotationAnchor::Tail);
        assert_eq!(layout.message, "opener…");
        assert_eq!(layout.band_col + layout.band_cols() + 3, 12);
    }

    #[test]
    fn annotation_suppresses_tiny_or_rowless_panes() {
        assert_eq!(AnnotationLayout::compute(11, 4, 2, 2..=8, "opener failed"), None);
        assert_eq!(AnnotationLayout::compute(40, 1, 0, 2..=8, "opener failed"), None);
    }

    #[test]
    fn annotation_colors_match_the_demo_theme() {
        let background = [14.0 / 255.0, 14.0 / 255.0, 16.0 / 255.0, 1.0];
        let ansi_red = [239.0 / 255.0, 68.0 / 255.0, 68.0 / 255.0, 1.0];
        let colors = AnnotationColors::from_theme(background, ansi_red);

        assert_color_eq(colors.band, [32.0 / 255.0, 18.0 / 255.0, 20.0 / 255.0, 1.0]); // #201214
        assert_color_eq(colors.text, [243.0 / 255.0, 115.0 / 255.0, 115.0 / 255.0, 1.0]); // #f37373
        assert_color_eq(colors.joinery, [239.0 / 255.0, 68.0 / 255.0, 68.0 / 255.0, 0.6]);
    }

    #[test]
    fn annotation_text_clamps_to_body_text_contrast() {
        let colors = AnnotationColors::from_theme(
            [245.0 / 255.0, 245.0 / 255.0, 245.0 / 255.0, 1.0],
            [239.0 / 255.0, 68.0 / 255.0, 68.0 / 255.0, 1.0],
        );

        assert!(contrast_ratio(rgb(colors.text), rgb(colors.band)) >= 4.5);
    }

    fn assert_color_eq(actual: [f32; 4], expected: [f32; 4]) {
        for (actual_channel, expected_channel) in actual.into_iter().zip(expected) {
            assert!(
                (actual_channel - expected_channel).abs() < 1e-6,
                "{actual_channel} != {expected_channel}"
            );
        }
    }
}
