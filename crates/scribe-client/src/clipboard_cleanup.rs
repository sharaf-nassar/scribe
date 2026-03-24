//! Clipboard text cleanup for Claude Code sessions.
//!
//! Two transforms are applied in sequence when the active session is running
//! Claude Code and the user has the `claude_copy_cleanup` setting enabled:
//!
//! 1. **Dedent** — strip the minimum shared leading whitespace from all lines.
//! 2. **Unwrap** — rejoin hard-wrapped prose lines into flowing paragraphs.

/// Apply configured cleanup transforms to terminal-selected text.
///
/// When `claude_active` is `false` or `cleanup_enabled` is `false` the text
/// is returned unchanged.
#[allow(
    clippy::fn_params_excessive_bools,
    reason = "two independent booleans (AI state + config toggle) are clearer than an enum here"
)]
pub fn prepare_copy_text(text: &str, claude_active: bool, cleanup_enabled: bool) -> String {
    if !claude_active || !cleanup_enabled {
        return text.to_owned();
    }
    let dedented = dedent(text);
    unwrap_lines(&dedented)
}

// ---------------------------------------------------------------------------
// Dedent
// ---------------------------------------------------------------------------

/// Strip the minimum shared leading whitespace from all non-empty lines.
///
/// Empty lines are preserved as-is and are not counted when computing the
/// minimum indent.  Returns the original text unchanged when the minimum
/// indent is zero.
fn dedent(text: &str) -> String {
    let trailing_newline = text.ends_with('\n');
    let lines: Vec<&str> = text.lines().collect();

    let min_indent = lines
        .iter()
        .filter(|l| !l.is_empty())
        .map(|l| l.chars().take_while(|c| *c == ' ').count())
        .min()
        .unwrap_or(0);

    if min_indent == 0 {
        return text.to_owned();
    }

    let mut result = lines
        .iter()
        .map(|l| if l.is_empty() { String::new() } else { strip_leading_chars(l, min_indent) })
        .collect::<Vec<_>>()
        .join("\n");
    if trailing_newline {
        result.push('\n');
    }
    result
}

/// Remove the first `n` characters from `s`.
fn strip_leading_chars(s: &str, n: usize) -> String {
    s.chars().skip(n).collect()
}

// ---------------------------------------------------------------------------
// Unwrap
// ---------------------------------------------------------------------------

/// Rejoin hard-wrapped prose lines into flowing paragraphs.
///
/// The wrap width is auto-detected from the content.  Lines at that width
/// are joined with the following line (inserting one space) unless either
/// line is an "intentional break" — a bullet, heading, code block, table
/// row, blank line, or a line ending with structural punctuation.
fn unwrap_lines(text: &str) -> String {
    let trailing_newline = text.ends_with('\n');
    let lines: Vec<&str> = text.lines().collect();
    let width = detect_wrap_width(&lines);

    if width == 0 {
        return text.to_owned();
    }

    let mut out: Vec<String> = Vec::new();
    let mut idx = 0;
    let len = lines.len();

    while idx < len {
        let line = lines.get(idx).copied().unwrap_or_default();

        if should_join(line, lines.get(idx + 1).copied(), width) {
            // Start accumulating a joined paragraph.
            let mut paragraph = String::from(line.trim_end());
            idx += 1;
            while idx < len && should_join_continuation(lines.get(idx).copied(), width) {
                paragraph.push(' ');
                paragraph.push_str(lines.get(idx).copied().unwrap_or_default().trim());
                idx += 1;
            }
            // Append the final short tail of this paragraph (the last line
            // that didn't reach the wrap width).  Skip lines that are longer
            // than the wrap width — those are standalone, not tails.
            let tail = lines.get(idx).copied().unwrap_or_default();
            let tail_len = tail.chars().count();
            if idx < len && !is_intentional_break(tail) && tail_len < width {
                paragraph.push(' ');
                paragraph.push_str(tail.trim());
                idx += 1;
            }
            out.push(paragraph);
        } else {
            out.push(line.to_owned());
            idx += 1;
        }
    }

    let mut result = out.join("\n");
    if trailing_newline {
        result.push('\n');
    }
    result
}

