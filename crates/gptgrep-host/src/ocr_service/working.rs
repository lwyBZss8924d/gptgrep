//! Aggregate logical admission for service-owned buffers. This is not process RSS.
use super::{Result, ServiceFailure};
use anyhow::ensure;
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

#[derive(Clone)]
pub(crate) struct WorkingBudget(Arc<Counter>);
struct Counter {
    limit: u64,
    used: AtomicU64,
}
pub(crate) struct WorkingLease {
    budget: WorkingBudget,
    bytes: u64,
}
impl WorkingBudget {
    pub(crate) fn new(limit: u64) -> Self {
        Self(Arc::new(Counter {
            limit,
            used: AtomicU64::new(0),
        }))
    }
    pub(crate) fn limit(&self) -> u64 {
        self.0.limit
    }
    pub(crate) fn reserve(&self, bytes: u64) -> Result<WorkingLease> {
        self.admit(bytes)?;
        Ok(WorkingLease {
            budget: self.clone(),
            bytes,
        })
    }
    fn admit(&self, bytes: u64) -> Result<()> {
        let mut used = self.0.used.load(Ordering::Acquire);
        loop {
            let next = used
                .checked_add(bytes)
                .filter(|n| *n <= self.0.limit)
                .ok_or(ServiceFailure::WorkingLimit)?;
            match self
                .0
                .used
                .compare_exchange_weak(used, next, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => return Ok(()),
                Err(actual) => used = actual,
            }
        }
    }
    #[cfg(test)]
    pub(crate) fn used(&self) -> u64 {
        self.0.used.load(Ordering::Acquire)
    }
}
impl WorkingLease {
    pub(crate) fn shrink_to(&mut self, bytes: u64) -> Result<()> {
        ensure!(bytes <= self.bytes, "ocr_service_working_lease_grow");
        self.budget
            .0
            .used
            .fetch_sub(self.bytes - bytes, Ordering::AcqRel);
        self.bytes = bytes;
        Ok(())
    }
    pub(crate) fn bytes(&self) -> u64 {
        self.bytes
    }
    /// Grow before the corresponding buffer allocation; failure preserves the old charge.
    pub(crate) fn grow_to(&mut self, bytes: u64) -> Result<()> {
        ensure!(bytes >= self.bytes, "ocr_service_working_lease_shrink");
        self.budget.admit(bytes - self.bytes)?;
        self.bytes = bytes;
        Ok(())
    }
}
impl Drop for WorkingLease {
    fn drop(&mut self) {
        self.budget.0.used.fetch_sub(self.bytes, Ordering::AcqRel);
    }
}

/// The lease follows all Bytes clones and slices, including queued HTTP data after
/// the request owner exits. No Vec/BytesMut copy is made by this service.
pub(crate) struct LeasedResponse {
    pub(crate) bytes: Vec<u8>,
    pub(crate) _lease: Arc<WorkingLease>,
}
impl AsRef<[u8]> for LeasedResponse {
    fn as_ref(&self) -> &[u8] {
        &self.bytes
    }
}
