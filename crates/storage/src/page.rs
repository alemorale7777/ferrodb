//! The fundamental unit of storage: a fixed-size page with a CRC32C trailer.

/// Size of a page on disk, in bytes. The single source of truth for page geometry.
pub const PAGE_SIZE: usize = 4096;

/// Usable bytes in a page. The last 4 bytes hold the CRC32C checksum trailer.
pub const PAGE_DATA_SIZE: usize = PAGE_SIZE - 4;

/// A page identifier: the page's index within the data file. `PageId(0)` is the meta page.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct PageId(pub u32);

/// A page: `PAGE_SIZE` raw bytes whose last 4 bytes are a checksum over the rest.
#[derive(Clone)]
pub struct Page {
    bytes: [u8; PAGE_SIZE],
}

impl Page {
    pub fn new_zeroed() -> Self {
        Page {
            bytes: [0u8; PAGE_SIZE],
        }
    }

    pub fn from_bytes(bytes: [u8; PAGE_SIZE]) -> Self {
        Page { bytes }
    }

    pub fn as_bytes(&self) -> &[u8; PAGE_SIZE] {
        &self.bytes
    }

    /// The usable data region (excludes the checksum trailer).
    pub fn data(&self) -> &[u8] {
        &self.bytes[..PAGE_DATA_SIZE]
    }

    /// Mutable view of the usable data region (excludes the checksum trailer).
    pub fn data_mut(&mut self) -> &mut [u8] {
        &mut self.bytes[..PAGE_DATA_SIZE]
    }

    /// Recompute and store the CRC32C of the data region into the trailer.
    pub fn compute_checksum(&mut self) {
        let sum = crc32fast::hash(&self.bytes[..PAGE_DATA_SIZE]);
        self.bytes[PAGE_DATA_SIZE..].copy_from_slice(&sum.to_le_bytes());
    }

    /// Returns true if the stored trailer matches the CRC32C of the data region.
    pub fn verify_checksum(&self) -> bool {
        let stored = u32::from_le_bytes(self.bytes[PAGE_DATA_SIZE..].try_into().unwrap());
        stored == crc32fast::hash(&self.bytes[..PAGE_DATA_SIZE])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checksum_roundtrip_detects_corruption() {
        let mut p = Page::new_zeroed();
        p.data_mut()[0..3].copy_from_slice(b"abc");
        p.compute_checksum();
        assert!(p.verify_checksum());
        p.data_mut()[1] = b'X';
        assert!(!p.verify_checksum());
    }

    #[test]
    fn data_excludes_checksum_trailer() {
        assert_eq!(Page::new_zeroed().data().len(), PAGE_DATA_SIZE);
        assert_eq!(PAGE_DATA_SIZE, PAGE_SIZE - 4);
    }
}
