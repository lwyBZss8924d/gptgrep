use super::*;
use crate::rich_batch::tests::{publish, publish_visual, request};
use anyhow::Result;
use gptgrep_jev::ExecutionContext;
use serde_json::json;
use std::{
    fs,
    time::{Duration, SystemTime},
};

fn client(limit: u32) -> Result<JevClient> {
    Ok(
        JevClient::new("ordinary-dummy-no-network", None)?.with_execution_context(
            ExecutionContext::new(
                "ordinary-paid-batch",
                &"b".repeat(64),
                SystemTime::now() + Duration::from_secs(60),
                limit,
            )?,
        ),
    )
}
fn paid(id: &str) -> Value {
    json!({"id":id,"op":"search_multimodal","query":"ordinary source evidence"})
}
fn reader(root: &Path) -> Result<RichGenerationReader> {
    publish(root, &["ordinary source evidence"])?;
    RichGenerationReader::open(root)
}

#[test]
fn paid_schema_rejects_cursor_scope_and_call_cap_before_io() {
    let input = json!({"schema_version":RICH_BATCH_REQUEST_SCHEMA,"operations":[{"id":"rank","op":"search_multimodal","query":"ordinary","min_score":0.25}]});
    assert!(parse_rich_batch_request(&serde_json::to_vec(&input).unwrap()).is_ok());
    let input = json!({"schema_version":RICH_BATCH_REQUEST_SCHEMA,"operations":[{"id":"rank","op":"search_multimodal","query":"ordinary","cursor":"forbidden"}]});
    assert!(parse_rich_batch_request(&serde_json::to_vec(&input).unwrap()).is_err());
    let input = json!({"schema_version":RICH_BATCH_REQUEST_SCHEMA,"budget":{"max_decisions_submissions":201},"operations":[paid("rank")]});
    assert!(parse_rich_batch_request(&serde_json::to_vec(&input).unwrap()).is_err());
    let input = json!({"schema_version":RICH_BATCH_REQUEST_SCHEMA,"operations":[paid("same"),paid("same")]});
    assert!(parse_rich_batch_request(&serde_json::to_vec(&input).unwrap()).is_err());
    for field in [
        json!({"window_bytes":3}),
        json!({"max_concurrent_batches":5}),
        json!({"document":"../outside"}),
    ] {
        let mut op = paid("rank");
        op.as_object_mut()
            .unwrap()
            .extend(field.as_object().unwrap().clone());
        let r = request(json!([op]));
        assert!(
            preflight(
                &GenerationPin {
                    generation: "fixed".into(),
                    manifest_sha256: "a".repeat(64)
                },
                &r.operations[0]
            )
            .is_err()
        );
    }
}
#[test]
fn paid_preparation_retains_exact_calls_pin_and_context_without_submission() -> Result<()> {
    let root = tempfile::tempdir()?;
    let reader = reader(root.path())?;
    let c = client(8)?;
    let ctx = c.execution_context().clone();
    let p = prepare_rich_batch_with_reader(
        &reader,
        request(json!([paid("one"),{"id":"catalog","op":"inspect_documents"},paid("two")])),
        Some(c),
    )?;
    assert_eq!(p.generation(), reader.pin());
    assert_eq!(p.item_call_shares(), &[2, 0, 2]);
    assert_eq!(p.reserved_calls(), 4);
    assert_eq!(
        p.prepared_searches().map(|(id, _)| id).collect::<Vec<_>>(),
        ["one", "two"]
    );
    assert!(p.prepared_searches().all(|(_, p)| {
        p.routing_requests()
            .chain(p.rerank_pool_requests())
            .all(|r| r.body_bytes() == r.canonical_body().len())
    }));
    assert_eq!(p.reserved_output_bytes(), 1_048_576);
    assert!(
        p.searches
            .iter()
            .flatten()
            .all(|p| p.generation() == &reader.pin())
    );
    let s = ctx.snapshot()?;
    assert_eq!(s.logical_reservations, 0);
    assert_eq!(s.submission_observations, 0);
    assert_eq!(s.send_start_observations, 0);
    Ok(())
}
#[test]
fn whole_call_shares_never_borrow_or_reset_parent_limit() -> Result<()> {
    let root = tempfile::tempdir()?;
    let reader = reader(root.path())?;
    let mut r =
        request(json!([paid("first"),paid("second"),{"id":"local","op":"inspect_documents"}]));
    r.budget.max_decisions_submissions = 2;
    let p = prepare_rich_batch_with_reader(&reader, r, Some(client(8)?))?;
    assert_eq!(p.item_call_shares(), &[2, 0, 0]);
    assert_eq!(
        p.starts[1].as_ref().unwrap_err().code,
        RichBatchErrorCode::SubmissionBudgetInsufficient
    );
    let p =
        prepare_rich_batch_with_reader(&reader, request(json!([paid("first")])), Some(client(1)?))?;
    assert_eq!(p.reserved_calls(), 0);
    assert_eq!(
        p.starts[0].as_ref().unwrap_err().code,
        RichBatchErrorCode::SubmissionBudgetInsufficient
    );
    Ok(())
}
#[test]
fn paid_output_failure_admits_no_calls_and_preserves_other_shares() -> Result<()> {
    let root = tempfile::tempdir()?;
    let reader = reader(root.path())?;
    let c = client(8)?;
    let ctx = c.execution_context().clone();
    let mut r = request(json!([paid("rank"),{"id":"catalog","op":"inspect_documents"}]));
    r.budget.max_output_bytes = 8192;
    let p = prepare_rich_batch_with_reader(&reader, r, Some(c))?;
    assert_eq!(p.reserved_calls(), 0);
    assert_eq!(p.quotas, &[2048, 2048]);
    assert_eq!(
        p.starts[0].as_ref().unwrap_err().code,
        RichBatchErrorCode::OutputBudgetInsufficient
    );
    assert!(p.starts[1].is_ok());
    assert_eq!(ctx.snapshot()?.logical_reservations, 0);
    Ok(())
}
#[tokio::test]
async fn missing_context_is_item_failure_and_local_results_keep_parity() -> Result<()> {
    let root = tempfile::tempdir()?;
    let reader = reader(root.path())?;
    let local = request(json!([{"id":"catalog","op":"inspect_documents"}]));
    let one = dispatch_rich_batch_with_reader(&reader, local.clone())?;
    let two = dispatch_rich_batch_with_reader_and_client(&reader, local, None).await?;
    assert_eq!(one.items[0].result, two.items[0].result);
    assert_eq!(two.execution.model_submissions, Some(0));
    assert!(
        serde_json::to_value(&two)?["execution"]
            .get("context_before")
            .is_none()
    );
    let r = dispatch_rich_batch_with_reader_and_client(
        &reader,
        request(json!([paid("missing"),{"id":"local","op":"inspect_documents"}])),
        None,
    )
    .await?;
    assert_eq!(
        r.items[0].error.as_ref().unwrap().code,
        RichBatchErrorCode::ModelContextRequired
    );
    assert_eq!(r.items[1].status, RichBatchStatus::Ok);
    assert_eq!(r.execution.model_submissions, None);
    assert_eq!(
        r.metrics
            .decisions
            .unwrap()
            .unique_scheduled_logical_attempts,
        0
    );
    Ok(())
}
#[test]
fn inherited_expiry_and_stale_source_fail_before_reservation() -> Result<()> {
    let root = tempfile::tempdir()?;
    let reader = reader(root.path())?;
    let c = client(8)?;
    let ctx = c
        .execution_context()
        .with_deadline(Instant::now() + Duration::from_millis(20))?;
    // Only the in-memory view expires; no production RunClockAnchor is captured.
    std::thread::sleep(Duration::from_millis(30));
    let p = prepare_rich_batch_with_reader(
        &reader,
        request(json!([paid("expired")])),
        Some(c.with_execution_context(ctx.clone())),
    )?;
    assert!(p.starts[0].is_err());
    assert_eq!(ctx.snapshot()?.logical_reservations, 0);
    fs::write(root.path().join("source.pdf"), b"changed source")?;
    let c = client(8)?;
    let ctx = c.execution_context().clone();
    let p = prepare_rich_batch_with_reader(&reader, request(json!([paid("stale")])), Some(c))?;
    assert!(p.starts[0].is_err());
    assert_eq!(ctx.snapshot()?.submission_observations, 0);
    Ok(())
}
#[test]
fn pending_preparations_keep_old_pin_after_current_publication() -> Result<()> {
    let root = tempfile::tempdir()?;
    let reader = reader(root.path())?;
    let pin = reader.pin();
    let p = prepare_rich_batch_with_reader(
        &reader,
        request(json!([paid("rank"),{"id":"catalog","op":"inspect_documents"}])),
        Some(client(8)?),
    )?;
    let next = publish(root.path(), &["ordinary source evidence"])?;
    assert_ne!(next, pin);
    assert_eq!(p.generation(), pin);
    p.reader.validate()?;
    assert_eq!(p.searches[0].as_ref().unwrap().generation(), &pin);
    Ok(())
}
#[test]
fn bounded_usage_projection_preserves_null_unknown_and_huge_number_distinctions() {
    let usage: Value = serde_json::from_str(&format!(
        "{{\"cost\":0.{}1,\"input_tokens\":18446744073709551615,\"output_tokens\":null}}",
        "0".repeat(100_000)
    ))
    .unwrap();
    let c = observed(&usage, "cost", |v| {
        v.as_f64().filter(|n| n.is_finite() && *n >= 0.0)
    });
    assert_eq!(c.presence, UsagePresence::Reported);
    assert!(serde_json::to_vec(&c).unwrap().len() < 128);
    assert_eq!(
        observed(&usage, "input_tokens", Value::as_u64).value,
        Some(u64::MAX)
    );
    assert_eq!(
        observed(&usage, "output_tokens", Value::as_u64).presence,
        UsagePresence::Null
    );
    assert_eq!(
        observed(&usage, "missing", Value::as_u64).presence,
        UsagePresence::Absent
    );
    assert_eq!(
        observed(&json!({"cost":"unknown"}), "cost", Value::as_f64).presence,
        UsagePresence::Invalid
    );
}

