use gptgrep_jev::{
    DecisionInput, ExecutionContext, ImageEvidence, JevClient, MAX_RESPONSE_BYTES, SourceBinding,
    TextEvidence,
};
use serde_json::json;
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, SystemTime},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

fn questions() -> serde_json::Value {
    json!({"q":{"type":"noul","instructions":"Does this invented source support the stated relation?"}})
}
fn context(deadline: Duration, cap: u32) -> ExecutionContext {
    ExecutionContext::new(
        "invented-offline-run",
        &"a".repeat(64),
        SystemTime::now() + deadline,
        cap,
    )
    .unwrap()
}
fn binding(unit: &str) -> SourceBinding {
    SourceBinding {
        source_id: "invented-source".into(),
        unit_id: unit.into(),
        source_sha256: "a".repeat(64),
    }
}
async fn read_request(socket: &mut tokio::net::TcpStream) -> Vec<u8> {
    let mut bytes = Vec::new();
    loop {
        let mut chunk = [0; 4096];
        let len = socket.read(&mut chunk).await.unwrap();
        assert!(len > 0);
        bytes.extend_from_slice(&chunk[..len]);
        if let Some(offset) = bytes.windows(4).position(|p| p == b"\r\n\r\n") {
            let headers = String::from_utf8_lossy(&bytes[..offset]).to_ascii_lowercase();
            let len: usize = headers
                .lines()
                .find_map(|l| l.strip_prefix("content-length:"))
                .unwrap()
                .trim()
                .parse()
                .unwrap();
            if bytes.len() >= offset + 4 + len {
                return bytes;
            }
        }
        assert!(bytes.len() < 128 * 1024);
    }
}
async fn respond(socket: &mut tokio::net::TcpStream, status: &str, body: &str) {
    let bytes = format!(
        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = socket.write_all(bytes.as_bytes()).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn clones_share_four_slots_and_completed_failures_keep_submission_counts() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!(
        "http://{}/api/alpha/decisions",
        listener.local_addr().unwrap()
    );
    let active = Arc::new(AtomicUsize::new(0));
    let maximum = Arc::new(AtomicUsize::new(0));
    let server_active = active.clone();
    let server_maximum = maximum.clone();
    let server = tokio::spawn(async move {
        let mut tasks = Vec::new();
        for index in 0..12 {
            let (mut socket, _) = listener.accept().await.unwrap();
            let active = server_active.clone();
            let maximum = server_maximum.clone();
            tasks.push(tokio::spawn(async move {
                read_request(&mut socket).await;
                let current = active.fetch_add(1, Ordering::SeqCst) + 1;
                maximum.fetch_max(current, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(35)).await;
                active.fetch_sub(1, Ordering::SeqCst);
                match index % 3 {
                    0 => respond(&mut socket, "200 OK", r#"{"model":"invented-actual-model","answers":{"q":{"type":"noul","noul":0.8}}}"#).await,
                    1 => respond(&mut socket, "429 Too Many Requests", "invented-private-body-sentinel").await,
                    _ => respond(&mut socket, "200 OK", r#"{"model":"invented-model","answers":{"q":{"type":"UNKNOWN","isUnknown":true,"raw":{}}}}"#).await,
                }
            }));
        }
        for task in tasks {
            task.await.unwrap();
        }
    });
    let ctx = context(Duration::from_secs(5), 12);
    let client = JevClient::with_endpoint("invented-secret-sentinel", None, &endpoint)
        .unwrap()
        .with_execution_context(ctx.clone());
    let mut tasks = Vec::new();
    for _ in 0..12 {
        let client = client.clone();
        tasks.push(tokio::spawn(async move {
            client.decide(json!("invented source"), questions()).await
        }));
    }
    let mut successes = 0;
    for task in tasks {
        match task.await.unwrap() {
            Ok(response) => {
                successes += 1;
                assert_eq!(response.model, "invented-actual-model");
                assert!(response.usage.is_null());
            }
            Err(error) => {
                assert!(!error.to_string().contains("sentinel"));
            }
        }
    }
    server.await.unwrap();
    assert_eq!(successes, 4);
    assert_eq!(maximum.load(Ordering::SeqCst), 4);
    let snapshot = ctx.snapshot().unwrap();
    assert_eq!(snapshot.logical_reservations, 12);
    assert_eq!(snapshot.send_start_observations, 12);
    assert_eq!(snapshot.terminal_submissions, 12);
    assert_eq!(snapshot.active_or_quarantined_slots, 0);
    assert!(
        client
            .decide(json!("data"), questions())
            .await
            .unwrap_err()
            .to_string()
            .contains("budget exhausted")
    );
    assert_eq!(ctx.snapshot().unwrap().logical_reservations, 12);
}

#[tokio::test]
async fn cancellation_and_transport_unknown_quarantine_without_retry() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!(
        "http://{}/api/alpha/decisions",
        listener.local_addr().unwrap()
    );
    let (received, seen) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        read_request(&mut socket).await;
        received.send(()).unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        // No response: closure is a transport-unknown outcome, never a retry.
    });
    let ctx = context(Duration::from_secs(2), 2);
    let client = JevClient::with_endpoint("invented-key", None, &endpoint)
        .unwrap()
        .with_execution_context(ctx.clone());
    let task =
        tokio::spawn(async move { client.decide(json!("invented source"), questions()).await });
    seen.await.unwrap();
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    let snapshot = ctx.snapshot().unwrap();
    assert_eq!(snapshot.logical_reservations, 1);
    assert_eq!(snapshot.send_start_observations, 1);
    assert_eq!(snapshot.quarantined_slots, 1);
    server.await.unwrap();

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!(
        "http://{}/api/alpha/decisions",
        listener.local_addr().unwrap()
    );
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        read_request(&mut socket).await;
    });
    let client = JevClient::with_endpoint("invented-key", None, &endpoint)
        .unwrap()
        .with_execution_context(ctx.clone());
    assert!(
        client
            .decide(json!("invented source"), questions())
            .await
            .is_err()
    );
    server.await.unwrap();
    assert_eq!(ctx.snapshot().unwrap().quarantined_slots, 2);
    assert_eq!(ctx.snapshot().unwrap().terminal_submissions, 0);
}

