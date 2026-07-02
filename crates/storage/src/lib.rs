//! ferrodb storage engine (Milestone 1).
//!
//! A page-based, buffer-pooled, crash-safe-ready B+-tree key/value store built
//! bottom-up from a raw file: [`disk`] → [`buffer`] → [`slotted`] → [`btree`].
//! No third-party storage/btree/serialization crate is used — that is the point.

pub mod disk;
pub mod encoding;
pub mod freelist;
pub mod meta;
pub mod page;

use thiserror::Error;

/// Every fallible storage operation returns this error.
#[derive(Debug, Error)]
pub enum StorageError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("page {0} out of range")]
    PageOutOfRange(u32),
    #[error("checksum mismatch on page {0}")]
    BadChecksum(u32),
    #[error("page full")]
    PageFull,
    #[error("corrupt: {0}")]
    Corrupt(&'static str),
}

pub type Result<T> = std::result::Result<T, StorageError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_displays() {
        assert_eq!(
            StorageError::PageOutOfRange(7).to_string(),
            "page 7 out of range"
        );
    }
}
