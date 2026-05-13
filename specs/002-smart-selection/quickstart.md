# Quickstart: Smart Selection

## Read First

1. `specs/002-smart-selection/spec.md`
2. `specs/002-smart-selection/plan.md`
3. `specs/002-smart-selection/research.md`
4. `specs/002-smart-selection/data-model.md`
5. `specs/002-smart-selection/contracts/config-smart-selection.md`
6. `specs/002-smart-selection/contracts/settings-ui-smart-selection.md`

## Implementation Outline

1. Add Smart Selection config types and defaults in `crates/scribe-common/src/config.rs`.
2. Add settings apply support for `terminal.smart_selection` and reset-to-defaults in `crates/scribe-settings/src/apply.rs`.
3. Add the Terminal page Smart Selection section in `settings.html`, `settings.js`, and `settings.css`.
4. Add a `smart_selection` module in `crates/scribe-client/src/` for compiled rules, logical text collection, candidate scoring, and parameter expansion.
5. Extend mouse click classification to recognize quad click.
6. Route double-click or quad-click dispatch to Smart Selection based on config.
7. Extend context-menu construction so matching rules contribute explicit actions.
8. Update `lat.md/` after behavior and architecture are implemented.

## Manual Verification Scenarios

Use a terminal pane containing these examples:

```text
https://example.com/path?q=1
/tmp/build/output.log
"quoted value with spaces"
foo.bar.baz
namespace::identifier
@selector(foo:bar:)
user@example.com
plainword
```

Verify:
- With activation set to `quad_click`, double-click still selects ordinary words and quad-click selects Smart Selection matches.
- With activation set to `double_click`, double-click selects Smart Selection matches.
- Rule precision chooses URL/path/email matches over generic words.
- Invalid custom regex shows an error in settings and does not break valid rules.
- Copy-on-select still copies Smart Selection text when enabled.
- Context menu shows configured rule actions over matching text and does not show them over unrelated text.
- Command-like actions do not run from selection alone.

## Suggested Commands

```bash
cargo test -p scribe-common smart_selection
cargo test -p scribe-settings smart_selection
cargo test -p scribe-client smart_selection
lat check
```

Use narrower filters while developing if needed, but finish with the relevant package checks and `lat check`.
