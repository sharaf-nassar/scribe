//! Unit coverage for the [`SCRIBE_JOIN_WINDOW`](super::JOIN_WINDOW_ENV) parser.
//!
//! The env read itself is untestable in-process (the workspace lints ban
//! `set_var`), so the value parser is exercised directly: a full UUID joins that
//! window, and every unusable spelling degrades to "claim your own window"
//! rather than aborting the launch.

use scribe_common::ids::WindowId;

use super::parse_join_window;

// @lat: [[test#GPUI Client Headless Suites#GPUI local share join]]
#[test]
fn parses_a_full_window_uuid_and_ignores_unusable_values() {
    let window_id = WindowId::new();
    assert_eq!(parse_join_window(&window_id.to_full_string()), Some(window_id));
    assert_eq!(parse_join_window(&format!("  {}  ", window_id.to_full_string())), Some(window_id));

    assert_eq!(parse_join_window(""), None);
    assert_eq!(parse_join_window("   "), None);
    assert_eq!(parse_join_window("not-a-uuid"), None);
    // The short `Display` form (`win-1234abcd`) is a label, not a parsable id.
    assert_eq!(parse_join_window(&window_id.to_string()), None);
}