#[tokio::test]
async fn absolute_deadline_bounds_transport_and_no_later_admission() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!(
        "http://{}/api/alpha/decisions",
        listener.local_addr().unwrap()
    );
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        read_request(&mut socket).await;
        tokio::time::sleep(Duration::from_millis(120)).await;
    });
    let ctx = context(Duration::from_millis(45), 3);
    let client = JevClient::with_endpoint("invented-key", None, &endpoint)
        .unwrap()
        .with_execution_context(ctx.clone());
    assert!(
        client
            .decide(json!("data"), questions())
            .await
            .unwrap_err()
            .to_string()
            .contains("deadline expired")
    );
    assert!(client.prepare_decision(json!("data"), questions()).is_err());
    assert_eq!(ctx.snapshot().unwrap().logical_reservations, 1);
    assert_eq!(ctx.snapshot().unwrap().quarantined_slots, 1);
    server.await.unwrap();
}

#[tokio::test]
async fn oversized_headers_and_partial_chunked_body_quarantine_charged_slots_before_eof() {
    let ctx = context(Duration::from_secs(3), 2);
    for chunked in [false, true] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!(
            "http://{}/api/alpha/decisions",
            listener.local_addr().unwrap()
        );
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            read_request(&mut socket).await;
            if chunked {
                let headers = format!(
                    "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n{:x}\r\n",
                    MAX_RESPONSE_BYTES + 1
                );
                socket.write_all(headers.as_bytes()).await.unwrap();
                let _ = socket.write_all(&vec![b' '; MAX_RESPONSE_BYTES + 1]).await;
                // Intentionally no final zero chunk: the body has not reached EOF.
            } else {
                let headers = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n",
                    MAX_RESPONSE_BYTES + 1
                );
                socket.write_all(headers.as_bytes()).await.unwrap();
                // Intentionally no body: the advertised response is unconsumed.
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        });
        let client = JevClient::with_endpoint("invented-key", None, &endpoint)
            .unwrap()
            .with_execution_context(ctx.clone());
        assert!(
            client
                .decide(json!("invented source"), questions())
                .await
                .unwrap_err()
                .to_string()
                .contains("byte limit")
        );
        server.await.unwrap();
    }
    let snapshot = ctx.snapshot().unwrap();
    assert_eq!(snapshot.logical_reservations, 2);
    assert_eq!(snapshot.send_start_observations, 2);
    assert_eq!(snapshot.quarantined_slots, 2);
    assert_eq!(snapshot.active_or_quarantined_slots, 2);
    assert_eq!(snapshot.terminal_submissions, 0);
}

