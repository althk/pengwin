use ext4_core::block_device::{BlockDevice, BlockDeviceError};
use lru::LruCache;
use parking_lot::Mutex;
use std::num::NonZeroUsize;

/// A BlockDevice wrapper that adds an LRU sector cache.
pub struct CachedBlockDevice<D: BlockDevice> {
    inner: D,
    cache: Mutex<LruCache<u64, [u8; 512]>>,
}

impl<D: BlockDevice> CachedBlockDevice<D> {
    pub fn new(inner: D, cache_size: usize) -> Self {
        let capacity = NonZeroUsize::new(cache_size)
            .unwrap_or(NonZeroUsize::new(1).expect("1 is non-zero"));
        Self {
            inner,
            cache: Mutex::new(LruCache::new(capacity)),
        }
    }
}

impl<D: BlockDevice + Send + Sync> BlockDevice for CachedBlockDevice<D> {
    fn read_sector(&self, sector_index: u64, buf: &mut [u8; 512]) -> Result<(), BlockDeviceError> {
        {
            let mut cache = self.cache.lock();
            if let Some(cached_buf) = cache.get(&sector_index) {
                buf.copy_from_slice(cached_buf);
                return Ok(());
            }
        }

        // Cache miss
        self.inner.read_sector(sector_index, buf)?;

        // Update cache
        {
            let mut cache = self.cache.lock();
            cache.put(sector_index, *buf);
        }

        Ok(())
    }

    fn sector_count(&self) -> u64 {
        self.inner.sector_count()
    }
}
