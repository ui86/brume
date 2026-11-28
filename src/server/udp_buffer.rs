use super::UDP_BUFFER_SIZE;
use std::sync::{Arc, Mutex};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

const MAX_BUFFERS: usize = 32;

pub(super) struct BufferPool {
    slots: Arc<Semaphore>,
    idle: Mutex<Vec<Vec<u8>>>,
}

impl BufferPool {
    pub(super) fn new() -> Arc<Self> {
        Arc::new(Self {
            slots: Arc::new(Semaphore::new(MAX_BUFFERS)),
            idle: Mutex::new(Vec::new()),
        })
    }

    pub(super) async fn acquire(self: &Arc<Self>) -> BufferLease {
        let permit = Arc::clone(&self.slots).acquire_owned().await.unwrap();
        let buffer = self
            .idle
            .lock()
            .unwrap()
            .pop()
            .unwrap_or_else(|| vec![0; UDP_BUFFER_SIZE]);
        BufferLease {
            buffer,
            pool: Arc::clone(self),
            _permit: permit,
        }
    }
}

pub(super) struct BufferLease {
    pub(super) buffer: Vec<u8>,
    pool: Arc<BufferPool>,
    _permit: OwnedSemaphorePermit,
}

impl Drop for BufferLease {
    fn drop(&mut self) {
        // 正常发送和任务取消均先归还缓冲，再释放许可，避免重复分配。
        self.pool
            .idle
            .lock()
            .unwrap()
            .push(std::mem::take(&mut self.buffer));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn buffer_limit_waits_and_reuses_released_storage() {
        let pool = BufferPool::new();
        let mut leases = Vec::new();
        for _ in 0..MAX_BUFFERS {
            leases.push(pool.acquire().await);
        }
        assert!(
            tokio::time::timeout(Duration::from_millis(20), pool.acquire())
                .await
                .is_err()
        );
        let last = leases.pop().unwrap();
        let pointer = last.buffer.as_ptr();
        drop(last);
        let lease = pool.acquire().await;
        assert_eq!(lease.buffer.as_ptr(), pointer);
        drop(lease);
        drop(leases);
        assert_eq!(pool.slots.available_permits(), MAX_BUFFERS);
        assert_eq!(pool.idle.lock().unwrap().len(), MAX_BUFFERS);
    }
}
