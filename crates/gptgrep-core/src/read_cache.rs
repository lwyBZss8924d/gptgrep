//! Private immutable-byte cache primitive. A cache hit is not source freshness,
//! artifact custody or permission to cite evidence; the reader owns those checks.

use std::collections::HashMap;
use std::fmt;
use std::ops::Deref;
use std::sync::{Arc, Mutex};

use sha2::{Digest, Sha256};

const MAX_KEY_PART_BYTES: usize = 1024;

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct CacheKey {
    generation: String,
    artifact_id: String,
    content_sha256: String,
}

impl CacheKey {
    pub(crate) fn content_sha256(&self) -> &str {
        &self.content_sha256
    }
    pub(crate) fn new(
        generation: &str,
        artifact_id: &str,
        content_sha256: &str,
    ) -> Result<Self, CacheError> {
        if generation.is_empty()
            || artifact_id.is_empty()
            || generation.len() > MAX_KEY_PART_BYTES
            || artifact_id.len() > MAX_KEY_PART_BYTES
            || content_sha256.len() != 64
            || !content_sha256
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(CacheError::InvalidKey);
        }
        Ok(Self {
            generation: generation.into(),
            artifact_id: artifact_id.into(),
            content_sha256: content_sha256.into(),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CacheError {
    InvalidCapacity,
    InvalidKey,
    Oversized { requested: usize, capacity: usize },
    CapacityRetained,
    LengthMismatch { expected: usize, actual: usize },
    DigestMismatch,
    AllocationFailed,
    Loader(String),
    CounterOverflow,
    Poisoned,
}

impl fmt::Display for CacheError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidCapacity => formatter.write_str("read_cache_invalid_capacity"),
            Self::InvalidKey => formatter.write_str("read_cache_invalid_key"),
            Self::Oversized {
                requested,
                capacity,
            } => write!(
                formatter,
                "read_cache_oversized: requested={requested} capacity={capacity}"
            ),
            Self::CapacityRetained => formatter.write_str("read_cache_capacity_retained"),
            Self::LengthMismatch { expected, actual } => write!(
                formatter,
                "read_cache_length_mismatch: expected={expected} actual={actual}"
            ),
            Self::DigestMismatch => formatter.write_str("read_cache_digest_mismatch"),
            Self::AllocationFailed => formatter.write_str("read_cache_allocation_failed"),
            Self::Loader(message) => write!(formatter, "read_cache_loader: {message}"),
            Self::CounterOverflow => formatter.write_str("read_cache_counter_overflow"),
            Self::Poisoned => formatter.write_str("read_cache_state_poisoned"),
        }
    }
}

impl std::error::Error for CacheError {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReadOrigin {
    Loaded,
    Cache,
    Uncached,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct ReadMetrics {
    pub(crate) logical_delivered_bytes: u64,
    pub(crate) cached_logical_bytes: u64,
    pub(crate) physical_read_bytes: u64,
    pub(crate) physical_hash_bytes: u64,
    pub(crate) cache_hits: u64,
    pub(crate) cache_misses: u64,
    pub(crate) uncached_logical_bytes: u64,
    pub(crate) cache_fallbacks: u64,
    pub(crate) uncached_load_bytes: u64,
}

/// Clone shares actual counters. Delivery is recorded by the caller after it
/// knows the delivered window, rather than charging a whole fetched artifact.
#[derive(Clone, Debug, Default)]
pub(crate) struct ReadMeter(Arc<Mutex<ReadMetrics>>);

impl ReadMeter {
    pub(crate) fn snapshot(&self) -> Result<ReadMetrics, CacheError> {
        self.0
            .lock()
            .map(|value| *value)
            .map_err(|_| CacheError::Poisoned)
    }

