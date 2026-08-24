//! Stop-hook classifier — provider-independent heuristic that maps an AI
//! tool's last-message text to either `AiState::IdlePrompt` or
//! `AiState::WaitingForInput`.
//!
//! Replaces the per-provider shell regexes in `dist/detect-claude-question.sh`
//! and `dist/detect-codex-question.sh` per spec FR-013a.
//!
//! See `specs/003-ai-hook-channel/research.md` Decision 5 for the rule set.

use std::sync::OnceLock;

use regex::Regex;
use scribe_common::ai_state::AiState;

/// Number of trailing non-empty lines the heuristic considers (matches the
/// shell scripts' `tail -n 20`).
const TAIL_LINE_WINDOW: usize = 20;

/// Question-phrase pattern (case-insensitive).
const QUESTION_PHRASES_PATTERN: &str = "(?i)(would you like|should i|do you want|which option|\
     please (choose|select|pick)|how (should|would|do|to)|\
     what (should|would|do)|let me know|\
     your (choice|preference|call))";

/// Approval/review-phrase pattern (case-insensitive).
const APPROVAL_PHRASES_PATTERN: &str = "(?i)(please (review|approve)|review and approve|once approved|\
     approve (the|this|it|above)|waiting for.*(approval|review)|\
     i.ll execute.*once approved|confirm (the|this|before)|\
     ready to (proceed|execute|start)|proceed\\?)";

/// Trailing `?` on any line (after whitespace strip).
const TRAILING_QUESTION_MARK_PATTERN: &str = r"\?\s*$";

/// Get a static reference to the cached compiled regex, or `None` if the
/// pattern failed to compile. Failure is silent (returns `None` → never
/// matches) so a regex bug never panics the server.
fn cached_regex(slot: &'static OnceLock<Option<Regex>>, pattern: &str) -> Option<&'static Regex> {
    slot.get_or_init(|| Regex::new(pattern).ok()).as_ref()
}

fn question_phrases() -> Option<&'static Regex> {
    static RE: OnceLock<Option<Regex>> = OnceLock::new();
    cached_regex(&RE, QUESTION_PHRASES_PATTERN)
}

fn approval_phrases() -> Option<&'static Regex> {
    static RE: OnceLock<Option<Regex>> = OnceLock::new();
    cached_regex(&RE, APPROVAL_PHRASES_PATTERN)
}

fn trailing_question_mark() -> Option<&'static Regex> {
    static RE: OnceLock<Option<Regex>> = OnceLock::new();
    cached_regex(&RE, TRAILING_QUESTION_MARK_PATTERN)
}

/// Classify the assistant's last-message text as either waiting or idle.
///
/// Algorithm (matches `dist/detect-claude-question.sh`):
///   1. Strip fenced code blocks (triple-backtick toggles).
///   2. Take the last `TAIL_LINE_WINDOW` non-empty lines.
///   3. If any line ends with `?`, matches a question phrase, or matches an
///      approval/review phrase → `WaitingForInput`.
///   4. Otherwise → `IdlePrompt`.
#[must_use]
pub fn classify(last_message: &str) -> AiState {
    let stripped = strip_code_fences(last_message);
    let tail = tail_non_empty_lines(&stripped, TAIL_LINE_WINDOW);

    for line in &tail {
        if trailing_question_mark().is_some_and(|re| re.is_match(line))
            || question_phrases().is_some_and(|re| re.is_match(line))
            || approval_phrases().is_some_and(|re| re.is_match(line))
        {
            return AiState::WaitingForInput;
        }
    }

    AiState::IdlePrompt
}

/// Remove fenced code blocks delimited by lines starting with three
/// backticks. `CommonMark` allows up to 3 leading spaces of indentation on
/// the fence delimiter, so we check the trimmed-start prefix instead of
/// the raw prefix — otherwise `   ``` Should I continue?` (an indented
/// fence with a question inside) would slip past the filter and produce
/// a false-positive `WaitingForInput`.
fn strip_code_fences(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut inside = false;
    for line in input.lines() {
        if line.trim_start().starts_with("```") {
            inside = !inside;
            continue;
        }
        if !inside {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

/// Last `n` lines after filtering out blank/whitespace-only lines.
fn tail_non_empty_lines(input: &str, n: usize) -> Vec<&str> {
    let mut all: Vec<&str> = input.lines().filter(|line| !line.trim().is_empty()).collect();
    if all.len() > n {
        let drop = all.len() - n;
        all.drain(..drop);
    }
    all
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn waiting_and_idle_examples() {
        for message in [
            "Want me to proceed?",
            "1. Add auth\n2. Add tests\n\nWhich would you like first?\n\nThese options will help me proceed.",
            "Should I apply this change.",
            "Would you like me to continue.",
            "Please review the proposed diff before I land it.",
            "I will execute the migration once approved.",
            "Ready to proceed?",
        ] {
            assert_eq!(classify(message), AiState::WaitingForInput, "message: {message:?}");
        }
        for message in ["Done. All files updated successfully.", ""] {
            assert_eq!(classify(message), AiState::IdlePrompt, "message: {message:?}");
        }
    }

    #[test]
    fn question_inside_fenced_code_block_does_not_trigger() {
        let msg = "Here is the example code:\n```\nfn foo() -> bool {\n    // Should I return true?\n    true\n}\n```\nAll done.";
        assert_eq!(classify(msg), AiState::IdlePrompt);
    }

    #[test]
    fn question_inside_indented_fenced_code_block_does_not_trigger() {
        // CommonMark allows up to 3 spaces of indentation on the fence.
        // The classifier must trim-start before checking the prefix.
        let msg = "List item with a snippet:\n   ```\n   should i wait?\n   ```\nAll done.";
        assert_eq!(classify(msg), AiState::IdlePrompt);
    }

    #[test]
    fn tail_window_limits_search_to_last_20_lines() {
        // 25 lines, only line 1 contains a question. Lines 2-25 are plain.
        // The trailing-question heuristic should NOT match because line 1 is
        // outside the tail-20 window.
        let mut msg = String::from("Want me to proceed?\n");
        for i in 0..24 {
            msg.push_str("Line ");
            msg.push_str(&i.to_string());
            msg.push_str(".\n");
        }
        assert_eq!(classify(&msg), AiState::IdlePrompt);
    }
}
