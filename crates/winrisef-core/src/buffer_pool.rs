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
    buffer_count: usize,
    state: Mutex<PoolState>,
}

#[derive(Debug)]
struct PoolState {
    allocated: usize,
    available: Vec<Vec<u8>>,
}

#[derive(Debug)]
pub struct PooledBuffer {
    inner: Arc<PoolInner>,
    bytes: Option<Vec<u8>>,
    logical_len: usize,
}

impl BufferPool {
    pub fn new(buffer_count: usize, block_size: usize) -> Result<Self, BufferPoolError> {
        validate_shape(buffer_count, block_size)?;
        let mut available = Vec::with_capacity(buffer_count);
        for _ in 0..buffer_count {
            available.push(vec![0; block_size]);
        }
        Ok(Self {
            inner: Arc::new(PoolInner {
                block_size,
                buffer_count,
                state: Mutex::new(PoolState {
                    allocated: buffer_count,
                    available,
                }),
            }),
        })
    }

    pub fn new_lazy(buffer_count: usize, block_size: usize) -> Result<Self, BufferPoolError> {
        validate_shape(buffer_count, block_size)?;
        Ok(Self {
            inner: Arc::new(PoolInner {
                block_size,
                buffer_count,
                state: Mutex::new(PoolState {
                    allocated: 0,
                    available: Vec::with_capacity(buffer_count),
                }),
            }),
        })
    }

    pub fn acquire(&self) -> Result<PooledBuffer, BufferPoolError> {
        let mut state = self
            .inner
            .state
            .lock()
            .map_err(|_| BufferPoolError::Poisoned)?;
        let bytes = if let Some(bytes) = state.available.pop() {
            bytes
        } else if state.allocated < self.inner.buffer_count {
            state.allocated += 1;
            drop(state);
            vec![0; self.inner.block_size]
        } else {
            return Err(BufferPoolError::Exhausted);
        };
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
            .state
            .lock()
            .map_err(|_| BufferPoolError::Poisoned)?
            .available
            .len())
    }

    pub fn allocated_count(&self) -> Result<usize, BufferPoolError> {
        Ok(self
            .inner
            .state
            .lock()
            .map_err(|_| BufferPoolError::Poisoned)?
            .allocated)
    }
}

fn validate_shape(buffer_count: usize, block_size: usize) -> Result<(), BufferPoolError> {
    if buffer_count == 0 {
        return Err(BufferPoolError::EmptyPool);
    }
    if block_size == 0 {
        return Err(BufferPoolError::EmptyBuffer);
    }
    Ok(())
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
        if let Ok(mut state) = self.inner.state.lock() {
            state.available.push(bytes);
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

    #[test]
    fn lazy_pool_allocates_once_and_reuses_the_buffer() {
        let pool = BufferPool::new_lazy(1, 16).unwrap();
        assert_eq!(pool.available_count().unwrap(), 0);
        assert_eq!(pool.allocated_count().unwrap(), 0);

        let mut buffer = pool.acquire().unwrap();
        let pointer = buffer.full_mut().as_ptr();
        assert_eq!(pool.allocated_count().unwrap(), 1);
        drop(buffer);

        let mut reused = pool.acquire().unwrap();
        assert_eq!(reused.full_mut().as_ptr(), pointer);
    }
}