    pub(crate) fn record_delivery(
        &self,
        bytes: usize,
        origin: ReadOrigin,
    ) -> Result<(), CacheError> {
        let bytes = u64::try_from(bytes).map_err(|_| CacheError::CounterOverflow)?;
        let mut metrics = self.0.lock().map_err(|_| CacheError::Poisoned)?;
        let delivered = metrics
            .logical_delivered_bytes
            .checked_add(bytes)
            .ok_or(CacheError::CounterOverflow)?;
        let cached = metrics
            .cached_logical_bytes
            .checked_add(if origin == ReadOrigin::Cache {
                bytes
            } else {
                0
            })
            .ok_or(CacheError::CounterOverflow)?;
        let uncached = metrics
            .uncached_logical_bytes
            .checked_add(if origin == ReadOrigin::Uncached {
                bytes
            } else {
                0
            })
            .ok_or(CacheError::CounterOverflow)?;
        metrics.logical_delivered_bytes = delivered;
        metrics.cached_logical_bytes = cached;
        metrics.uncached_logical_bytes = uncached;
        Ok(())
    }

    /// Record actual successful read bytes, including reads during failures or
    /// freshness checks. The cache never assumes a loader read its bound.
    pub(crate) fn record_physical_read(&self, bytes: usize) -> Result<(), CacheError> {
        self.add(bytes, |metrics| &mut metrics.physical_read_bytes)
    }

    /// Record actual bytes fed into a hash, separately from physical IO.
    pub(crate) fn record_physical_hash(&self, bytes: usize) -> Result<(), CacheError> {
        self.add(bytes, |metrics| &mut metrics.physical_hash_bytes)
    }

    fn add(
        &self,
        bytes: usize,
        select: impl FnOnce(&mut ReadMetrics) -> &mut u64,
    ) -> Result<(), CacheError> {
        let increment = u64::try_from(bytes).map_err(|_| CacheError::CounterOverflow)?;
        let mut metrics = self.0.lock().map_err(|_| CacheError::Poisoned)?;
        let counter = select(&mut metrics);
        *counter = counter
            .checked_add(increment)
            .ok_or(CacheError::CounterOverflow)?;
        Ok(())
    }

    fn lookup(&self, hit: bool) -> Result<(), CacheError> {
        self.add(1, |metrics| {
            if hit {
                &mut metrics.cache_hits
            } else {
                &mut metrics.cache_misses
            }
        })
    }

    pub(crate) fn record_fallback(&self) -> Result<(), CacheError> {
        self.add(1, |metrics| &mut metrics.cache_fallbacks)
    }

    pub(crate) fn record_uncached_load(&self, bytes: usize) -> Result<(), CacheError> {
        self.add(bytes, |metrics| &mut metrics.uncached_load_bytes)
    }
}

#[derive(Debug, Default)]
struct Retained {
    bytes: usize,
    entries: usize,
}

#[derive(Debug)]
struct Capacity {
    bytes: usize,
    entries: usize,
    retained: Mutex<Retained>,
}

impl Capacity {
    fn reserve(self: &Arc<Self>, bytes: usize) -> Result<Option<Charge>, CacheError> {
        let mut retained = self.retained.lock().map_err(|_| CacheError::Poisoned)?;
        let next_bytes = retained
            .bytes
            .checked_add(bytes)
            .ok_or(CacheError::CounterOverflow)?;
        let next_entries = retained
            .entries
            .checked_add(1)
            .ok_or(CacheError::CounterOverflow)?;
        if next_bytes > self.bytes || next_entries > self.entries {
            return Ok(None);
        }
        retained.bytes = next_bytes;
        retained.entries = next_entries;
        Ok(Some(Charge {
            capacity: Arc::clone(self),
            bytes,
        }))
    }
}

#[derive(Debug)]
struct Charge {
    capacity: Arc<Capacity>,
    bytes: usize,
}

impl Drop for Charge {
    fn drop(&mut self) {
        // Drop must release the original reservation even during unwinding.
        let mut retained = self
            .capacity
            .retained
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        retained.bytes -= self.bytes;
        retained.entries -= 1;
    }
}

#[derive(Debug)]
struct Payload {
    bytes: Vec<u8>,
    _charge: Charge,
}

/// The charge lives inside the Arc, so an evicted or cleared entry remains
/// charged until its last immutable pin drops. No owned byte-copy API exists.
#[derive(Clone, Debug)]
pub(crate) struct ReadPin(Arc<Payload>);

impl Deref for ReadPin {
    type Target = [u8];
    fn deref(&self) -> &Self::Target {
        &self.0.bytes
    }
}

#[derive(Debug)]
pub(crate) struct CacheRead {
    pub(crate) bytes: ReadPin,
    pub(crate) origin: ReadOrigin,
}

#[derive(Debug)]
struct Entry {
    payload: Arc<Payload>,
    touched: u64,
}

#[derive(Debug, Default)]
struct CacheState {
    entries: HashMap<CacheKey, Entry>,
    clock: u64,
}

impl CacheState {
    fn tick(&mut self) -> Result<u64, CacheError> {
        self.clock = self
            .clock
            .checked_add(1)
            .ok_or(CacheError::CounterOverflow)?;
        Ok(self.clock)
    }

