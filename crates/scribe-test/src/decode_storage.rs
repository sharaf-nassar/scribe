use std::sync::Arc;

use scribe_image_decode::{DecodeStorage, StorageProcess, StorageValidation};

/// Real paired limited ledgers for standalone decoder coverage.
pub fn decode_storage() -> Arc<DecodeStorage> {
    DecodeStorage::new(
        StorageProcess::new(256 * 1024 * 1024),
        128 * 1024 * 1024,
        0,
        StorageValidation::default(),
    )
}