#[test]
fn maximum_escaped_attempt_projection_fits_reserved_call_envelope() -> Result<()> {
    // Size discrimination only: this fixture is not a provider observation.
    let ids = vec!["a".repeat(64); 64];
    let escaped = "\\\"".repeat(128);
    let digest = "b".repeat(64);
    let fields = AttemptFields {
        stage: "document_routing",
        batch_index: usize::MAX,
        candidate_ids: &ids,
        requested_model: gptgrep_jev::DEFAULT_MODEL,
        prepared_request_sha256: &digest,
        encoded_request_bytes: 8_388_608,
        question_count: 64,
        response_status: "validated",
        returned_model: Some("openai/gpt-6-luna-decisions-20261006"),
        provider_response_id: Some(&escaped),
        provider: Some(&escaped),
        failure: None,
        actual_provider_requests: None,
    };
    let usage = json!({"input_tokens":u64::MAX,"output_tokens":u64::MAX,"cost":f64::MAX});
    let a = BatchAttempt {
        batch_attempt_id: digest.clone(),
        batch_attempt_identity_scope: "batch_response_local_not_sdk_native_or_cross_batch",
        attempt: fields,
        usage: BatchUsage {
            input_tokens: observed(&usage, "input_tokens", Value::as_u64),
            output_tokens: observed(&usage, "output_tokens", Value::as_u64),
            cost: observed(&usage, "cost", Value::as_f64),
            usage_observed: true,
            complete_usage_sha256: Some(&digest),
            projection: "bounded_numeric_fields; cost is a lossy finite_f64 projection; other usage metadata omitted and SHA retained",
        },
    };
    assert!(measure(&a, 8192).is_ok());
    Ok(())
}

