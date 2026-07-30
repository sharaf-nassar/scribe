use vte::Perform;

use crate::metadata::{MetadataEvent, MetadataParser};

/// VTE [`Perform`] adapter that extracts OSC metadata from a raw byte stream.
///
/// This runs in parallel with `alacritty_terminal`'s own VTE parser because
/// `alacritty_terminal` ignores our custom OSC 1337 `AiState` extension.
/// By feeding the same bytes through an `OscInterceptor` we can extract:
///
/// - OSC 7 — current working directory
/// - OSC 133 — shell-integration prompt marks
/// - OSC 1337 `ScribeContext` / `ScribeAiLaunch` — shell context, AI pre-arm
///
/// OSC 0 / 2 (window title) is deliberately absent: `alacritty_terminal`
/// parses it natively and surfaces it as `Event::Title` / `Event::ResetTitle`,
/// joining the semicolon-separated params back into one title in the process.
/// Recognising it here too emitted a second, differently-truncated
/// `MetadataEvent::TitleChanged` for every title sequence.
///
/// Create one per read-loop iteration, advance it with `vte::Parser::advance`,
/// then inspect the `events` vec passed in at construction time.
pub struct OscInterceptor<'a> {
    events: &'a mut Vec<MetadataEvent>,
}

impl<'a> OscInterceptor<'a> {
    /// Create a new interceptor that pushes events into `events`.
    ///
    /// The caller owns the `Vec` and is responsible for clearing it between
    /// iterations, avoiding a heap allocation per read.
    #[must_use]
    pub fn new(events: &'a mut Vec<MetadataEvent>) -> Self {
        Self { events }
    }
}

impl Perform for OscInterceptor<'_> {
    /// Called for every OSC sequence. Delegates to [`MetadataParser::process_osc`].
    fn osc_dispatch(&mut self, params: &[&[u8]], _bell_terminated: bool) {
        if let Some(event) = MetadataParser::process_osc(params) {
            self.events.push(event);
        }
    }

    // All remaining Perform methods are intentional no-ops: we only care about
    // OSC sequences, not printable characters, control bytes or CSI/DCS/ESC.
    //
    // C0 control bytes included: BEL (0x07) is dispatched by
    // `alacritty_terminal`'s parser as `Event::Bell`, and this interceptor sees
    // the same byte stream, so recognising it here too would emit a second
    // `MetadataEvent::Bell` for every bell.

    fn execute(&mut self, _byte: u8) {}

    fn print(&mut self, _c: char) {}

    fn hook(&mut self, _params: &vte::Params, _intermediates: &[u8], _ignore: bool, _action: char) {
    }

    fn put(&mut self, _byte: u8) {}

    fn unhook(&mut self) {}

    fn csi_dispatch(
        &mut self,
        _params: &vte::Params,
        _intermediates: &[u8],
        _ignore: bool,
        _action: char,
    ) {
    }

    fn esc_dispatch(&mut self, _intermediates: &[u8], _ignore: bool, _byte: u8) {}
}
