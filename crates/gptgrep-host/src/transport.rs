use anyhow::{Result, anyhow, ensure};
use std::{
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    task::{Context, Poll},
};
use tokio::{
    io::{AsyncRead, ReadBuf},
    sync::Notify,
};
#[derive(Clone)]
pub(crate) struct TransportBudget(Arc<TransportState>);

struct TransportState {
    used: AtomicUsize,
    limit: usize,
    exceeded: AtomicBool,
    unknown: AtomicBool,
    wake: Notify,
}

impl TransportBudget {
    pub fn new(limit: usize) -> Self {
        Self(Arc::new(TransportState {
            used: AtomicUsize::new(0),
            limit,
            exceeded: AtomicBool::new(false),
            unknown: AtomicBool::new(false),
            wake: Notify::new(),
        }))
    }
    pub fn used(&self) -> usize {
        self.0.used.load(Ordering::SeqCst)
    }
    pub fn remaining(&self) -> usize {
        self.0.limit.saturating_sub(self.used())
    }
    pub fn check(&self) -> Result<()> {
        ensure!(
            !self.0.exceeded.load(Ordering::SeqCst),
            "ocr_total_transport_limit"
        );
        ensure!(
            !self.0.unknown.load(Ordering::SeqCst),
            "host_transport_unknown"
        );
        Ok(())
    }
    pub(crate) fn overflowed(&self) -> bool {
        self.0.exceeded.load(Ordering::SeqCst)
    }
    pub(crate) fn check_host(&self) -> Result<()> {
        ensure!(!self.overflowed(), "host_total_transport_limit");
        ensure!(
            !self.0.unknown.load(Ordering::SeqCst),
            "host_transport_unknown"
        );
        Ok(())
    }
    pub(crate) fn admit(&self, bytes: usize) -> Result<()> {
        self.check()?;
        if self
            .0
            .used
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |used| {
                used.checked_add(bytes)
                    .filter(|total| *total <= self.0.limit)
            })
            .is_err()
        {
            self.0.exceeded.store(true, Ordering::SeqCst);
            self.0.wake.notify_one();
            return Err(anyhow!("ocr_total_transport_limit"));
        }
        self.check()
    }
    pub(crate) fn invalidate(&self) {
        self.0.unknown.store(true, Ordering::SeqCst);
        self.0.wake.notify_one();
    }
    pub async fn exceeded(&self) {
        loop {
            let wake = self.0.wake.notified();
            if self.0.exceeded.load(Ordering::SeqCst) || self.0.unknown.load(Ordering::SeqCst) {
                return;
            }
            wake.await;
        }
    }
}

/// Only a fixed scratch chunk precedes admission. Unadmitted bytes never enter the
/// protocol frame buffer or a retained stderr buffer, and no next read follows failure.
pub(crate) struct BudgetedReader<R> {
    reader: R,
    transport: TransportBudget,
}
impl<R> BudgetedReader<R> {
    pub fn new(reader: R, transport: TransportBudget) -> Self {
        Self { reader, transport }
    }
}
impl<R: AsyncRead + Unpin> AsyncRead for BudgetedReader<R> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        output: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        if let Err(error) = self.transport.check() {
            return Poll::Ready(Err(std::io::Error::other(error.to_string())));
        }
        let mut scratch = [0u8; 4096];
        let capacity = output.remaining().min(scratch.len());
        if capacity == 0 {
            return Poll::Ready(Ok(()));
        }
        let mut chunk = ReadBuf::new(&mut scratch[..capacity]);
        match Pin::new(&mut self.reader).poll_read(cx, &mut chunk) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Err(error)) => Poll::Ready(Err(error)),
            Poll::Ready(Ok(())) => {
                if let Err(error) = self.transport.admit(chunk.filled().len()) {
                    return Poll::Ready(Err(std::io::Error::other(error.to_string())));
                }
                output.put_slice(chunk.filled());
                Poll::Ready(Ok(()))
            }
        }
    }
}

/// Reject the encoded outer frame before retaining bytes beyond its hard cap.
pub(crate) fn encode_frame(value: &serde_json::Value, limit: usize) -> Result<Vec<u8>> {
    struct Frame {
        bytes: Vec<u8>,
        limit: usize,
    }
    impl std::io::Write for Frame {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            let end = self
                .bytes
                .len()
                .checked_add(bytes.len())
                .filter(|n| *n <= self.limit)
                .ok_or_else(|| std::io::Error::other("host_frame_limit"))?;
            self.bytes
                .try_reserve_exact(end - self.bytes.len())
                .map_err(|_| std::io::Error::other("host_frame_allocation"))?;
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    ensure!(limit > 0, "host_frame_limit");
    let mut frame = Frame {
        bytes: Vec::new(),
        limit: limit - 1,
    };
    serde_json::to_writer(&mut frame, value).map_err(|_| anyhow!("host_frame_limit"))?;
    frame.bytes.push(b'\n');
    Ok(frame.bytes)
}