/// Whether the current line should be joined with the next line.
fn should_join(line: &str, next: Option<&str>, width: usize) -> bool {
    let Some(next_line) = next else { return false };
    if is_intentional_break(line) || is_intentional_break(next_line) {
        return false;
    }
    let char_len = line.chars().count();
    // The line must be at or very near the wrap width (allow ±2 tolerance).
    char_len >= width.saturating_sub(2) && char_len <= width.saturating_add(2)
}

/// Whether a line is eligible to be appended to an in-progress paragraph.
fn should_join_continuation(line: Option<&str>, width: usize) -> bool {
    let Some(l) = line else { return false };
    if is_intentional_break(l) {
        return false;
    }
    let char_len = l.chars().count();
    char_len >= width.saturating_sub(2) && char_len <= width.saturating_add(2)
}

/// Detect the most likely hard-wrap column from a block of text.
///
/// Returns `0` when fewer than two candidate lines exist or no dominant
/// width can be identified — this suppresses unwrap entirely.
fn detect_wrap_width(lines: &[&str]) -> usize {
    use std::collections::HashMap;

    let mut freq: HashMap<usize, usize> = HashMap::new();

    for line in lines {
        if is_intentional_break(line) {
            continue;
        }
        let char_len = line.chars().count();
        if char_len > 0 {
            *freq.entry(char_len).or_insert(0) += 1;
        }
    }

    // Find the width with the highest frequency (prefer larger on tie).
    let winner = freq.iter().max_by(|a, b| a.1.cmp(b.1).then_with(|| a.0.cmp(b.0)));

    match winner {
        Some((&width, &count)) if count >= 2 => width,
        _ => 0,
    }
}

/// Return `true` for lines that should never be merged with adjacent lines.
fn is_intentional_break(line: &str) -> bool {
    if line.is_empty() {
        return true;
    }

    let trimmed = line.trim_end();

    // Structural ending punctuation.
    if ends_with_structural_punct(trimmed) {
        return true;
    }

    let stripped = line.trim_start();

    // Markdown heading.
    if stripped.starts_with('#') {
        return true;
    }

    // Table row.
    if stripped.starts_with('|') {
        return true;
    }

    // Code block fence.
    if stripped.starts_with("```") {
        return true;
    }

    // Unordered list marker: -, *, + followed by space.
    if starts_with_list_marker(stripped) {
        return true;
    }

    // Ordered list marker: digit(s) + '.' + space.
    if starts_with_ordered_marker(stripped) {
        return true;
    }

    // Indented code block (4+ leading spaces beyond current context).
    if has_code_indent(line) {
        return true;
    }

    false
}

/// Check for structural ending punctuation that signals an intentional break.
fn ends_with_structural_punct(line: &str) -> bool {
    let last = line.chars().last();
    matches!(last, Some(':' | '{' | '}'))
}

/// Check for unordered list markers: `- `, `* `, `+ `.
fn starts_with_list_marker(stripped: &str) -> bool {
    let mut chars = stripped.chars();
    let first = chars.next();
    let second = chars.next();
    matches!((first, second), (Some('-' | '*' | '+'), Some(' ')))
}

/// Check for ordered list markers: `1. `, `12. `, etc.
fn starts_with_ordered_marker(stripped: &str) -> bool {
    let dot_pos = stripped.find('.');
    let Some(pos) = dot_pos else { return false };
    if pos == 0 || pos > 4 {
        return false;
    }
    let prefix = &stripped[..pos];
    if !prefix.chars().all(|c| c.is_ascii_digit()) {
        return false;
    }
    // Must be followed by a space.
    stripped.get(pos + 1..pos + 2) == Some(" ")
}