#[tokio::test]
async fn escaped_query_small_output_and_shared_snapshot_are_admitted_before_provider() -> Result<()>
{
    let root = tempfile::tempdir()?;
    let reader = reader(root.path())?;
    let ctx = ExecutionContext::new(
        &"\\\"".repeat(128),
        &"b".repeat(64),
        SystemTime::now() + Duration::from_secs(60),
        8,
    )?;
    let c = JevClient::new("ordinary-dummy", None)?.with_execution_context(ctx.clone());
    let mut op = paid("\\\"".repeat(64).as_str());
    op["query"] = json!("\\\n".repeat(4096));
    let mut r = request(json!([op]));
    r.budget.max_output_bytes = 5632;
    let p = prepare_rich_batch_with_reader(&reader, r, Some(c))?;
    assert_eq!(p.reserved_calls(), 0);
    let r = p.execute().await?;
    assert_eq!(
        r.items[0].error.as_ref().unwrap().code,
        RichBatchErrorCode::OutputBudgetInsufficient
    );
    assert!(serde_json::to_vec(&r)?.len() <= 5632);
    assert_eq!(ctx.snapshot()?.logical_reservations, 0);
    Ok(())
}
#[tokio::test]
async fn shared_manifest_failure_preserves_ordered_not_executed_items() -> Result<()> {
    let root = tempfile::tempdir()?;
    let reader = reader(root.path())?;
    let c = client(8)?;
    let ctx = c.execution_context().clone();
    let p = prepare_rich_batch_with_reader(
        &reader,
        request(json!([paid("rank"),{"id":"catalog","op":"inspect_documents"}])),
        Some(c),
    )?;
    fs::write(
        root.path()
            .join(".gptgrep/generations")
            .join(reader.pin().generation)
            .join("manifest.json"),
        b"corrupt",
    )?;
    let e = p.execute().await.unwrap_err();
    assert_eq!(e.error.code, RichBatchErrorCode::SharedIntegrity);
    assert_eq!(
        e.items.iter().map(|x| x.id.as_str()).collect::<Vec<_>>(),
        ["rank", "catalog"]
    );
    assert!(
        e.items
            .iter()
            .all(|x| x.status == RichBatchStatus::NotExecuted)
    );
    assert_eq!(ctx.snapshot()?.logical_reservations, 0);
    assert_eq!(e.execution.model_submissions, None);
    Ok(())
}
#[test]
fn visual_preparation_uses_real_reader_without_delivery_or_submission() -> Result<()> {
    let root = tempfile::tempdir()?;
    publish_visual(root.path(), 1)?;
    let reader = RichGenerationReader::open(root.path())?;
    let c = client(8)?;
    let ctx = c.execution_context().clone();
    let p = prepare_rich_batch_with_reader(&reader, request(json!([paid("visual")])), Some(c))?;
    assert!(p.starts[0].is_ok());
    assert!(p.reserved_calls() >= 2);
    assert!(
        p.searches[0]
            .as_ref()
            .unwrap()
            .candidate_scope()
            .any(|(_, c)| matches!(c, crate::RichMultimodalContent::Visual { .. }))
    );
    assert_eq!(ctx.snapshot()?.submission_observations, 0);
    Ok(())
}

