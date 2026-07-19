use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Extent {
    pub offset: u64,
    pub len: u64,
}

#[derive(Debug)]
pub struct ExtentScheduler {
    total_size: u64,
    extent_size: u64,
    next_offset: AtomicU64,
}

impl ExtentScheduler {
    pub fn new(total_size: u64, extent_size: u64) -> Result<Self, SchedulerError> {
        if total_size == 0 {
            return Err(SchedulerError::EmptyTransfer);
        }
        if extent_size == 0 {
            return Err(SchedulerError::EmptyExtent);
        }
        Ok(Self {
            total_size,
            extent_size,
            next_offset: AtomicU64::new(0),
        })
    }

    pub fn next(&self) -> Option<Extent> {
        let offset = self
            .next_offset
            .fetch_add(self.extent_size, Ordering::Relaxed);
        if offset >= self.total_size {
            return None;
        }
        Some(Extent {
            offset,
            len: self.extent_size.min(self.total_size - offset),
        })
    }

    pub const fn total_size(&self) -> u64 {
        self.total_size
    }

    pub const fn extent_size(&self) -> u64 {
        self.extent_size
    }
}

#[derive(Debug, thiserror::Error, Eq, PartialEq)]
pub enum SchedulerError {
    #[error("transfer size must be greater than zero")]
    EmptyTransfer,
    #[error("extent size must be greater than zero")]
    EmptyExtent,
}

#[cfg(test)]
mod tests {
    use super::{Extent, ExtentScheduler};

    #[test]
    fn schedules_complete_non_overlapping_extents() {
        let scheduler = ExtentScheduler::new(150, 64).unwrap();
        assert_eq!(scheduler.next(), Some(Extent { offset: 0, len: 64 }));
        assert_eq!(
            scheduler.next(),
            Some(Extent {
                offset: 64,
                len: 64
            })
        );
        assert_eq!(
            scheduler.next(),
            Some(Extent {
                offset: 128,
                len: 22
            })
        );
        assert_eq!(scheduler.next(), None);
    }
}
