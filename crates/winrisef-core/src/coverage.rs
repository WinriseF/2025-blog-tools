use std::sync::{
    Mutex,
    atomic::{AtomicU64, Ordering},
};

use crate::ExtentHeader;

#[derive(Debug)]
pub struct CoverageTracker {
    total_size: u64,
    extent_size: u64,
    seen: Mutex<Vec<bool>>,
    received_bytes: AtomicU64,
}

impl CoverageTracker {
    pub fn new(total_size: u64, extent_size: u64) -> Result<Self, CoverageError> {
        if total_size == 0 || extent_size == 0 {
            return Err(CoverageError::InvalidShape);
        }
        let count = total_size.div_ceil(extent_size);
        let count = usize::try_from(count).map_err(|_| CoverageError::TooManyExtents)?;
        Ok(Self {
            total_size,
            extent_size,
            seen: Mutex::new(vec![false; count]),
            received_bytes: AtomicU64::new(0),
        })
    }

    pub fn record(&self, header: ExtentHeader) -> Result<(), CoverageError> {
        if !header.offset.is_multiple_of(self.extent_size) || header.len == 0 {
            return Err(CoverageError::InvalidExtent);
        }
        let end = header
            .offset
            .checked_add(header.len)
            .ok_or(CoverageError::OutOfRange)?;
        if end > self.total_size {
            return Err(CoverageError::OutOfRange);
        }
        let expected_len = self.extent_size.min(self.total_size - header.offset);
        if header.len != expected_len {
            return Err(CoverageError::InvalidExtent);
        }
        let index = usize::try_from(header.offset / self.extent_size)
            .map_err(|_| CoverageError::TooManyExtents)?;
        let mut seen = self.seen.lock().map_err(|_| CoverageError::Poisoned)?;
        let slot = seen.get_mut(index).ok_or(CoverageError::OutOfRange)?;
        if *slot {
            return Err(CoverageError::DuplicateExtent(header.offset));
        }
        *slot = true;
        self.received_bytes.fetch_add(header.len, Ordering::Relaxed);
        Ok(())
    }

    pub fn is_complete(&self) -> Result<bool, CoverageError> {
        let seen = self.seen.lock().map_err(|_| CoverageError::Poisoned)?;
        Ok(
            self.received_bytes.load(Ordering::Relaxed) == self.total_size
                && seen.iter().all(|value| *value),
        )
    }

    pub fn received_bytes(&self) -> u64 {
        self.received_bytes.load(Ordering::Relaxed)
    }
}

#[derive(Debug, thiserror::Error, Eq, PartialEq)]
pub enum CoverageError {
    #[error("total size and extent size must be greater than zero")]
    InvalidShape,
    #[error("extent is unaligned or has the wrong length")]
    InvalidExtent,
    #[error("extent lies outside the declared transfer")]
    OutOfRange,
    #[error("extent at offset {0} was received more than once")]
    DuplicateExtent(u64),
    #[error("extent count does not fit this platform")]
    TooManyExtents,
    #[error("coverage mutex is poisoned")]
    Poisoned,
}

#[cfg(test)]
mod tests {
    use crate::{CoverageTracker, ExtentHeader};

    #[test]
    fn requires_exact_complete_coverage() {
        let coverage = CoverageTracker::new(150, 64).unwrap();
        coverage
            .record(ExtentHeader {
                offset: 64,
                len: 64,
            })
            .unwrap();
        coverage
            .record(ExtentHeader {
                offset: 128,
                len: 22,
            })
            .unwrap();
        assert!(!coverage.is_complete().unwrap());
        coverage
            .record(ExtentHeader { offset: 0, len: 64 })
            .unwrap();
        assert!(coverage.is_complete().unwrap());
    }
}