    fn evict_lru(&mut self) -> bool {
        let key = self
            .entries
            .iter()
            .min_by_key(|(key, entry)| (entry.touched, *key))
            .map(|(key, _)| key.clone());
        key.is_some_and(|key| self.entries.remove(&key).is_some())
    }
}

#[derive(Clone, Debug)]
pub(crate) struct ReadCache {
    state: Arc<Mutex<CacheState>>,
    capacity: Arc<Capacity>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CacheStats {
    pub(crate) capacity_bytes: usize,
    pub(crate) capacity_entries: usize,
    pub(crate) retained_bytes: usize,
    pub(crate) retained_entries: usize,
    pub(crate) resident_bytes: usize,
    pub(crate) resident_entries: usize,
}

impl ReadCache {
    /// Limits come from the owning reader's admitted contract. Entry capacity
    /// also bounds zero-byte payloads and per-entry/key metadata.
    pub(crate) fn new(bytes: usize, entries: usize) -> Result<Self, CacheError> {
        if bytes == 0 || entries == 0 {
            return Err(CacheError::InvalidCapacity);
        }
        Ok(Self {
            state: Arc::new(Mutex::new(CacheState::default())),
            capacity: Arc::new(Capacity {
                bytes,
                entries,
                retained: Mutex::new(Retained::default()),
            }),
        })
    }

    pub(crate) fn stats(&self) -> Result<CacheStats, CacheError> {
        let state = self.state.lock().map_err(|_| CacheError::Poisoned)?;
        let retained = self
            .capacity
            .retained
            .lock()
            .map_err(|_| CacheError::Poisoned)?;
        Ok(CacheStats {
            capacity_bytes: self.capacity.bytes,
            capacity_entries: self.capacity.entries,
            retained_bytes: retained.bytes,
            retained_entries: retained.entries,
            resident_bytes: state
                .entries
                .values()
                .map(|entry| entry.payload.bytes.len())
                .sum(),
            resident_entries: state.entries.len(),
        })
    }

    pub(crate) fn clear(&self) -> Result<(), CacheError> {
        self.state
            .lock()
            .map_err(|_| CacheError::Poisoned)?
            .entries
            .clear();
        Ok(())
    }