/// Check for code-block indentation (4+ leading spaces or a tab).
fn has_code_indent(line: &str) -> bool {
    let mut spaces = 0;
    for c in line.chars() {
        match c {
            ' ' => {
                spaces += 1;
                if spaces >= 4 {
                    return true;
                }
            }
            '\t' => return true,
            _ => return false,
        }
    }
    false
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // ── prepare_copy_text ─────────────────────────────────────────────

    #[test]
    fn passthrough_when_claude_inactive() {
        let text = "    indented\n    lines\n";
        assert_eq!(prepare_copy_text(text, false, true), text);
    }

    #[test]
    fn passthrough_when_cleanup_disabled() {
        let text = "    indented\n    lines\n";
        assert_eq!(prepare_copy_text(text, true, false), text);
    }

    #[test]
    fn applies_transforms_when_enabled() {
        let text = "    hello world\n    goodbye world\n";
        let result = prepare_copy_text(text, true, true);
        assert_eq!(result, "hello world\ngoodbye world\n");
    }

    // ── dedent ────────────────────────────────────────────────────────

    #[test]
    fn dedent_no_indent() {
        assert_eq!(dedent("foo\nbar"), "foo\nbar");
    }

    #[test]
    fn dedent_uniform_indent() {
        assert_eq!(dedent("    foo\n    bar"), "foo\nbar");
    }

    #[test]
    fn dedent_mixed_indent_strips_minimum() {
        assert_eq!(dedent("  foo\n    bar"), "foo\n  bar");
    }

    #[test]
    fn dedent_empty_lines_ignored_for_minimum() {
        assert_eq!(dedent("    foo\n\n    bar"), "foo\n\nbar");
    }

    #[test]
    fn dedent_zero_min_is_noop() {
        let text = "no indent\n  some indent";
        assert_eq!(dedent(text), text);
    }

    #[test]
    fn dedent_preserves_trailing_newline() {
        assert_eq!(dedent("  a\n  b\n"), "a\nb\n");
    }

    #[test]
    fn dedent_no_trailing_newline_stays_without() {
        assert_eq!(dedent("  a\n  b"), "a\nb");
    }

    #[test]
    fn dedent_single_line() {
        assert_eq!(dedent("    hello"), "hello");
    }

    #[test]
    fn dedent_all_empty_lines() {
        assert_eq!(dedent("\n\n"), "\n\n");
    }

    #[test]
    fn dedent_empty_string() {
        assert_eq!(dedent(""), "");
    }

    // ── detect_wrap_width ─────────────────────────────────────────────

    #[test]
    fn detect_width_from_repeated_length() {
        // Three lines of 40 chars, one short tail.
        let line40 = "a".repeat(40);
        let lines = vec![line40.as_str(), line40.as_str(), line40.as_str(), "short"];
        assert_eq!(detect_wrap_width(&lines), 40);
    }

    #[test]
    fn detect_width_returns_zero_for_single_line() {
        let lines = vec!["only one line here"];
        assert_eq!(detect_wrap_width(&lines), 0);
    }

    #[test]
    fn detect_width_returns_zero_for_no_dominant() {
        let lines = vec!["short", "medium length", "a different size entirely"];
        assert_eq!(detect_wrap_width(&lines), 0);
    }

    #[test]
    fn detect_width_ignores_intentional_breaks() {
        let line30 = "x".repeat(30);
        let lines = vec![
            line30.as_str(),
            line30.as_str(),
            "",              // blank — intentional break
            "- bullet item", // list marker — intentional break
            "# heading",     // heading — intentional break
        ];
        assert_eq!(detect_wrap_width(&lines), 30);
    }

    #[test]
    fn detect_width_prefers_larger_on_tie() {
        // Two lines of length 40, two of length 50.
        let a = "a".repeat(40);
        let b = "b".repeat(50);
        let lines = vec![a.as_str(), a.as_str(), b.as_str(), b.as_str()];
        assert_eq!(detect_wrap_width(&lines), 50);
    }

    // ── unwrap_lines ──────────────────────────────────────────────────

    #[test]
    fn unwrap_joins_hard_wrapped_prose() {
        let w = 40;
        let line_a = format!("{:<width$}", "This is a line that wraps at", width = w);
        let line_b = format!("{:<width$}", "a fixed width and should be", width = w);
        let tail = "joined together.";
        let input = format!("{line_a}\n{line_b}\n{tail}");
        let result = unwrap_lines(&input);
        // Should be one flowing line.
        assert!(!result.contains('\n'), "expected single line, got:\n{result}");
        assert!(result.contains("wraps at"));
        assert!(result.contains("joined together."));
    }

    #[test]
    fn unwrap_preserves_blank_line_paragraph_separator() {
        let w = 40;
        let a1 = format!("{:<width$}", "First paragraph line one that", width = w);
        let a2 = "wraps here.";
        let b1 = format!("{:<width$}", "Second paragraph line one that", width = w);
        let b2 = "also wraps.";
        let input = format!("{a1}\n{a2}\n\n{b1}\n{b2}");
        let result = unwrap_lines(&input);
        // Blank line must survive.
        assert!(result.contains("\n\n"), "blank line separator lost:\n{result}");
    }

    #[test]
    fn unwrap_preserves_bullet_lists() {
        let w = 40;
        let line = format!("{:<width$}", "Some prose that wraps at exactly", width = w);
        let input = format!("{line}\n{line}\n- bullet one\n- bullet two");
        let result = unwrap_lines(&input);
        assert!(result.contains("\n- bullet one\n- bullet two"));
    }

    #[test]
    fn unwrap_preserves_ordered_lists() {
        let w = 40;
        let line = format!("{:<width$}", "Some prose that wraps at exactly", width = w);
        let input = format!("{line}\n{line}\n1. first\n2. second");
        let result = unwrap_lines(&input);
        assert!(result.contains("\n1. first\n2. second"));
    }

    #[test]
    fn unwrap_preserves_headings() {
        let w = 40;
        let line = format!("{:<width$}", "Some prose that wraps at exactly", width = w);
        let input = format!("# Title\n{line}\n{line}\ntail");
        let result = unwrap_lines(&input);
        assert!(result.starts_with("# Title\n"));
    }

    #[test]
    fn unwrap_preserves_code_blocks() {
        let w = 40;
        let line = format!("{:<width$}", "Some prose that wraps at exactly", width = w);
        let input = format!("{line}\n{line}\n```\ncode here\n```");
        let result = unwrap_lines(&input);
        assert!(result.contains("\n```\ncode here\n```"));
    }

    #[test]
    fn unwrap_preserves_table_rows() {
        let w = 40;
        let line = format!("{:<width$}", "Some prose that wraps at exactly", width = w);
        let input = format!("{line}\n{line}\n| col1 | col2 |");
        let result = unwrap_lines(&input);
        assert!(result.contains("\n| col1 | col2 |"));
    }

    #[test]
    fn unwrap_preserves_lines_ending_with_colon() {
        let w = 40;
        let line = format!("{:<width$}", "Some prose that wraps at exactly", width = w);
        let colon_line = "Here is a label:";
        let input = format!("{line}\n{line}\n{colon_line}");
        let result = unwrap_lines(&input);
        assert!(result.ends_with("\nHere is a label:"));
    }

    #[test]
    fn unwrap_preserves_indented_code() {
        let w = 40;
        let line = format!("{:<width$}", "Some prose that wraps at exactly", width = w);
        let input = format!("{line}\n{line}\n    fn main() {{}}\ntail");
        let result = unwrap_lines(&input);
        assert!(result.contains("\n    fn main() {}"));
    }

    #[test]
    fn unwrap_noop_when_no_dominant_width() {
        let input = "short\nmedium length\na different size line";
        assert_eq!(unwrap_lines(input), input);
    }

    #[test]
    fn unwrap_preserves_trailing_newline() {
        let w = 40;
        let line = format!("{:<width$}", "Some prose that wraps at exactly", width = w);
        let input = format!("{line}\n{line}\ntail\n");
        let result = unwrap_lines(&input);
        assert!(result.ends_with('\n'), "trailing newline lost");
    }

    #[test]
    fn unwrap_no_trailing_newline_stays_without() {
        let w = 40;
        let line = format!("{:<width$}", "Some prose that wraps at exactly", width = w);
        let input = format!("{line}\n{line}\ntail");
        let result = unwrap_lines(&input);
        assert!(!result.ends_with('\n'));
    }

    #[test]
    fn unwrap_does_not_consume_overlong_lines() {
        // A line much longer than the wrap width should not be merged
        // into the preceding paragraph.
        let w = 40;
        let line = format!("{:<width$}", "Some prose that wraps at exactly", width = w);
        let overlong = "x".repeat(80);
        let input = format!("{line}\n{line}\n{overlong}");
        let result = unwrap_lines(&input);
        // The overlong line must appear on its own.
        assert!(
            result.contains(&format!("\n{overlong}")),
            "overlong line was incorrectly merged:\n{result}"
        );
    }

    // ── is_intentional_break ──────────────────────────────────────────

    #[test]
    fn break_empty_line() {
        assert!(is_intentional_break(""));
    }

    #[test]
    fn break_bullet_dash() {
        assert!(is_intentional_break("- item"));
    }

    #[test]
    fn break_bullet_star() {
        assert!(is_intentional_break("* item"));
    }

    #[test]
    fn break_bullet_plus() {
        assert!(is_intentional_break("+ item"));
    }

    #[test]
    fn break_ordered_list() {
        assert!(is_intentional_break("1. first"));
        assert!(is_intentional_break("12. twelfth"));
    }

    #[test]
    fn break_heading() {
        assert!(is_intentional_break("# Title"));
        assert!(is_intentional_break("## Subtitle"));
    }

    #[test]
    fn break_table_row() {
        assert!(is_intentional_break("| a | b |"));
    }

    #[test]
    fn break_code_fence() {
        assert!(is_intentional_break("```rust"));
        assert!(is_intentional_break("```"));
    }

    #[test]
    fn break_colon_ending() {
        assert!(is_intentional_break("options:"));
    }

    #[test]
    fn break_brace_ending() {
        assert!(is_intentional_break("fn main() {"));
        assert!(is_intentional_break("}"));
    }

    #[test]
    fn break_indented_code() {
        assert!(is_intentional_break("    code line"));
        assert!(is_intentional_break("\tcode line"));
    }

    #[test]
    fn not_break_normal_prose() {
        assert!(!is_intentional_break("This is normal text"));
    }

    #[test]
    fn not_break_dash_without_space() {
        // A dash not followed by a space is not a list marker.
        assert!(!is_intentional_break("-nospace"));
    }

    #[test]
    fn not_break_number_without_dot_space() {
        assert!(!is_intentional_break("100 items"));
    }

    // ── end-to-end: dedent + unwrap together ──────────────────────────

    #[test]
    fn full_cleanup_claude_output() {
        // Simulates Claude Code output indented 4 spaces, wrapped at 76
        // chars (80 - 4 indent).
        let w = 76;
        let make_line = |s: &str| format!("    {s:<w$}");
        let input = [
            make_line("Claude Code often outputs prose with hard line breaks"),
            make_line("at a fixed width like a column of roughly eighty"),
            String::from("    characters wide."),
            String::new(),
            String::from("    - Bullet one"),
            String::from("    - Bullet two"),
        ]
        .join("\n");

        let result = prepare_copy_text(&input, true, true);

        // Prose should be unwrapped into one line.
        assert!(result.contains("prose with hard line breaks at a fixed width"));
        assert!(result.contains("characters wide."));
        // Bullets preserved on separate lines.
        assert!(result.contains("\n- Bullet one\n- Bullet two"));
        // No leading 4-space indent.
        assert!(!result.starts_with("    "));
    }

    #[test]
    fn full_cleanup_preserves_code_in_prose() {
        let w = 60;
        let make_line = |s: &str| format!("  {s:<w$}");
        let input = [
            make_line("Here is some prose that wraps at sixty characters"),
            make_line("and continues on the next line for a while."),
            String::from("  Followed by a short line."),
            String::new(),
            String::from("  ```"),
            String::from("  fn main() {}"),
            String::from("  ```"),
        ]
        .join("\n");

        let result = prepare_copy_text(&input, true, true);

        // Code block fences must survive as separate lines.
        assert!(result.contains("\n```\n"));
        assert!(result.contains("fn main() {}"));
    }
}
