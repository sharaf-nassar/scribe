# `gpui-component` adoption evaluation

**Decision: decline for the current client rebuild.** Reconsider only when a
new, isolated surface has a concrete component need and can be integrated and
validated against Scribe's pinned GPUI revision without changing established
chrome behaviour.

## Evidence

The rebuild plan already makes this v1 decision explicitly: its Chrome section
states that the custom titlebar, integrated tab bar, status bar, command-mark
scrollbar, dividers, dialogs, palette, tooltips, AI indicator, and prompt bar
are GPUI elements/views, and says no `gpui-component` dependency for v1 because
the widgets are bespoke and fewer moving parts are preferable. This evaluation
keeps that decision rather than reopening it without a new product need.

The current client has bespoke implementations at
`crates/scribe-client/src/titlebar.rs`, `tab_bar.rs`, `status_bar.rs`,
`scrollbar.rs`, `dialog.rs`, `command_palette.rs`, `tooltip.rs`, and
`prompt_bar.rs`. These surfaces draw from Scribe's layout, interaction, and
theme data; they are not unowned generic controls waiting to be replaced.

`Cargo.toml` has no `gpui-component` dependency. Its GPUI crates are pinned to
Zed revision `f96212f2c50f54d93712fa130d6226b1ce7d76b5`. Adding a component
crate would therefore first require confirming compatibility with that exact
revision. It would also need an integration design for the application root,
event/focus ownership, and Scribe's resolved theme/chrome colours. Those are
integration risks to validate, not claims that the crate cannot work here.

## Candidate future uses

| Candidate | Expected migration cost | Decision now |
| --- | --- | --- |
| Settings-only forms | Moderate: map existing settings state, validation, and Scribe theme into a component boundary. | Defer; consider only for a new, isolated Settings flow. |
| Dialog buttons and simple menus | Low to moderate: preserve modal/menu ownership, keyboard behaviour, and theme styling. | Retain bespoke implementations; revisit for a newly introduced simple dialog or menu. |
| Tooltip or hover card | Low to moderate: preserve anchor placement, transient state, and current tooltip styling. | Retain bespoke tooltip; revisit for a new isolated hover card. |
| Generic icon buttons | Low per control, plus shared styling and focus integration work. | Do not introduce a dependency for existing chrome; reconsider if repeated non-chrome controls create a proven shared need. |

## Revisit criteria

Re-evaluate before adoption only when all of these are true:

- a new isolated surface would otherwise duplicate a stable generic control;
- the candidate version is compatible with GPUI revision
  `f96212f2c50f54d93712fa130d6226b1ce7d76b5`, or an intentional GPUI upgrade
  is separately approved;
- a small integration spike demonstrates root, focus/event, and theme mapping;
- the change preserves the behaviour and visual contracts of the affected
  Scribe surface.

Until then, retain bespoke chrome and avoid adding `gpui-component` merely as a
potential future abstraction.
