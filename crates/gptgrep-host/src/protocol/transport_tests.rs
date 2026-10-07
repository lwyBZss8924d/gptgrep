//! Ordinary source tests use in-memory Tokio transports, never a native runtime.
use super::*;
use std::{
    io,
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};
use tokio::io::AsyncReadExt;

fn rpc<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    reader: R,
    writer: W,
    budget: TransportBudget,
    deadline: Instant,
) -> Rpc<BufReader<BudgetedReader<R>>, W> {
    Rpc {
        reader: BufReader::new(BudgetedReader::new(reader, budget.clone())),
        writer: Arc::new(AsyncMutex::new(writer)),
        transport: budget,
        deadline,
        startup: Startup::default(),
        messages: 0,
        trace: None,
    }
}

#[tokio::test]
async fn actual_rpc_counts_outbound_and_stdout_once() {
    let input = b"{\"id\":1,\"result\":{}}\n";
    let budget = TransportBudget::new(1024);
    let mut rpc = rpc(
        &input[..],
        Vec::<u8>::new(),
        budget.clone(),
        Instant::now() + Duration::from_secs(1),
    );
    let request = json!({"id":1,"method":"initialize","params":{}});
    let encoded = crate::transport::encode_frame(&request, MAX_FRAME_BYTES).unwrap();
    rpc.send(request).await.unwrap();
    assert_eq!(rpc.receive().await.unwrap(), json!({"id":1,"result":{}}));
    assert_eq!(budget.used(), encoded.len() + input.len());
    assert_eq!(*rpc.writer.lock().await, encoded);
}

#[tokio::test]
async fn complete_outbound_admission_precedes_first_write() {
    let budget = TransportBudget::new(5);
    let mut rpc = rpc(
        &b""[..],
        Vec::<u8>::new(),
        budget.clone(),
        Instant::now() + Duration::from_secs(1),
    );
    assert_eq!(
        rpc.send(json!({"id":1})).await.unwrap_err().to_string(),
        "host_total_transport_limit"
    );
    assert!(rpc.writer.lock().await.is_empty());
    assert_eq!(budget.used(), 0);
    assert!(budget.overflowed());
}

struct FailingWriter {
    bytes: Vec<u8>,
}
impl AsyncWrite for FailingWriter {
    fn poll_write(
        mut self: Pin<&mut Self>,
        _: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        if self.bytes.len() == 5 {
            return Poll::Ready(Err(io::Error::other("owned fixture failure")));
        }
        let n = bytes.len().min(5 - self.bytes.len());
        self.bytes.extend_from_slice(&bytes[..n]);
        Poll::Ready(Ok(n))
    }
    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}
#[tokio::test]
async fn partial_send_is_unknown_and_never_refunds_or_retries() {
    let budget = TransportBudget::new(1024);
    let mut rpc = rpc(
        &b""[..],
        FailingWriter { bytes: Vec::new() },
        budget.clone(),
        Instant::now() + Duration::from_secs(1),
    );
    let value = json!({"id":1,"method":"initialize"});
    let length = crate::transport::encode_frame(&value, MAX_FRAME_BYTES)
        .unwrap()
        .len();
    assert_eq!(
        rpc.send(value.clone()).await.unwrap_err().to_string(),
        "Codex input transport closed"
    );
    assert_eq!(budget.used(), length);
    assert_eq!(
        budget.check_host().unwrap_err().to_string(),
        "host_transport_unknown"
    );
    assert_eq!(
        rpc.send(value).await.unwrap_err().to_string(),
        "host_transport_unknown"
    );
    assert_eq!(rpc.writer.lock().await.bytes.len(), 5);
    assert_eq!(budget.used(), length);
}