// Compile these genuine loopback cases everywhere. A denied listener is an
// environment failure, not successful pipeline/provider qualification.
async fn mock(
    count: usize,
    fail: bool,
) -> Result<(String, tokio::task::JoinHandle<Result<Vec<Vec<u8>>>>)> {
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let endpoint = format!("http://{}/decisions", listener.local_addr()?);
    let handle = tokio::spawn(async move {
        let mut requests = Vec::new();
        for _ in 0..count {
            let (mut socket, _) =
                tokio::time::timeout(Duration::from_secs(10), listener.accept()).await??;
            let mut bytes = Vec::new();
            let (end, len) = loop {
                let mut buf = [0u8; 4096];
                let n = socket.read(&mut buf).await?;
                anyhow::ensure!(n > 0);
                bytes.extend_from_slice(&buf[..n]);
                if let Some(pos) = bytes.windows(4).position(|b| b == b"\r\n\r\n") {
                    let header = std::str::from_utf8(&bytes[..pos])?;
                    let len = header
                        .lines()
                        .find_map(|l| {
                            l.split_once(':')
                                .filter(|(k, _)| k.eq_ignore_ascii_case("content-length"))
                                .and_then(|(_, v)| v.trim().parse::<usize>().ok())
                        })
                        .unwrap();
                    break (pos + 4, len);
                }
            };
            while bytes.len() < end + len {
                let mut b = [0u8; 4096];
                let n = socket.read(&mut b).await?;
                anyhow::ensure!(n > 0);
                bytes.extend_from_slice(&b[..n]);
            }
            let body = bytes[end..end + len].to_vec();
            let v: Value = serde_json::from_slice(&body)?;
            let answers = v["questions"]
                .as_object()
                .unwrap()
                .keys()
                .map(|id| (id.clone(), json!({"type":"score","score":3.0})))
                .collect::<serde_json::Map<_, _>>();
            let response = serde_json::to_vec(
                &json!({"model":"openai/gpt-6-luna-decisions-20261006","provider":"ordinary-loopback","usage":{"input_tokens":12,"output_tokens":0,"cost":0.01},"answers":answers}),
            )?;
            let status = if fail { "502 Bad Gateway" } else { "200 OK" };
            socket
                .write_all(
                    format!(
                        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        response.len()
                    )
                    .as_bytes(),
                )
                .await?;
            socket.write_all(&response).await?;
            requests.push(body);
        }
        Ok(requests)
    });
    Ok((endpoint, handle))
}
#[tokio::test]
async fn loopback_paid_local_success_keeps_order_unique_calls_and_usage() -> Result<()> {
    let root = tempfile::tempdir()?;
    let reader = reader(root.path())?;
    let (endpoint, server) = mock(4, false).await?;
    let ctx = client(8)?.execution_context().clone();
    let c = JevClient::with_endpoint("ordinary-dummy", None, &endpoint)?
        .with_execution_context(ctx.clone());
    let r = dispatch_rich_batch_with_reader_and_client(
        &reader,
        request(json!([paid("first"),{"id":"catalog","op":"inspect_documents"},paid("second")])),
        Some(c),
    )
    .await?;
    let wires = server.await??;
    assert_eq!(wires.len(), 4);
    assert_eq!(ctx.snapshot()?.submission_observations, 4);
    assert!(r.items.iter().all(|x| x.status == RichBatchStatus::Ok));
    assert_eq!(r.execution.model_submissions, None);
    let m = r.metrics.decisions.as_ref().unwrap();
    assert_eq!(m.unique_scheduled_logical_attempts, 4);
    assert_eq!(m.observed_cost_subtotal_usd, Some(0.04));
    assert!(m.cost_complete);
    assert_eq!(wires[0], wires[2]);
    assert_eq!(wires[1], wires[3]);
    let a = &r.items[0].result.as_ref().unwrap()["attempts"];
    let b = &r.items[2].result.as_ref().unwrap()["attempts"];
    assert_ne!(a[0]["batch_attempt_id"], b[0]["batch_attempt_id"]);
    assert_eq!(
        r.items[0].result.as_ref().unwrap()["citation_status"],
        "model_exposure_only_delivery_unqualified"
    );
    assert!(serde_json::to_vec(&r)?.len() <= 1_048_576);
    Ok(())
}
#[tokio::test]
async fn loopback_provider_failure_keeps_receipt_stops_paid_and_continues_local() -> Result<()> {
    let root = tempfile::tempdir()?;
    let reader = reader(root.path())?;
    let (endpoint, server) = mock(1, true).await?;
    let ctx = client(8)?.execution_context().clone();
    let c = JevClient::with_endpoint("ordinary-dummy", None, &endpoint)?
        .with_execution_context(ctx.clone());
    let r = dispatch_rich_batch_with_reader_and_client(
        &reader,
        request(json!([paid("first"),paid("second"),{"id":"catalog","op":"inspect_documents"}])),
        Some(c),
    )
    .await?;
    assert_eq!(server.await??.len(), 1);
    assert_eq!(
        r.items.iter().map(|i| i.status).collect::<Vec<_>>(),
        [
            RichBatchStatus::Error,
            RichBatchStatus::NotExecuted,
            RichBatchStatus::Ok
        ]
    );
    assert_eq!(
        r.items[0].result.as_ref().unwrap()["attempts"][0]["failure"]["http_status"],
        502
    );
    let m = r.metrics.decisions.as_ref().unwrap();
    assert_eq!(m.unique_scheduled_logical_attempts, 1);
    // Non-success HTTP responses terminate before their body is read.
    // A cost in that unread body is unknown, not observed billing.
    assert_eq!(m.observed_cost_subtotal_usd, None);
    assert_eq!(m.observed_cost_attempts, 0);
    assert!(!m.cost_complete);
    assert!(
        r.items[0].result.as_ref().unwrap()["attempts"][0]["failure"]["reported_usage"].is_null()
    );
    assert_eq!(ctx.snapshot()?.logical_reservations, 1);
    Ok(())
}

