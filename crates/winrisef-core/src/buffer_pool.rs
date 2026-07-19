use std::{
    ops::{Deref, DerefMut},
    sync::{Arc, Mutex},
};

#[derive(Clone, Debug)]
pub struct BufferPool {
    inner: Arc<PoolInner>,
}

#[derive(Debug)]
struct PoolInner {
    block_size: usize,
    available: Mutex<Vec<Vec<u8>>>,
}

#[derive(Debug)]
pub struct PooledBuffer {
    inner: Arc<PoolInner>,
    bytes: Option<Vec<u8>>,
    logical_len: usize,
}

impl BufferPool {
    pub fn new(buffer_count: usize, block_size: usize) -> Result<Self, BufferPoolError> {
        if buffer_count == 0 {
            return Err(BufferPoolError::EmptyPool);
        }
        if block_size == 0 {
            return Err(BufferPoolError::EmptyBuffer);
        }
        let mut available = Vec::with_capacity(buffer_count);
        for _ in 0..buffer_count {
            available.push(vec![0; block_size]);
        }
        Ok(Self {
            inner: Arc::new(PoolInner {
                block_size,
                available: Mutex::new(available),
            }),
        })
    }

    pub fn acquire(&self) -> Result<PooledBuffer, BufferPoolError> {
        let bytes = self
            .inner
            .available
            .lock()
            .map_err(|_| BufferPoolError::Poisoned)?
            .pop()
            .ok_or(BufferPoolError::Exhausted)?;
        Ok(PooledBuffer {
            inner: Arc::clone(&self.inner),
            bytes: Some(bytes),
            logical_len: 0,
        })
    }

    pub fn block_size(&self) -> usize {
        self.inner.block_size
    }

    pub fn available_count(&self) -> Result<usize, BufferPoolError> {
        Ok(self
            .inner
            .available
            .lock()
            .map_err(|_| BufferPoolError::Poisoned)?
            .len())
    }
}

impl PooledBuffer {
    pub fn set_len(&mut self, logical_len: usize) -> Result<(), BufferPoolError> {
        if logical_len > self.inner.block_size {
            return Err(BufferPoolError::LengthOutOfRange {
                requested: logical_len,
                capacity: self.inner.block_size,
            });
        }
        self.logical_len = logical_len;
        Ok(())
    }

    pub const fn len(&self) -> usize {
        self.logical_len
    }

    pub const fn is_empty(&self) -> bool {
        self.logical_len == 0
    }

    pub fn full_mut(&mut self) -> &mut [u8] {
        self.bytes
            .as_mut()
            .expect("pooled buffer is present until drop")
    }
}

impl Deref for PooledBuffer {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        &self
            .bytes
            .as_ref()
            .expect("pooled buffer is present until drop")[..self.logical_len]
    }
}

impl DerefMut for PooledBuffer {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self
            .bytes
            .as_mut()
            .expect("pooled buffer is present until drop")[..self.logical_len]
    }
}

impl Drop for PooledBuffer {
    fn drop(&mut self) {
        let Some(bytes) = self.bytes.take() else {
            return;
        };
        if let Ok(mut available) = self.inner.available.lock() {
            available.push(bytes);
        }
    }
}

#[derive(Debug, thiserror::Error, Eq, PartialEq)]
pub enum BufferPoolError {
    #[error("buffer pool must contain at least one buffer")]
    EmptyPool,
    #[error("buffer size must be greater than zero")]
    EmptyBuffer,
    #[error("buffer pool exhausted; an in-flight bound was violated")]
    Exhausted,
    #[error("buffer pool mutex is poisoned")]
    Poisoned,
    #[error("logical buffer length {requested} exceeds capacity {capacity}")]
    LengthOutOfRange { requested: usize, capacity: usize },
}

#[cfg(test)]
mod tests {
    use super::{BufferPool, BufferPoolError};

    #[test]
    fn returns_buffer_to_pool_on_drop() {
        let pool = BufferPool::new(1, 16).unwrap();
        let mut buffer = pool.acquire().unwrap();
        buffer.set_len(8).unwrap();
        assert_eq!(buffer.len(), 8);
        assert_eq!(pool.acquire().unwrap_err(), BufferPoolError::Exhausted);
        drop(buffer);
        assert_eq!(pool.available_count().unwrap(), 1);
    }
}