#[tokio::test]
async fn canceled_partial_send_closes_the_same_owned_writer() {
    let (writer, mut peer) = tokio::io::duplex(1);
    let budget = TransportBudget::new(1024);
    let mut rpc = rpc(
        &b""[..],
        writer,
        budget.clone(),
        Instant::now() + Duration::from_secs(1),
    );
    let value = json!({"id":1,"method":"thread/start"});
    let length = crate::transport::encode_frame(&value, MAX_FRAME_BYTES)
        .unwrap()
        .len();
    assert!(
        tokio::time::timeout(Duration::from_millis(20), rpc.send(value.clone()))
            .await
            .is_err()
    );
    assert_eq!(budget.used(), length);
    assert!(rpc.startup.thread_pending);
    let mut delivered = [0u8; 1];
    peer.read_exact(&mut delivered).await.unwrap();
    assert_eq!(delivered, b"{"[..]);
    assert_eq!(
        rpc.send(value).await.unwrap_err().to_string(),
        "host_transport_unknown"
    );
    assert_eq!(budget.used(), length);
}

struct FlushFailure {
    bytes: Vec<u8>,
}
impl AsyncWrite for FlushFailure {
    fn poll_write(
        mut self: Pin<&mut Self>,
        _: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.bytes.extend_from_slice(bytes);
        Poll::Ready(Ok(bytes.len()))
    }
    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Err(io::Error::other("owned flush fixture failure")))
    }
    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}
#[tokio::test]
async fn complete_write_with_failed_flush_is_still_unknown() {
    let budget = TransportBudget::new(1024);
    let mut rpc = rpc(
        &b""[..],
        FlushFailure { bytes: Vec::new() },
        budget.clone(),
        Instant::now() + Duration::from_secs(1),
    );
    let value = json!({"id":1});
    let frame = crate::transport::encode_frame(&value, MAX_FRAME_BYTES).unwrap();
    assert_eq!(
        rpc.send(value).await.unwrap_err().to_string(),
        "Codex input transport closed"
    );
    assert_eq!(rpc.writer.lock().await.bytes, frame);
    assert_eq!(budget.used(), frame.len());
    assert_eq!(
        budget.check_host().unwrap_err().to_string(),
        "host_transport_unknown"
    );
}

#[tokio::test]
async fn next_write_poll_observes_shared_stderr_failure() {
    let budget = TransportBudget::new(8);
    budget.admit(4).unwrap();
    let mut bytes = Vec::<u8>::new();
    let mut writer = CheckedWriter {
        writer: &mut bytes,
        execution: None,
        transport: &budget,
        deadline: Instant::now() + Duration::from_secs(1),
    };
    writer.write_all(b"ab").await.unwrap();
    assert!(budget.admit(5).is_err());
    assert!(writer.write_all(b"cd").await.is_err());
    assert!(writer.flush().await.is_err());
    assert_eq!(bytes, b"ab");
    assert_eq!(budget.used(), 4);
}

#[tokio::test]
async fn one_original_deadline_stops_pending_receive_and_future_send() {
    let (reader, _peer) = tokio::io::duplex(1);
    let budget = TransportBudget::new(1024);
    let deadline = Instant::now() + Duration::from_millis(20);
    let mut rpc = rpc(reader, Vec::<u8>::new(), budget.clone(), deadline);
    assert_eq!(
        rpc.receive().await.unwrap_err().to_string(),
        "host_receive_deadline_unknown"
    );
    assert_eq!(rpc.deadline, deadline);
    assert_eq!(
        budget.check_host().unwrap_err().to_string(),
        "host_transport_unknown"
    );
    assert!(rpc.send(json!({"id":1})).await.is_err());
    assert!(rpc.writer.lock().await.is_empty());
}

#[tokio::test]
async fn startup_reserves_before_send_and_cannot_replace_bound_thread_or_turn() {
    let budget = TransportBudget::new(4096);
    let mut rpc = rpc(
        &b""[..],
        Vec::<u8>::new(),
        budget.clone(),
        Instant::now() + Duration::from_secs(1),
    );
    assert!(
        rpc.send(json!({"id":0,"method":"turn/start","params":{}}))
            .await
            .is_err()
    );
    assert_eq!(budget.used(), 0);
    assert!(!rpc.startup.turn_pending);
    rpc.send(json!({"id":1,"method":"thread/start"}))
        .await
        .unwrap();
    let used = budget.used();
    assert_eq!(
        rpc.send(json!({"id":2,"method":"thread/start"}))
            .await
            .unwrap_err()
            .to_string(),
        "host_thread_start_replayed"
    );
    assert_eq!(budget.used(), used);
    rpc.bind_thread("thread-a").unwrap();
    assert_eq!(
        rpc.bind_thread("thread-b").unwrap_err().to_string(),
        "host_thread_already_bound"
    );
    assert!(
        rpc.send(json!({"id":3,"method":"turn/start","params":{"threadId":"thread-b"}}))
            .await
            .is_err()
    );
    rpc.send(json!({"id":4,"method":"turn/start","params":{"threadId":"thread-a"}}))
        .await
        .unwrap();
    let used = budget.used();
    assert_eq!(
        rpc.send(json!({"id":5,"method":"turn/start","params":{"threadId":"thread-a"}}))
            .await
            .unwrap_err()
            .to_string(),
        "host_turn_start_replayed"
    );
    assert_eq!(budget.used(), used);
    assert_eq!(rpc.startup.thread.as_deref(), Some("thread-a"));
}