#[tokio::test]
async fn typed_input_output_and_unqualified_codec_rejections_make_zero_submissions() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!(
        "http://{}/api/alpha/decisions",
        listener.local_addr().unwrap()
    );
    let ctx = context(Duration::from_secs(2), 3);
    let client = JevClient::with_endpoint("invented-key", None, &endpoint)
        .unwrap()
        .with_execution_context(ctx.clone());
    let input = DecisionInput {
        text: vec![TextEvidence {
            source: binding("text"),
            text: "invented source text".into(),
        }],
        images: vec![],
    };
    assert!(
        client
            .prepare_input(input.clone(), questions(), MAX_RESPONSE_BYTES - 1)
            .is_err()
    );
    let mut invalid = input.clone();
    invalid.text[0].source.source_sha256 = "invalid".into();
    assert!(
        client
            .prepare_input(invalid, questions(), MAX_RESPONSE_BYTES)
            .is_err()
    );
    let mut duplicate = input.clone();
    duplicate.text.push(duplicate.text[0].clone());
    assert!(
        client
            .prepare_input(duplicate, questions(), MAX_RESPONSE_BYTES)
            .is_err()
    );
    let many: serde_json::Map<String, serde_json::Value> = (0..65)
        .map(|i| (format!("q{i}"), questions()["q"].clone()))
        .collect();
    let mut conflicting = input.clone();
    let mut other = conflicting.text[0].clone();
    other.source.unit_id = "other-unit".into();
    other.source.source_sha256 = "b".repeat(64);
    conflicting.text.push(other);
    assert!(
        client
            .prepare_input(conflicting, questions(), MAX_RESPONSE_BYTES)
            .is_err()
    );
    assert!(
        client
            .prepare_input(input.clone(), many.into(), MAX_RESPONSE_BYTES)
            .is_err()
    );
    let mut png = std::io::Cursor::new(Vec::new());
    image::DynamicImage::new_rgb8(2, 3)
        .write_to(&mut png, image::ImageFormat::Png)
        .unwrap();
    let image = ImageEvidence::from_bytes(binding("page"), png.into_inner()).unwrap();
    assert_eq!(image.dimensions(), (2, 3));
    assert_eq!(image.media_type(), "image/png");
    assert_eq!(image.base64_bytes(), image.file_bytes().div_ceil(3) * 4);
    let image_input = DecisionInput {
        text: vec![],
        images: vec![image.clone()],
    };
    let error = client
        .prepare_input(image_input, questions(), MAX_RESPONSE_BYTES)
        .err()
        .unwrap();
    assert!(
        error
            .to_string()
            .contains("wire codec and refusal contract are unqualified")
    );
    assert!(
        ImageEvidence::from_bytes(binding("page"), vec![0; gptgrep_jev::MAX_IMAGE_BYTES + 1])
            .is_err()
    );
    assert!(ImageEvidence::from_bytes(binding("page"), b"not a PNG".to_vec()).is_err());
    assert!(
        DecisionInput {
            text: vec![],
            images: vec![image; 5]
        }
        .validate()
        .is_err()
    );
    assert!(
        client
            .prepare_input(input, questions(), MAX_RESPONSE_BYTES)
            .is_ok()
    );
    let snapshot = ctx.snapshot().unwrap();
    assert_eq!(snapshot.logical_reservations, 0);
    assert_eq!(snapshot.send_start_observations, 0);
    assert!(
        tokio::time::timeout(Duration::from_millis(30), listener.accept())
            .await
            .is_err()
    );
}

#[test]
fn image_file_snapshot_compressed_base64_and_pixel_limits_are_admitted_independently() {
    let mut png = std::io::Cursor::new(Vec::new());
    image::DynamicImage::new_rgb8(2, 3)
        .write_to(&mut png, image::ImageFormat::Png)
        .unwrap();
    let original = png.into_inner();
    let path = std::env::temp_dir().join(format!(
        "gptgrep-image-admission-{}-{}.png",
        std::process::id(),
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::write(&path, &original).unwrap();
    let admitted = ImageEvidence::from_file(binding("page"), &path).unwrap();
    std::fs::write(&path, b"changed after admission").unwrap();
    assert_eq!(admitted.file_bytes(), original.len());
    use sha2::Digest;
    assert_eq!(
        admitted.body_sha256(),
        format!("{:x}", sha2::Sha256::digest(&original))
    );
    assert!(ImageEvidence::from_file(binding("page"), &path).is_err());
    std::fs::remove_file(path).unwrap();
    let mut boundary = original;
    boundary.resize(gptgrep_jev::MAX_IMAGE_BYTES, 0);
    let admitted = ImageEvidence::from_bytes(binding("boundary"), boundary.clone()).unwrap();
    assert_eq!(admitted.base64_bytes(), gptgrep_jev::MAX_IMAGE_BASE64_BYTES);
    boundary.push(0);
    assert!(ImageEvidence::from_bytes(binding("boundary"), boundary).is_err());

    let mut png = std::io::Cursor::new(Vec::new());
    image::DynamicImage::new_rgb8(10_000, 1_000)
        .write_to(&mut png, image::ImageFormat::Png)
        .unwrap();
    assert!(
        ImageEvidence::from_bytes(binding("too-many-pixels"), png.into_inner())
            .err()
            .unwrap()
            .to_string()
            .contains("pixel limit")
    );
    let mut png = std::io::Cursor::new(Vec::new());
    image::DynamicImage::new_rgb8(2_000, 4_000)
        .write_to(&mut png, image::ImageFormat::Png)
        .unwrap();
    let bytes = png.into_inner();
    let images: Vec<_> = (0..4)
        .map(|index| {
            ImageEvidence::from_bytes(binding(&format!("page-{index}")), bytes.clone()).unwrap()
        })
        .collect();
    assert!(
        DecisionInput {
            text: vec![],
            images: images[..3].to_vec()
        }
        .validate()
        .is_ok()
    );
    assert!(
        DecisionInput {
            text: vec![],
            images
        }
        .validate()
        .err()
        .unwrap()
        .to_string()
        .contains("aggregate pixel limit")
    );
}