fn rerank_operation(reader: &RichGenerationReader, id: &str) -> Result<Value> {
    let artifact = &reader.manifest()?.documents[0].pages[0].native_text.id;
    let evidence = reader.read_text(artifact, 0, 128)?.evidence;
    Ok(
        json!({"id":id,"op":"rerank","query":"ordinary source evidence",
        "candidates":[{"id":"output-side-label","evidence":evidence}]}),
    )
}
#[test]
fn rerank_schema_refuses_caller_content_cursors_dependencies_and_duplicate_evidence() -> Result<()>
{
    let root = tempfile::tempdir()?;
    let reader = reader(root.path())?;
    let operation = rerank_operation(&reader, "rank")?;
    assert!(
        parse_rich_batch_request(&serde_json::to_vec(&request(json!([operation.clone()])))?)
            .is_ok()
    );
    for field in ["cursor", "text", "image_url", "depends_on"] {
        let mut op = operation.clone();
        op[field] = json!("forbidden");
        assert!(
            parse_rich_batch_request(&serde_json::to_vec(
                &json!({"schema_version":RICH_BATCH_REQUEST_SCHEMA,"operations":[op]})
            )?)
            .is_err()
        );
    }
    let mut op = operation;
    let mut duplicate = op["candidates"][0].clone();
    duplicate["id"] = json!("another-label");
    op["candidates"].as_array_mut().unwrap().push(duplicate);
    let c = client(8)?;
    let ctx = c.execution_context().clone();
    let prepared = prepare_rich_batch_with_reader(&reader, request(json!([op])), Some(c))?;
    assert_eq!(
        prepared.starts[0].as_ref().unwrap_err().code,
        RichBatchErrorCode::InvalidOperation
    );
    assert_eq!(prepared.reserved_calls(), 0);
    assert_eq!(ctx.snapshot()?.logical_reservations, 0);
    Ok(())
}
#[test]
fn mixed_rerank_search_local_prepares_fixed_shares_and_actual_no_route_rerank() -> Result<()> {
    let root = tempfile::tempdir()?;
    let reader = reader(root.path())?;
    let c = client(8)?;
    let ctx = c.execution_context().clone();
    let p = prepare_rich_batch_with_reader(
        &reader,
        request(json!([
            rerank_operation(&reader,"rerank")?,{"id":"local","op":"inspect_documents"},paid("search")
        ])),
        Some(c),
    )?;
    assert_eq!(p.item_call_shares(), &[1, 0, 2]);
    assert_eq!(p.reserved_calls(), 3);
    assert_eq!(
        p.prepared_reranks().map(|(id, _)| id).collect::<Vec<_>>(),
        ["rerank"]
    );
    assert_eq!(
        p.prepared_searches().map(|(id, _)| id).collect::<Vec<_>>(),
        ["search"]
    );
    let (_, rerank) = p.prepared_reranks().next().unwrap();
    assert_eq!(rerank.requests().count(), 1);
    let body: Value = serde_json::from_slice(rerank.requests().next().unwrap().canonical_body())?;
    assert_eq!(body["questions"].as_object().unwrap().len(), 1);
    assert!(
        !std::str::from_utf8(rerank.requests().next().unwrap().canonical_body())?
            .contains("output-side-label")
    );
    assert!(
        p.prepared_searches()
            .all(|(_, s)| s.routing_requests().count() == 1)
    );
    assert_eq!(ctx.snapshot()?.logical_reservations, 0);
    Ok(())
}
#[test]
fn rerank_never_borrows_later_search_or_parent_call_allowances() -> Result<()> {
    let root = tempfile::tempdir()?;
    let reader = reader(root.path())?;
    let c = client(8)?;
    let ctx = c.execution_context().clone();
    let mut r = request(json!([rerank_operation(&reader, "rank")?, paid("search")]));
    r.budget.max_decisions_submissions = 1;
    let p = prepare_rich_batch_with_reader(&reader, r, Some(c))?;
    assert_eq!(p.item_call_shares(), &[1, 0]);
    assert_eq!(
        p.starts[1].as_ref().unwrap_err().code,
        RichBatchErrorCode::SubmissionBudgetInsufficient
    );
    assert_eq!(ctx.snapshot()?.logical_reservations, 0);
    Ok(())
}
#[test]
fn rerank_output_failure_and_forged_ref_do_not_borrow_other_item_capacity() -> Result<()> {
    let root = tempfile::tempdir()?;
    let reader = reader(root.path())?;
    let c = client(8)?;
    let ctx = c.execution_context().clone();
    let mut r =
        request(json!([rerank_operation(&reader,"rank")?,{"id":"local","op":"inspect_documents"}]));
    r.budget.max_output_bytes = 8192;
    let p = prepare_rich_batch_with_reader(&reader, r, Some(c.clone()))?;
    assert_eq!(p.reserved_calls(), 0);
    assert_eq!(
        p.starts[0].as_ref().unwrap_err().code,
        RichBatchErrorCode::OutputBudgetInsufficient
    );
    assert!(p.starts[1].is_ok());
    let mut bad = rerank_operation(&reader, "bad")?;
    bad["candidates"][0]["evidence"]["binding"]["source_sha256"] = json!("e".repeat(64));
    let p = prepare_rich_batch_with_reader(
        &reader,
        request(json!([bad,{"id":"local","op":"inspect_documents"}])),
        Some(c),
    )?;
    assert_eq!(p.reserved_calls(), 0);
    assert_eq!(
        p.starts[0].as_ref().unwrap_err().code,
        RichBatchErrorCode::InvalidOperation
    );
    assert!(p.starts[1].is_ok());
    assert_eq!(ctx.snapshot()?.logical_reservations, 0);
    Ok(())
}
#[tokio::test]
async fn local_sync_rerank_context_refusal_preserves_later_local_output() -> Result<()> {
    let root = tempfile::tempdir()?;
    let reader = reader(root.path())?;
    let r =
        request(json!([rerank_operation(&reader,"rank")?,{"id":"local","op":"inspect_documents"}]));
    let sync = dispatch_rich_batch_with_reader(&reader, r.clone())?;
    let asynchronous = dispatch_rich_batch_with_reader_and_client(&reader, r, None).await?;
    assert_eq!(
        sync.items[0].error.as_ref().unwrap().code,
        RichBatchErrorCode::ModelContextRequired
    );
    assert_eq!(
        asynchronous.items[0].error.as_ref().unwrap().code,
        RichBatchErrorCode::ModelContextRequired
    );
    assert_eq!(sync.items[1].result, asynchronous.items[1].result);
    assert_eq!(asynchronous.execution.model_submissions, None);
    Ok(())
}
