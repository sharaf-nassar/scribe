use std::sync::Arc;

use scribe_common::terminal_images::ImageLimits;
use scribe_image_decode::{
    DecodeAdmissionError, DecodeCeilings, DecodePermit, DecodeRequest, DecodeScheduler,
    DecodeStorage, DecodeTarget, StorageProcess, StorageValidation,
};

/// Real paired limited ledgers for standalone decoder coverage.
pub fn decode_storage() -> Arc<DecodeStorage> {
    DecodeStorage::new(
        StorageProcess::new(256 * 1024 * 1024),
        128 * 1024 * 1024,
        0,
        StorageValidation::default(),
    )
}

/// Real scheduler-issued admission for standalone decoder coverage. A decoder
/// cannot be reached without one, so every probe admits through the production
/// scheduler rather than fabricating a permit.
pub fn decode_permit() -> Result<DecodePermit, DecodeAdmissionError> {
    let limits = ImageLimits::V1;
    let scheduler = DecodeScheduler::new(DecodeCeilings {
        concurrent_decodes: limits.max_concurrent_decodes,
        queue_depth: limits.max_decode_queue_depth,
        queue_bytes: limits.max_decode_queue_bytes,
        queue_wait: std::time::Duration::from_millis(limits.max_queue_wait_ms),
    });
    let session = scheduler.new_session();
    let request = DecodeRequest {
        session,
        generation: 1,
        target: DecodeTarget::kitty(0),
        requested_bytes: 0,
        storage: decode_storage(),
    };
    let ticket = scheduler.issue(request)?;
    scheduler.admit(ticket)
}
