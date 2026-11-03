//! The size a tab mirror opens at, remembered from the last user resize.
//!
//! Only the size is kept. A mirror always opens centred over the window that
//! opened it, so a remembered position would pull it away from its tab.
//! The value lives in `mirror_state.toml` under the flavor's state root, next
//! to `settings_state.toml`, and in memory for every window of this process.

use std::path::PathBuf;
use std::sync::{LazyLock, Mutex};

use gpui::{Pixels, Size, px, size};
use scribe_client::window_state::logical_px_to_i32;
use scribe_common::app::current_state_dir;

/// Sides outside this range, in logical pixels, are a corrupt or hand-edited
/// file and are ignored rather than opened.
const SIDE_RANGE: std::ops::RangeInclusive<u32> = 100..=16_384;

/// A mirror's outer size in whole logical pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MirrorSize {
    pub width: u32,
    pub height: u32,
}

impl MirrorSize {
    /// `size` rounded to whole pixels, or `None` when either side is outside
    /// [`SIDE_RANGE`].
    pub fn of(extent: Size<Pixels>) -> Option<Self> {
        let whole = |value: Pixels| u32::try_from(logical_px_to_i32(f32::from(value))).ok();
        Some(Self { width: whole(extent.width)?, height: whole(extent.height)? })
            .filter(|size| size.is_sane())
    }

    fn is_sane(self) -> bool {
        SIDE_RANGE.contains(&self.width) && SIDE_RANGE.contains(&self.height)
    }

    fn to_size(self) -> Size<Pixels> {
        let side = |value: u32| px(f32::from(u16::try_from(value).unwrap_or(u16::MAX)));
        size(side(self.width), side(self.height))
    }
}

struct Remembered {
    size: Option<MirrorSize>,
    /// Whether `size` changed since it was last written.
    unsaved: bool,
}

static REMEMBERED: LazyLock<Mutex<Remembered>> =
    LazyLock::new(|| Mutex::new(Remembered { size: load(), unsaved: false }));

/// The size a user last resized a mirror to, if any.
pub fn remembered() -> Option<Size<Pixels>> {
    REMEMBERED.lock().ok()?.size.map(MirrorSize::to_size)
}

/// Record a user resize. Returns whether the caller should schedule a
/// [`flush`]: true only when the value just went from saved to unsaved, so a
/// drag-resize schedules one write at a time rather than one per frame.
pub fn remember(size: MirrorSize) -> bool {
    let Ok(mut remembered) = REMEMBERED.lock() else { return false };
    if remembered.size == Some(size) {
        return false;
    }
    remembered.size = Some(size);
    !std::mem::replace(&mut remembered.unsaved, true)
}

/// Write the remembered size if it changed since the last write.
pub fn flush() {
    let size = {
        let Ok(mut remembered) = REMEMBERED.lock() else { return };
        if !std::mem::take(&mut remembered.unsaved) {
            return;
        }
        remembered.size
    };
    if let Some(size) = size {
        save(size);
    }
}

fn state_path() -> Option<PathBuf> {
    current_state_dir().map(|dir| dir.join("mirror_state.toml"))
}

fn load() -> Option<MirrorSize> {
    let path = state_path()?;
    match std::fs::read_to_string(&path) {
        Ok(text) => {
            let parsed = parse(&text);
            if parsed.is_none() {
                tracing::warn!(path = %path.display(), "ignoring an unreadable tab mirror size");
            }
            parsed
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "failed to read the tab mirror size");
            None
        }
    }
}

fn parse(text: &str) -> Option<MirrorSize> {
    toml::from_str::<MirrorSize>(text).ok().filter(|size| size.is_sane())
}

fn save(size: MirrorSize) {
    let Some(path) = state_path() else {
        tracing::warn!("no state directory; the tab mirror size is not saved");
        return;
    };
    if let Some(parent) = path.parent()
        && let Err(error) = std::fs::create_dir_all(parent)
    {
        tracing::warn!(path = %parent.display(), %error, "failed to create the state directory");
        return;
    }
    let written = toml::to_string(&size)
        .map_err(|error| error.to_string())
        .and_then(|text| std::fs::write(&path, text).map_err(|error| error.to_string()));
    match written {
        Ok(()) => {
            tracing::info!(width = size.width, height = size.height, "saved the tab mirror size");
        }
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "failed to save the tab mirror size");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // @lat: [[test#GPUI Client Headless Suites#Tab mirror size#A saved size is validated]]
    #[test]
    fn a_saved_mirror_size_is_validated() {
        let saved = MirrorSize { width: 640, height: 400 };
        let text = toml::to_string(&saved).unwrap();

        assert_eq!(parse(&text), Some(saved), "a written size reads back");
        assert_eq!(parse("width = 20\nheight = 400\n"), None, "a side too small is ignored");
        assert_eq!(parse("width = 640\nheight = 99999\n"), None, "a side too large is ignored");
        assert_eq!(parse("not toml"), None, "garbage is ignored");
        assert_eq!(MirrorSize::of(size(px(640.4), px(399.6))), Some(saved), "sizes round");
    }
}
