//! Proves that GPUI's Linux text backend honors caller-supplied fallback order.

use std::{error::Error, process::ExitCode};

use gpui::{FontFallbacks, TextStyle};

const ICON: &str = "\u{e0b0}";
const PRIMARY: &str = "Nimbus Mono PS";
const NERD_FONT: &str = "Symbols Nerd Font Mono";
const GENERIC_SYMBOL_FONT: &str = "Unifont Sample";

fn main() -> ExitCode {
    match verify_nerd_font_precedes_generic_symbols() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("GPUI fallback-order spike failed: {error}");
            ExitCode::FAILURE
        }
    }
}

fn verify_nerd_font_precedes_generic_symbols() -> Result<(), Box<dyn Error>> {
    let expected_fallbacks = vec![NERD_FONT.to_owned(), GENERIC_SYMBOL_FONT.to_owned()];
    let text_style = TextStyle {
        font_family: PRIMARY.into(),
        font_fallbacks: Some(FontFallbacks::from_fonts(expected_fallbacks.clone())),
        ..Default::default()
    };
    let run = text_style.to_run(ICON.len());
    let selected_fallbacks = run.font.fallbacks.ok_or("GPUI dropped the per-run fallback list")?;

    if selected_fallbacks.fallback_list() == expected_fallbacks {
        println!("PASS: {ICON:?} carries {NERD_FONT:?} before {GENERIC_SYMBOL_FONT:?}.");
        return Ok(());
    }

    Err(format!("GPUI changed the requested fallback order: {selected_fallbacks:?}").into())
}
