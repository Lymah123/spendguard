//! Persistence: payment records, the state log, and budget reservation.

pub mod budget;
pub mod store;

pub use store::{LogEntry, PaymentRecord, PaymentStore};

/// Current unix time in seconds. Only the service layer calls this; everything
/// below it takes `now` as an argument so tests control the clock.
pub fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