    /// Reserve the exact known length before allocating the fill buffer or
    /// calling the trusted loader. It can only fill that admitted slice and
    /// must report its actual filled length. IO progress belongs in the meter,
    /// even when the loader fails. SHA-256 is verified before cache insertion.
    ///
    /// The loader runs outside the cache lock. Concurrent misses for the same
    /// key can perform separately charged loads; this primitive promises no
    /// single-flight behavior and does not perform path/freshness validation.
    pub(crate) fn get_or_load<F, E>(
        &self,
        key: &CacheKey,
        known_bytes: usize,
        meter: &ReadMeter,
        loader: F,
    ) -> Result<CacheRead, CacheError>
    where
        F: FnOnce(&mut [u8], &ReadMeter) -> Result<usize, E>,
        E: fmt::Display,
    {
        if known_bytes > self.capacity.bytes {
            return Err(CacheError::Oversized {
                requested: known_bytes,
                capacity: self.capacity.bytes,
            });
        }
        let charge = {
            let mut state = self.state.lock().map_err(|_| CacheError::Poisoned)?;
            if let Some(entry) = state.entries.get(key) {
                if entry.payload.bytes.len() != known_bytes {
                    return Err(CacheError::LengthMismatch {
                        expected: entry.payload.bytes.len(),
                        actual: known_bytes,
                    });
                }
                meter.lookup(true)?;
                let payload = Arc::clone(&entry.payload);
                let touched = state.tick()?;
                state
                    .entries
                    .get_mut(key)
                    .expect("existing cache entry")
                    .touched = touched;
                return Ok(CacheRead {
                    bytes: ReadPin(payload),
                    origin: ReadOrigin::Cache,
                });
            }
            meter.lookup(false)?;
            loop {
                if let Some(charge) = self.capacity.reserve(known_bytes)? {
                    break charge;
                }
                if !state.evict_lru() {
                    return Err(CacheError::CapacityRetained);
                }
            }
        };
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(known_bytes)
            .map_err(|_| CacheError::AllocationFailed)?;
        bytes.resize(known_bytes, 0);
        let actual =
            loader(&mut bytes, meter).map_err(|error| CacheError::Loader(error.to_string()))?;
        if actual != known_bytes {
            return Err(CacheError::LengthMismatch {
                expected: known_bytes,
                actual,
            });
        }
        let digest = Sha256::digest(&bytes);
        meter.record_physical_hash(bytes.len())?;
        let digits = b"0123456789abcdef";
        if !digest
            .iter()
            .zip(key.content_sha256.as_bytes().chunks_exact(2))
            .all(|(byte, pair)| {
                pair[0] == digits[usize::from(byte >> 4)]
                    && pair[1] == digits[usize::from(byte & 15)]
            })
        {
            return Err(CacheError::DigestMismatch);
        }
        let payload = Arc::new(Payload {
            bytes,
            _charge: charge,
        });
        let mut state = self.state.lock().map_err(|_| CacheError::Poisoned)?;
        // A racing load may already have inserted the same immutable identity.
        // Reuse it, release our duplicate charge, and preserve Loaded origin
        // because this call actually executed its loader/hash.
        if let Some(entry) = state.entries.get(key) {
            if entry.payload.bytes.len() != known_bytes {
                return Err(CacheError::LengthMismatch {
                    expected: entry.payload.bytes.len(),
                    actual: known_bytes,
                });
            }
            let existing = Arc::clone(&entry.payload);
            let touched = state.tick()?;
            state
                .entries
                .get_mut(key)
                .expect("existing cache entry")
                .touched = touched;
            return Ok(CacheRead {
                bytes: ReadPin(existing),
                origin: ReadOrigin::Loaded,
            });
        }
        let touched = state.tick()?;
        state.entries.insert(
            key.clone(),
            Entry {
                payload: Arc::clone(&payload),
                touched,
            },
        );
        Ok(CacheRead {
            bytes: ReadPin(payload),
            origin: ReadOrigin::Loaded,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn key(generation: &str, id: &str, bytes: &[u8]) -> CacheKey {
        let sha = format!("{:x}", Sha256::digest(bytes));
        CacheKey::new(generation, id, &sha).unwrap()
    }

    fn load(cache: &ReadCache, key: &CacheKey, bytes: &[u8], meter: &ReadMeter) -> CacheRead {
        cache
            .get_or_load(
                key,
                bytes.len(),
                meter,
                |buffer, meter| -> Result<usize, CacheError> {
                    buffer.copy_from_slice(bytes);
                    meter.record_physical_read(bytes.len())?;
                    Ok(bytes.len())
                },
            )
            .unwrap()
    }

    #[test]
    fn logical_delivery_and_cache_delivery_are_separate_from_actual_io_and_hashing() {
        let cache = ReadCache::new(64, 4).unwrap();
        let meter = ReadMeter::default();
        let key = key("g1", "a", b"abcdefgh");
        let first = load(&cache, &key, b"abcdefgh", &meter);
        assert_eq!(first.origin, ReadOrigin::Loaded);
        meter.record_delivery(3, first.origin).unwrap();
        let hit = cache
            .get_or_load(&key, 8, &meter, |_, _| -> Result<usize, CacheError> {
                panic!("cache hit must not load")
            })
            .unwrap();
        meter.clone().record_delivery(2, hit.origin).unwrap();
        // A source freshness read/hash is real work even on a cache hit.
        meter.record_physical_read(11).unwrap();
        meter.record_physical_hash(11).unwrap();
        assert_eq!(
            meter.snapshot().unwrap(),
            ReadMetrics {
                logical_delivered_bytes: 5,
                cached_logical_bytes: 2,
                physical_read_bytes: 19,
                physical_hash_bytes: 19,
                cache_hits: 1,
                cache_misses: 1,
                ..Default::default()
            }
        );
        assert_eq!(&*hit.bytes, b"abcdefgh");
    }

    #[test]
    fn lru_hits_refresh_recency_and_keys_bind_generation_artifact_and_digest() {
        let cache = ReadCache::new(8, 2).unwrap();
        let meter = ReadMeter::default();
        let a = key("g1", "a", b"aaaa");
        let b = key("g1", "b", b"bbbb");
        drop(load(&cache, &a, b"aaaa", &meter));
        drop(load(&cache, &b, b"bbbb", &meter));
        drop(load(&cache, &a, b"aaaa", &meter));
        let other_generation = key("g2", "a", b"aaaa");
        assert_eq!(
            load(&cache, &other_generation, b"aaaa", &meter).origin,
            ReadOrigin::Loaded
        );
        let state = cache.state.lock().unwrap();
        assert!(state.entries.contains_key(&a));
        assert!(!state.entries.contains_key(&b));
        assert!(state.entries.contains_key(&other_generation));
        drop(state);
        assert_ne!(a, key("g1", "other", b"aaaa"));
        assert_ne!(a, key("g1", "a", b"cccc"));
    }

    #[test]
    fn cloned_caches_and_evicted_pins_share_capacity_until_last_pin_drops() {
        let cache = ReadCache::new(4, 1).unwrap();
        let clone = cache.clone();
        let meter = ReadMeter::default();
        let first = load(&cache, &key("g", "a", b"aaaa"), b"aaaa", &meter);
        let pin_clone = first.bytes.clone();
        let called = AtomicUsize::new(0);
        let error = clone
            .get_or_load(
                &key("g", "b", b"bbbb"),
                4,
                &meter,
                |_, _| -> Result<usize, CacheError> {
                    called.fetch_add(1, Ordering::SeqCst);
                    Ok(4)
                },
            )
            .unwrap_err();
        assert_eq!(error, CacheError::CapacityRetained);
        assert_eq!(called.load(Ordering::SeqCst), 0);
        assert_eq!(clone.stats().unwrap().resident_entries, 0);
        assert_eq!(clone.stats().unwrap().retained_bytes, 4);
        drop(first);
        drop(cache);
        assert_eq!(clone.stats().unwrap().retained_bytes, 4);
        drop(pin_clone);
        assert_eq!(clone.stats().unwrap().retained_bytes, 0);
        let second = load(&clone, &key("g", "b", b"bbbb"), b"bbbb", &meter);
        assert_eq!(&*second.bytes, b"bbbb");
    }

    #[test]
    fn clear_keeps_pins_charged_and_oversize_does_not_evict_or_invoke_loader() {
        let cache = ReadCache::new(4, 1).unwrap();
        let meter = ReadMeter::default();
        let a = key("g", "a", b"aaaa");
        let pin = load(&cache, &a, b"aaaa", &meter);
        let before = cache.stats().unwrap();
        let error = cache
            .get_or_load(
                &key("g", "big", b"12345"),
                5,
                &meter,
                |_, _| -> Result<usize, CacheError> { panic!("oversize loader") },
            )
            .unwrap_err();
        assert!(matches!(error, CacheError::Oversized { .. }));
        assert_eq!(cache.stats().unwrap(), before);
        cache.clear().unwrap();
        assert_eq!(cache.stats().unwrap().retained_bytes, 4);
        drop(pin);
        assert_eq!(cache.stats().unwrap().retained_entries, 0);
    }

    #[test]
    fn zero_byte_entries_still_consume_shared_entry_capacity() {
        let cache = ReadCache::new(1, 1).unwrap();
        let meter = ReadMeter::default();
        let pin = load(&cache, &key("g", "empty1", b""), b"", &meter);
        cache.clear().unwrap();
        assert_eq!(cache.stats().unwrap().retained_bytes, 0);
        assert_eq!(cache.stats().unwrap().retained_entries, 1);
        assert_eq!(
            cache
                .get_or_load(
                    &key("g", "empty2", b""),
                    0,
                    &meter,
                    |_, _| -> Result<usize, CacheError> { panic!("pinned entry capacity") }
                )
                .unwrap_err(),
            CacheError::CapacityRetained
        );
        drop(pin);
        drop(load(&cache, &key("g", "empty2", b""), b"", &meter));
    }

    #[test]
    fn failed_partial_and_digest_mismatch_loads_release_admission_but_keep_actual_metering() {
        let cache = ReadCache::new(4, 1).unwrap();
        let meter = ReadMeter::default();
        let key = key("g", "a", b"aaaa");
        let error = cache
            .get_or_load(
                &key,
                4,
                &meter,
                |buffer, meter| -> Result<usize, CacheError> {
                    buffer[..2].copy_from_slice(b"aa");
                    meter.record_physical_read(2)?;
                    Err(CacheError::Loader("interrupted".into()))
                },
            )
            .unwrap_err();
        assert!(matches!(error, CacheError::Loader(_)));
        assert_eq!(cache.stats().unwrap().retained_entries, 0);
        let error = cache
            .get_or_load(
                &key,
                4,
                &meter,
                |buffer, meter| -> Result<usize, CacheError> {
                    buffer[..2].copy_from_slice(b"aa");
                    meter.record_physical_read(2)?;
                    Ok(2)
                },
            )
            .unwrap_err();
        assert_eq!(
            error,
            CacheError::LengthMismatch {
                expected: 4,
                actual: 2
            }
        );
        let error = cache
            .get_or_load(
                &key,
                4,
                &meter,
                |buffer, meter| -> Result<usize, CacheError> {
                    buffer.copy_from_slice(b"bbbb");
                    meter.record_physical_read(4)?;
                    Ok(4)
                },
            )
            .unwrap_err();
        assert_eq!(error, CacheError::DigestMismatch);
        assert_eq!(cache.stats().unwrap().retained_bytes, 0);
        assert_eq!(meter.snapshot().unwrap().physical_read_bytes, 8);
        assert_eq!(meter.snapshot().unwrap().physical_hash_bytes, 4);
        assert_eq!(meter.snapshot().unwrap().logical_delivered_bytes, 0);
        drop(load(&cache, &key, b"aaaa", &meter));
    }

    #[test]
    fn known_length_mismatch_on_hit_and_invalid_keys_fail_before_loader() {
        assert!(CacheKey::new("", "a", &"1".repeat(64)).is_err());
        assert!(CacheKey::new("g", "a", &"A".repeat(64)).is_err());
        assert!(CacheKey::new(&"g".repeat(1025), "a", &"1".repeat(64)).is_err());
        assert!(ReadCache::new(0, 1).is_err());
        assert!(ReadCache::new(1, 0).is_err());
        let cache = ReadCache::new(4, 1).unwrap();
        let meter = ReadMeter::default();
        let key = key("g", "a", b"aaaa");
        drop(load(&cache, &key, b"aaaa", &meter));
        assert!(matches!(
            cache
                .get_or_load(&key, 3, &meter, |_, _| -> Result<usize, CacheError> {
                    panic!("length mismatch loader")
                })
                .unwrap_err(),
            CacheError::LengthMismatch { .. }
        ));
    }

    #[test]
    fn pending_load_reservation_is_visible_to_other_clones_without_holding_cache_lock() {
        let cache = ReadCache::new(4, 1).unwrap();
        let clone = cache.clone();
        let meter = ReadMeter::default();
        let thread_meter = meter.clone();
        let (reserved, observed) = std::sync::mpsc::channel();
        let (continue_tx, continue_rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            clone
                .get_or_load(
                    &key("g", "a", b"aaaa"),
                    4,
                    &thread_meter,
                    |buffer, meter| -> Result<usize, CacheError> {
                        reserved.send(()).unwrap();
                        continue_rx.recv().unwrap();
                        buffer.copy_from_slice(b"aaaa");
                        meter.record_physical_read(4)?;
                        Ok(4)
                    },
                )
                .unwrap()
        });
        observed.recv().unwrap();
        assert_eq!(cache.stats().unwrap().retained_bytes, 4);
        assert_eq!(cache.stats().unwrap().resident_entries, 0);
        assert_eq!(
            cache
                .get_or_load(
                    &key("g", "b", b"bbbb"),
                    4,
                    &meter,
                    |_, _| -> Result<usize, CacheError> { panic!("pending reservation bypass") }
                )
                .unwrap_err(),
            CacheError::CapacityRetained
        );
        continue_tx.send(()).unwrap();
        let result = worker.join().unwrap();
        assert_eq!(&*result.bytes, b"aaaa");
    }

    #[test]
    fn delivery_overflow_is_atomic_and_never_silently_saturates() {
        let meter = ReadMeter::default();
        meter.0.lock().unwrap().cached_logical_bytes = u64::MAX;
        assert_eq!(
            meter.record_delivery(1, ReadOrigin::Cache),
            Err(CacheError::CounterOverflow)
        );
        assert_eq!(meter.snapshot().unwrap().logical_delivered_bytes, 0);
        meter.0.lock().unwrap().physical_read_bytes = u64::MAX;
        assert_eq!(
            meter.record_physical_read(1),
            Err(CacheError::CounterOverflow)
        );
    }

    #[test]
    fn impossible_allocation_releases_reserved_capacity_before_loader() {
        let cache = ReadCache::new(usize::MAX, 1).unwrap();
        let meter = ReadMeter::default();
        let error = cache
            .get_or_load(
                &key("g", "a", b""),
                usize::MAX,
                &meter,
                |_, _| -> Result<usize, CacheError> { panic!("failed allocation loader") },
            )
            .unwrap_err();
        assert_eq!(error, CacheError::AllocationFailed);
        assert_eq!(cache.stats().unwrap().retained_bytes, 0);
        assert_eq!(cache.stats().unwrap().retained_entries, 0);
        assert_eq!(meter.snapshot().unwrap().physical_read_bytes, 0);
    }

    #[test]
    fn racing_identical_misses_count_both_loads_but_retain_one_shared_payload() {
        let cache = ReadCache::new(8, 2).unwrap();
        let meter = ReadMeter::default();
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let mut workers = Vec::new();
        for _ in 0..2 {
            let cache = cache.clone();
            let meter = meter.clone();
            let barrier = Arc::clone(&barrier);
            workers.push(std::thread::spawn(move || {
                cache
                    .get_or_load(
                        &key("g", "a", b"aaaa"),
                        4,
                        &meter,
                        |buffer, meter| -> Result<usize, CacheError> {
                            barrier.wait();
                            buffer.copy_from_slice(b"aaaa");
                            meter.record_physical_read(4)?;
                            Ok(4)
                        },
                    )
                    .unwrap()
            }));
        }
        let first = workers.remove(0).join().unwrap();
        let second = workers.remove(0).join().unwrap();
        assert!(Arc::ptr_eq(&first.bytes.0, &second.bytes.0));
        assert_eq!(first.origin, ReadOrigin::Loaded);
        assert_eq!(second.origin, ReadOrigin::Loaded);
        assert_eq!(cache.stats().unwrap().retained_bytes, 4);
        assert_eq!(cache.stats().unwrap().retained_entries, 1);
        assert_eq!(meter.snapshot().unwrap().physical_read_bytes, 8);
        assert_eq!(meter.snapshot().unwrap().physical_hash_bytes, 8);
        assert_eq!(meter.snapshot().unwrap().cache_misses, 2);
    }

    #[test]
    fn loader_panic_releases_admission_without_poisoning_cache_state() {
        let cache = ReadCache::new(4, 1).unwrap();
        let meter = ReadMeter::default();
        let key = key("g", "a", b"aaaa");
        let result = std::panic::catch_unwind(|| {
            cache
                .get_or_load(&key, 4, &meter, |_, meter| -> Result<usize, CacheError> {
                    meter.record_physical_read(2)?;
                    panic!("synthetic loader unwind")
                })
                .unwrap()
        });
        assert!(result.is_err());
        assert_eq!(cache.stats().unwrap().retained_bytes, 0);
        assert_eq!(cache.stats().unwrap().retained_entries, 0);
        assert_eq!(meter.snapshot().unwrap().physical_read_bytes, 2);
        drop(load(&cache, &key, b"aaaa", &meter));
    }
}