#[test]
fn outer_frame_limit_includes_the_newline_for_both_directions() {
    let value = json!("x");
    assert_eq!(
        crate::transport::encode_frame(&value, 4).unwrap(),
        b"\"x\"\n"
    );
    assert_eq!(
        crate::transport::encode_frame(&value, 3)
            .unwrap_err()
            .to_string(),
        "host_frame_limit"
    );
    assert!(
        crate::transport::encode_frame(&json!("x".repeat(MAX_FRAME_BYTES)), MAX_FRAME_BYTES)
            .is_err()
    );
}

#[tokio::test]
async fn receive_accepts_exact_delimited_frame_cap_and_rejects_one_more_byte() {
    let value = json!("x".repeat(MAX_FRAME_BYTES - 3));
    let frame = crate::transport::encode_frame(&value, MAX_FRAME_BYTES).unwrap();
    assert_eq!(frame.len(), MAX_FRAME_BYTES);
    let mut rpc = rpc(
        &frame[..],
        Vec::<u8>::new(),
        TransportBudget::new(MAX_TOTAL_BYTES),
        Instant::now() + Duration::from_secs(2),
    );
    assert_eq!(rpc.receive().await.unwrap(), value);
    let mut oversized = vec![b' '; MAX_FRAME_BYTES];
    oversized.push(b'\n');
    let mut rpc = super::transport_tests::rpc(
        &oversized[..],
        Vec::<u8>::new(),
        TransportBudget::new(MAX_TOTAL_BYTES),
        Instant::now() + Duration::from_secs(2),
    );
    assert_eq!(
        rpc.receive().await.unwrap_err().to_string(),
        "Codex frame exceeded the byte limit"
    );
}

#[tokio::test]
async fn shared_stdout_stderr_limit_keeps_rejected_bytes_out_of_owned_buffers() {
    let budget = TransportBudget::new(8);
    let mut stdout = BudgetedReader::new(&b"abcd"[..], budget.clone());
    let mut stderr = BudgetedReader::new(&b"efghi"[..], budget.clone());
    let mut retained = Vec::new();
    stdout.read_to_end(&mut retained).await.unwrap();
    assert_eq!(retained, b"abcd");
    let mut rejected = Vec::new();
    assert!(stderr.read_to_end(&mut rejected).await.is_err());
    assert!(rejected.is_empty());
    assert_eq!(budget.used(), 4);
    // Only the run-owned designated waiter consumes notification; this is not a
    // general broadcast API for arbitrary cloned waiters.
    tokio::time::timeout(Duration::from_millis(20), budget.exceeded())
        .await
        .unwrap();
    assert_eq!(
        budget.check().unwrap_err().to_string(),
        "ocr_total_transport_limit"
    );
}

#[tokio::test]
async fn expired_original_deadline_does_not_admit_a_frame() {
    let budget = TransportBudget::new(1024);
    let mut rpc = rpc(
        &b""[..],
        Vec::<u8>::new(),
        budget.clone(),
        Instant::now() - Duration::from_millis(1),
    );
    assert_eq!(
        rpc.send(json!({"id":1})).await.unwrap_err().to_string(),
        "host_deadline_exceeded"
    );
    assert_eq!(budget.used(), 0);
    assert!(rpc.writer.lock().await.is_empty());
}
