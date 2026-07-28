//! Drag-and-drop file-path insertion with shell-aware quoting.
//!
//! When the compositor delivers a dropped file to a pane, the client inserts
//! the file's path into the running shell as if it had been typed. The path is
//! quoted for the pane's shell so spaces, quotes, and other metacharacters
//! survive, then a trailing space is appended so the next argument the user
//! types is separated from the path.
//!
//! Ported byte-for-byte from the legacy winit client's `handle_dropped_path`
//! and `quote_path_for_shell` helpers so drop insertion produces identical
//! bytes across the GPUI cutover. Drag-and-drop insertion is intentionally
//! outside the spec-011 paste confirmation gate (FR-013): the path is already
//! shell-quoted, so callers deliver it directly without a confirmation prompt.

use std::path::Path;

/// Quote a dropped file path for insertion into `shell_name` and append the
/// trailing separator space.
///
/// This is the full drop-insertion payload the caller writes to the pane; it
/// wraps [`quote_path_for_shell`] and adds the trailing space so the shell
/// treats the path as a complete, separated argument.
#[must_use]
pub fn dropped_path_insertion(path: &Path, shell_name: &str) -> String {
    format!("{} ", quote_path_for_shell(path, shell_name))
}

/// Quote a filesystem path for the named shell.
///
/// `shell_name` is the pane's reported shell (`fish`, `pwsh`/`powershell`,
/// `nu`, or anything else which falls back to POSIX `sh`/`bash` quoting).
#[must_use]
pub fn quote_path_for_shell(path: &Path, shell_name: &str) -> String {
    let text = path.to_string_lossy();
    match shell_name {
        "fish" => quote_fish_string(text.as_ref()),
        "pwsh" | "powershell" => quote_powershell_string(text.as_ref()),
        "nu" => quote_nushell_string(text.as_ref()),
        _ => quote_posix_string(text.as_ref()),
    }
}

/// POSIX single-quote: wrap in `'…'` and rewrite embedded quotes as `'"'"'`.
#[must_use]
pub fn quote_posix_string(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

/// Fish single-quote: only backslash and single-quote are special inside
/// `'…'`, so escape those with a backslash.
#[must_use]
pub fn quote_fish_string(value: &str) -> String {
    let escaped = value.replace('\\', "\\\\").replace('\'', "\\'");
    format!("'{escaped}'")
}

/// PowerShell single-quote: doubling `'` is the only escape needed inside
/// `'…'` (no backslash processing).
#[must_use]
pub fn quote_powershell_string(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

/// Nushell quoting: prefer a raw string when a single quote is present,
/// widening the `#` fence until it is unambiguous; fall back to a
/// backslash-escaped double-quoted string only when every raw fence collides.
#[must_use]
pub fn quote_nushell_string(value: &str) -> String {
    if !value.contains('\'') {
        return format!("'{value}'");
    }

    for hashes in 1..=8 {
        let marker = "#".repeat(hashes);
        let closing = format!("'{marker}");
        if !value.contains(&closing) {
            return format!("r{marker}'{value}'{marker}");
        }
    }

    let escaped = value.replace('\\', "\\\\").replace('\"', "\\\"");
    format!("\"{escaped}\"")
}

#[cfg(test)]
mod tests {
    use super::*;

    // @lat: [[test#Drag-drop path insertion#POSIX quoting escapes single quotes]]
    #[test]
    fn posix_quotes_embedded_single_quote() {
        assert_eq!(quote_posix_string("a'b"), "'a'\"'\"'b'");
        assert_eq!(quote_posix_string("/tmp/plain"), "'/tmp/plain'");
    }

    // @lat: [[test#Drag-drop path insertion#Fish quoting escapes backslash and quote]]
    #[test]
    fn fish_escapes_backslash_and_quote() {
        assert_eq!(quote_fish_string("a\\b"), "'a\\\\b'");
        assert_eq!(quote_fish_string("a'b"), "'a\\'b'");
    }

    // @lat: [[test#Drag-drop path insertion#PowerShell quoting doubles single quotes]]
    #[test]
    fn powershell_doubles_single_quote() {
        assert_eq!(quote_powershell_string("a'b"), "'a''b'");
    }

    // @lat: [[test#Drag-drop path insertion#Nushell raw-string fencing]]
    #[test]
    fn nushell_uses_raw_string_when_quote_present() {
        assert_eq!(quote_nushell_string("plain"), "'plain'");
        assert_eq!(quote_nushell_string("a'b"), "r#'a'b'#");
        // A path that closes the one-hash fence widens to two hashes.
        assert_eq!(quote_nushell_string("a'#b"), "r##'a'#b'##");
    }

    // @lat: [[test#Drag-drop path insertion#Shell dispatch selects quoter]]
    #[test]
    fn quote_path_for_shell_dispatches_by_shell() {
        let path = Path::new("/tmp/a b");
        assert_eq!(quote_path_for_shell(path, "bash"), "'/tmp/a b'");
        assert_eq!(quote_path_for_shell(path, "fish"), "'/tmp/a b'");
        assert_eq!(quote_path_for_shell(path, "pwsh"), "'/tmp/a b'");
        assert_eq!(quote_path_for_shell(path, "nu"), "'/tmp/a b'");
    }

    // @lat: [[test#Drag-drop path insertion#Insertion appends trailing space]]
    #[test]
    fn insertion_appends_trailing_space() {
        assert_eq!(dropped_path_insertion(Path::new("/tmp/x"), "bash"), "'/tmp/x' ");
    }
}
