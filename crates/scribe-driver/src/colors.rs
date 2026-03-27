//! Project color palette for driver task UI.

/// Fixed color palette for assigning colors to projects.
const COLORS: &[&str] =
    &["#7c3aed", "#2563eb", "#f43f5e", "#059669", "#d946ef", "#f59e0b", "#06b6d4", "#84cc16"];

/// Deterministically assign a color from the palette to a project path.
///
/// Uses a simple FNV-1a hash of the path bytes to pick an index.
pub fn project_color(path: &str) -> &'static str {
    let hash = fnv1a(path.as_bytes());
    let idx = (hash as usize) % COLORS.len();
    #[allow(clippy::indexing_slicing, reason = "idx is bounded by COLORS.len() via modulo")]
    COLORS[idx]
}

/// FNV-1a 32-bit hash.
fn fnv1a(data: &[u8]) -> u32 {
    let mut hash: u32 = 2_166_136_261;
    for &byte in data {
        hash ^= u32::from(byte);
        hash = hash.wrapping_mul(16_777_619);
    }
    hash
}
