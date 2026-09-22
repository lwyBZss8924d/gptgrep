use super::*;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

struct Mock {
    client: JevClient,
    requests: Arc<Mutex<Vec<Value>>>,
    maximum_routes: Arc<AtomicUsize>,
    server: tokio::task::JoinHandle<()>,
}
impl Drop for Mock {
    fn drop(&mut self) {
        self.server.abort();
    }
}

async fn request(socket: &mut tokio::net::TcpStream) -> Value {
    let mut bytes = Vec::new();
    loop {
        let mut buffer = [0; 4096];
        let count = socket.read(&mut buffer).await.unwrap();
        assert!(count > 0);
        bytes.extend_from_slice(&buffer[..count]);
        assert!(bytes.len() <= 128 * 1024);
        if let Some(offset) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
            let headers = String::from_utf8_lossy(&bytes[..offset]).to_ascii_lowercase();
            let length = headers
                .lines()
                .find_map(|line| line.strip_prefix("content-length:"))
                .unwrap()
                .trim()
                .parse::<usize>()
                .unwrap();
            if bytes.len() >= offset + 4 + length {
                return serde_json::from_slice(&bytes[offset + 4..offset + 4 + length]).unwrap();
            }
        }
    }
}

fn evidence_request(body: &Value) -> bool {
    body["questions"]
        .as_object()
        .unwrap()
        .values()
        .next()
        .unwrap()["instructions"]["candidate"]["text"]
        .as_str()
        .unwrap()
        .starts_with("Document: ")
}

async fn mock(
    queries: &[&str],
    delays: [u64; 3],
    statuses: [u16; 4],
    final_score: f64,
    barrier: bool,
) -> Result<Mock> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let endpoint = format!("http://{}/api/alpha/decisions", listener.local_addr()?);
    let client = JevClient::with_endpoint("invented-core-test-key", None, &endpoint)?;
    let requests = Arc::new(Mutex::new(Vec::new()));
    let maximum_routes = Arc::new(AtomicUsize::new(0));
    let recorded = requests.clone();
    let maximum = maximum_routes.clone();
    let queries: Vec<_> = queries.iter().map(|query| query.to_string()).collect();
    let server = tokio::spawn(async move {
        let active = Arc::new(AtomicUsize::new(0));
        let barrier = barrier.then(|| Arc::new(tokio::sync::Barrier::new(2)));
        let mut tasks = tokio::task::JoinSet::new();
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            let recorded = recorded.clone();
            let queries = queries.clone();
            let active = active.clone();
            let maximum = maximum.clone();
            let barrier = barrier.clone();
            tasks.spawn(async move {
                let body = request(&mut socket).await;
                let evidence = evidence_request(&body);
                let ordinal = if evidence { 3 } else {
                    queries.iter().position(|query| body["state"]["query"].as_str() == Some(query.as_str())).unwrap()
                };
                recorded.lock().unwrap().push(body.clone());
                if !evidence {
                    let active_count = active.fetch_add(1, Ordering::SeqCst) + 1;
                    maximum.fetch_max(active_count, Ordering::SeqCst);
                    if ordinal < 2 && let Some(barrier) = barrier { barrier.wait().await; }
                    tokio::time::sleep(Duration::from_millis(delays[ordinal])).await;
                }
                if statuses[ordinal] == 0 { std::future::pending::<()>().await; }
                let response = if statuses[ordinal] == 200 {
                    let answers: serde_json::Map<String, Value> = body["questions"].as_object().unwrap().keys()
                        .map(|id| (id.clone(), json!({"type":"score", "score":if evidence { final_score } else { 3.0 }}))).collect();
                    json!({"model":format!("typesafe/invented-{ordinal}"), "answers":answers,
                        "id":format!("response-{ordinal}"), "provider":"invented-provider",
                        "usage":{"input_tokens":100+ordinal,"output_tokens":2,"cost":0.001*(ordinal+1) as f64}}).to_string()
                } else { "private-provider-body-sentinel".into() };
                if !evidence { active.fetch_sub(1, Ordering::SeqCst); }
                let wire = format!("HTTP/1.1 {} Mock\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}", statuses[ordinal], response.len());
                let _ = socket.write_all(wire.as_bytes()).await;
            });
        }
    });
    Ok(Mock {
        client,
        requests,
        maximum_routes,
        server,
    })
}

async fn fixture(
    name: &str,
    eol: &str,
    reverse: bool,
) -> Result<(tempfile::TempDir, String, String)> {
    let directory = tempfile::tempdir()?;
    if let Some(parent) = Path::new(name).parent() {
        fs::create_dir_all(directory.path().join(parent))?;
    }
    let positions = if reverse { [80, 42, 8] } else { [8, 42, 80] };
    let lines: Vec<_> = (0..100)
        .map(|index| {
            if index == positions[0] {
                "cobalt stores seven painted pebbles.".into()
            } else if index == positions[1] {
                "黄鹂 carries eleven woven ribbons.".into()
            } else if index == positions[2] {
                "zephyr keeps thirteen paper lanterns.".into()
            } else {
                format!("Quiet river stones {index:03} surround café branches under cloud cover.")
            }
        })
        .collect();
    let text = lines.join(eol) + eol;
    fs::write(directory.path().join(name), &text)?;
    fs::write(
        directory.path().join("outside-scope.txt"),
        "cobalt 黄鹂 zephyr",
    )?;
    let generation = index(directory.path(), 10).await?.generation;
    Ok((directory, generation, text))
}

fn options(name: &str) -> SearchOptions {
    SearchOptions {
        document: Some(name.into()),
        limit: 24,
        ..SearchOptions::default()
    }
}

fn event_sink(
    events: Arc<Mutex<Vec<PlannedSearchEvent>>>,
) -> impl Fn(&PlannedSearchEvent) -> Result<()> + Send + Sync {
    move |event| {
        events.lock().unwrap().push(event.clone());
        Ok(())
    }
}

#[tokio::test]
async fn three_views_have_four_calls_two_routes_and_stable_union_in_reverse_completion_order()
-> Result<()> {
    let question = " \tcobalt  ";
    let queries = [question, "黄鹂", "zephyr"];
    let (directory, generation, text) = fixture("目录/notes.txt", "\r\n", false).await?;
    let mut selected = None;
    let mut final_hits = None;
    for delays in [[90, 10, 10], [10, 90, 10]] {
        let mock = mock(&queries, delays, [200; 4], 3.0, true).await?;
        let events = Arc::new(Mutex::new(Vec::new()));
        let report = search_planned_with_client_and_observer(
            directory.path(),
            question,
            &["黄鹂".into(), "zephyr".into()],
            &options("目录/notes.txt"),
            &generation,
            &mock.client,
            &event_sink(events.clone()),
        )
        .await?;
        assert_eq!(mock.maximum_routes.load(Ordering::SeqCst), 2);
        assert_eq!(
            (
                report.metrics.jev_calls_attempted,
                report.metrics.jev_requests
            ),
            (4, 4)
        );
        assert_eq!(report.operations.len(), 4);
        assert_eq!(report.coverage.indexed_files, 2);
        assert_eq!(report.coverage.scoped_files, 1);
        assert_eq!(report.coverage.views.len(), 3);
        assert_eq!(report.coverage.candidates_before_dedup, Some(3));
        assert_eq!(report.coverage.unique_candidates, Some(3));
        assert_eq!(report.coverage.union_candidates, Some(3));
        assert_eq!(report.hits.len(), 3);
        assert_eq!(
            report
                .hits
                .iter()
                .map(|hit| &hit.node_id)
                .collect::<HashSet<_>>()
                .len(),
            1
        );
        for hit in &report.hits {
            assert_eq!(hit.path, "目录/notes.txt");
            assert_eq!(hit.text, text[hit.byte_start..hit.byte_end]);
            assert!(hit.text.contains("\r\n"));
        }
        assert!(report.hits.iter().any(|hit| hit.text.contains("黄鹂")));
        let requests = mock.requests.lock().unwrap();
        assert_eq!(requests.len(), 4);
        let evidence: Vec<_> = requests
            .iter()
            .filter(|body| evidence_request(body))
            .collect();
        assert_eq!(evidence.len(), 1);
        assert_eq!(evidence[0]["state"]["query"], question);
        assert_eq!(evidence[0]["questions"].as_object().unwrap().len(), 3);
        assert!(
            requests
                .iter()
                .all(|body| !body.to_string().contains("outside-scope.txt"))
        );
        for (index, operation) in report.operations.iter().enumerate() {
            assert_eq!(operation.event, "finished");
            assert_eq!(
                (
                    operation.metrics.jev_calls_attempted,
                    operation.metrics.jev_requests
                ),
                (1, 1)
            );
            assert_eq!(
                operation.provider_response_id,
                Some(format!("response-{index}"))
            );
            assert_eq!(operation.provider.as_deref(), Some("invented-provider"));
            assert_eq!(
                operation.original_question_sha256,
                digest(question.as_bytes())
            );
        }
        assert!(report.operations[3].coverage.is_none());
        let history = events.lock().unwrap();
        assert_eq!(history.len(), 16);
        for operation in &report.operations {
            let sequence: Vec<_> = history
                .iter()
                .filter(|event| event.operation_id == operation.operation_id)
                .map(|event| event.event.as_str())
                .collect();
            assert_eq!(
                sequence,
                ["admitted", "before_call", "after_reply", "finished"]
            );
        }
        if let Some(previous) = &selected {
            assert_eq!(previous, &report.coverage.selected_spans);
        }
        if let Some(previous) = &final_hits {
            assert_eq!(previous, &serde_json::to_value(&report.hits)?);
        }
        selected = Some(report.coverage.selected_spans);
        final_hits = Some(serde_json::to_value(&report.hits)?);
    }
    Ok(())
}

#[tokio::test]
async fn original_only_anchor_and_diverse_windows_survive_file_placement_unicode_and_line_endings()
-> Result<()> {
    for (name, eol, reverse) in [
        ("renamed.txt", "\n", false),
        ("目录/移位.txt", "\r\n", true),
    ] {
        let (directory, generation, text) = fixture(name, eol, reverse).await?;
        let mock = mock(&["cobalt", "黄鹂", "zephyr"], [0; 3], [200; 4], 0.0, false).await?;
        let report = search_planned_with_client_and_observer(
            directory.path(),
            "cobalt",
            &["黄鹂".into(), "zephyr".into()],
            &options(name),
            &generation,
            &mock.client,
            &|_| Ok(()),
        )
        .await?;
        assert_eq!(report.coverage.unique_candidates, Some(3));
        assert_eq!(report.coverage.filtered_candidates, 2);
        assert_eq!(report.coverage.retained_literal_anchors, 1);
        assert_eq!(report.hits.len(), 1);
        assert!(report.hits[0].literal_anchor);
        assert_eq!(report.hits[0].score, 0.0);
        assert!(report.hits[0].text.contains("cobalt"));
        assert_eq!(
            report.hits[0].text,
            text[report.hits[0].byte_start..report.hits[0].byte_end]
        );
        let body = mock
            .requests
            .lock()
            .unwrap()
            .iter()
            .find(|body| evidence_request(body))
            .unwrap()
            .clone();
        let candidate_text = body["questions"]
            .as_object()
            .unwrap()
            .values()
            .map(|question| question.to_string())
            .collect::<String>();
        assert!(candidate_text.contains("黄鹂") && candidate_text.contains("zephyr"));
    }
    Ok(())
}

#[tokio::test]
async fn zero_alternatives_preserves_default_candidates_and_scoring() -> Result<()> {
    let (directory, generation, _) = fixture("notes.txt", "\n", false).await?;
    let plain = mock(&["cobalt"], [0; 3], [200; 4], 0.0, false).await?;
    let default_report = search_with_client(
        directory.path(),
        "cobalt",
        &options("notes.txt"),
        &plain.client,
    )
    .await?;
    let planned = mock(&["cobalt"], [0; 3], [200; 4], 0.0, false).await?;
    let planned_report = search_planned_with_client_and_observer(
        directory.path(),
        "cobalt",
        &[],
        &options("notes.txt"),
        &generation,
        &planned.client,
        &|_| Ok(()),
    )
    .await?;
    assert_eq!(
        serde_json::to_value(default_report.hits)?,
        serde_json::to_value(planned_report.hits)?
    );
    assert_eq!(default_report.metrics.jev_requests, 2);
    assert_eq!(planned_report.metrics.jev_requests, 2);
    assert_eq!(
        *plain.requests.lock().unwrap(),
        *planned.requests.lock().unwrap()
    );
    assert_eq!(planned_report.coverage.views.len(), 1);
    Ok(())
}

fn span(offset: usize) -> Hit {
    Hit {
        path: "invented.txt".into(),
        node_id: "same-node".into(),
        title: "Invented".into(),
        line_start: 1,
        line_end: 1,
        page_start: 1,
        page_end: 1,
        match_line: None,
        match_column: None,
        byte_start: offset,
        byte_end: offset + 1,
        node_offset: Some(offset),
        next_offset: Some(offset + 1),
        column_start: offset + 1,
        coordinate_system: "source_lines".into(),
        text: "x".into(),
        text_truncated: true,
        score: 1.0,
        confidence: None,
        literal_anchor: true,
        source_sha256: digest(b"invented-source"),
        source_fresh: true,
        citation: "invented.txt:L1-L1".into(),
    }
}

fn batch(hits: Vec<Hit>) -> CandidateBatch {
    CandidateBatch {
        hits,
        coverage: Coverage::default(),
        warnings: Vec::new(),
    }
}
fn view(ordinal: usize, candidates: usize) -> PlannedViewCoverage {
    PlannedViewCoverage {
        view_id: format!("q{ordinal}"),
        query_sha256: String::new(),
        coverage: Coverage::default(),
        collected_candidates: candidates,
        union_candidates: None,
        omitted_candidates: None,
    }
}

#[test]
fn span_identity_merges_provenance_checks_conflicts_and_caps_round_robin() -> Result<()> {
    let mut duplicate = span(0);
    duplicate.node_id = "alternate-valid-node".into();
    let batches = [
        (0, batch(vec![span(0), span(1)])),
        (1, batch(vec![duplicate, span(2)])),
    ];
    let mut coverage = PlannedCoverage {
        views: vec![view(0, 2), view(1, 2)],
        ..Default::default()
    };
    let union = merge_candidates("generation", &batches, 24, &mut coverage)?;
    assert_eq!(union.hits.len(), 3);
    assert_eq!(union.hits[0].node_id, "same-node");
    assert_eq!(coverage.selected_spans[0].view_ids, ["q0", "q1"]);
    assert!(
        !union
            .hits
            .iter()
            .find(|hit| hit.byte_start == 2)
            .unwrap()
            .literal_anchor
    );
    let mut conflict = span(0);
    conflict.text = "y".into();
    assert!(
        merge_candidates(
            "generation",
            &[(0, batch(vec![span(0)])), (1, batch(vec![conflict]))],
            24,
            &mut PlannedCoverage::default()
        )
        .is_err()
    );
    let mut different_source = span(0);
    different_source.source_sha256 = digest(b"second-source");
    let mut different_path = span(0);
    different_path.path = "another.txt".into();
    let union = merge_candidates(
        "generation",
        &[(0, batch(vec![span(0), different_source, different_path]))],
        24,
        &mut PlannedCoverage::default(),
    )?;
    assert_eq!(union.hits.len(), 3);
    assert_ne!(
        SpanKey::new("generation-a", &span(0)),
        SpanKey::new("generation-b", &span(0))
    );
    let batches: Vec<_> = (0..3)
        .map(|ordinal| {
            (
                ordinal,
                batch((ordinal * 24..(ordinal + 1) * 24).map(span).collect()),
            )
        })
        .collect();
    let mut coverage = PlannedCoverage {
        views: (0..3).map(|ordinal| view(ordinal, 24)).collect(),
        ..Default::default()
    };
    let union = merge_candidates("generation", &batches, 24, &mut coverage)?;
    assert_eq!(coverage.candidates_before_dedup, Some(72));
    assert_eq!(coverage.unique_candidates, Some(72));
    assert_eq!(coverage.union_candidates, Some(24));
    assert_eq!(coverage.omitted_unique_candidates, Some(48));
    assert!(coverage.truncated);
    assert!(
        coverage
            .views
            .iter()
            .all(|view| view.union_candidates == Some(8) && view.omitted_candidates == Some(16))
    );
    assert_eq!(
        union
            .hits
            .iter()
            .take(6)
            .map(|hit| hit.byte_start)
            .collect::<Vec<_>>(),
        [0, 24, 48, 1, 25, 49]
    );
    Ok(())
}

#[test]
fn same_view_duplicate_spans_count_once_and_do_not_become_cap_omissions() -> Result<()> {
    for (limit, expected_selected, expected_omitted) in [(24, 3, 0), (1, 1, 2)] {
        let mut duplicate = span(0);
        duplicate.node_id = "different-node-same-span".into();
        let mut omitted_duplicate = span(1);
        omitted_duplicate.node_id = "second-node-same-span".into();
        let batches = [(
            0,
            batch(vec![
                span(0),
                duplicate,
                span(1),
                omitted_duplicate,
                span(2),
            ]),
        )];
        let mut coverage = PlannedCoverage {
            views: vec![view(0, 5)],
            ..Default::default()
        };
        let union = merge_candidates("generation", &batches, limit, &mut coverage)?;
        assert_eq!(coverage.candidates_before_dedup, Some(5));
        assert_eq!(coverage.unique_candidates, Some(3));
        assert_eq!(coverage.union_candidates, Some(expected_selected));
        assert_eq!(coverage.omitted_unique_candidates, Some(expected_omitted));
        assert_eq!(coverage.views[0].collected_candidates, 5);
        assert_eq!(coverage.views[0].union_candidates, Some(expected_selected));
        assert_eq!(coverage.views[0].omitted_candidates, Some(expected_omitted));
        assert_eq!(union.hits.len(), expected_selected);
        assert_eq!(union.hits[0].node_id, "same-node");
        assert_eq!(coverage.selected_spans[0].view_ids, ["q0"]);
    }
    Ok(())
}

#[tokio::test]
async fn route_failure_retains_completed_sibling_usage_and_interrupts_pending_sibling() -> Result<()>
{
    let (directory, generation, _) = fixture("notes.txt", "\n", false).await?;
    let mock = mock(
        &["cobalt", "黄鹂", "zephyr"],
        [10, 90, 0],
        [200, 503, 0, 200],
        3.0,
        true,
    )
    .await?;
    let events = Arc::new(Mutex::new(Vec::new()));
    let error = search_planned_with_client_and_observer(
        directory.path(),
        "cobalt",
        &["黄鹂".into(), "zephyr".into()],
        &options("notes.txt"),
        &generation,
        &mock.client,
        &event_sink(events.clone()),
    )
    .await
    .unwrap_err();
    let failure = error.downcast_ref::<PlannedSearchError>().unwrap();
    assert_eq!(failure.stage, "q1.route");
    assert!(failure.cause.contains("HTTP 503"));
    assert_eq!(
        (
            failure.metrics.jev_calls_attempted,
            failure.metrics.jev_requests
        ),
        (3, 1)
    );
    assert_eq!(
        failure
            .operations
            .iter()
            .map(|event| event.event.as_str())
            .collect::<Vec<_>>(),
        ["finished", "failed", "interrupted"]
    );
    assert_eq!(
        failure.operations[0].provider_response_id.as_deref(),
        Some("response-0")
    );
    assert_eq!(
        failure.metrics.jev_usage,
        [json!({"input_tokens":100,"output_tokens":2,"cost":0.001})]
    );
    assert_eq!(failure.coverage.views.len(), 1);
    assert_eq!(failure.coverage.union_candidates, None);
    assert!(serde_json::to_value(failure)?["coverage"]["unique_candidates"].is_null());
    assert_eq!(mock.requests.lock().unwrap().len(), 3);
    assert!(!serde_json::to_string(failure)?.contains("private-provider-body-sentinel"));
    let latest: BTreeMap<_, _> = events
        .lock()
        .unwrap()
        .iter()
        .map(|event| (event.operation_id.clone(), event.clone()))
        .collect();
    assert_eq!(
        serde_json::to_value(latest.values().collect::<Vec<_>>())?,
        serde_json::to_value(&failure.operations)?
    );
    Ok(())
}

#[tokio::test]
async fn observer_failure_keeps_reply_and_prevents_fusion_admission() -> Result<()> {
    let (directory, generation, _) = fixture("notes.txt", "\n", false).await?;
    let mock = mock(&["cobalt", "黄鹂", "zephyr"], [0; 3], [200; 4], 3.0, false).await?;
    let observer = |event: &PlannedSearchEvent| {
        if event.operation_id == "q0.route" && event.event == "after_reply" {
            bail!("private-ledger-sentinel");
        }
        Ok(())
    };
    let error = search_planned_with_client_and_observer(
        directory.path(),
        "cobalt",
        &[],
        &options("notes.txt"),
        &generation,
        &mock.client,
        &observer,
    )
    .await
    .unwrap_err();
    let failure = error.downcast_ref::<PlannedSearchError>().unwrap();
    assert_eq!(
        (
            failure.metrics.jev_calls_attempted,
            failure.metrics.jev_requests
        ),
        (1, 1)
    );
    assert_eq!(failure.operations[0].event, "failed");
    assert_eq!(failure.metrics.jev_usage[0]["input_tokens"], 100);
    assert_eq!(mock.requests.lock().unwrap().len(), 1);
    assert!(!serde_json::to_string(failure)?.contains("private-ledger-sentinel"));
    Ok(())
}

#[tokio::test]
async fn caller_cancellation_records_all_admitted_pending_operations_and_prior_usage() -> Result<()>
{
    let (directory, generation, _) = fixture("notes.txt", "\n", false).await?;
    let mock = mock(
        &["cobalt", "黄鹂", "zephyr"],
        [10, 0, 0],
        [200, 0, 0, 200],
        3.0,
        true,
    )
    .await?;
    let events = Arc::new(Mutex::new(Vec::new()));
    let recorded = events.clone();
    let (sender, receiver) = tokio::sync::oneshot::channel();
    let sender = Mutex::new(Some(sender));
    let observer = move |event: &PlannedSearchEvent| {
        recorded.lock().unwrap().push(event.clone());
        if event.operation_id == "q2.route"
            && event.event == "before_call"
            && let Some(sender) = sender.lock().unwrap().take()
        {
            let _ = sender.send(());
        }
        Ok(())
    };
    let options = options("notes.txt");
    let alternatives = ["黄鹂".into(), "zephyr".into()];
    tokio::time::timeout(Duration::from_secs(5), async {
        tokio::select! {
            result = search_planned_with_client_and_observer(directory.path(), "cobalt", &alternatives, &options, &generation, &mock.client, &observer) => panic!("unexpected completed plan: {result:?}"),
            result = receiver => result.unwrap(),
        }
    }).await?;
    let latest: BTreeMap<_, _> = events
        .lock()
        .unwrap()
        .iter()
        .map(|event| (event.operation_id.clone(), event.clone()))
        .collect();
    assert_eq!(latest.len(), 3);
    assert_eq!(latest["q0.route"].event, "finished");
    assert_eq!(latest["q1.route"].event, "interrupted");
    assert_eq!(latest["q2.route"].event, "interrupted");
    assert_eq!(
        latest
            .values()
            .map(|event| event.metrics.jev_requests)
            .sum::<usize>(),
        1
    );
    assert_eq!(latest["q0.route"].metrics.jev_usage[0]["input_tokens"], 100);
    assert!(!latest["q2.route"].accounting_complete);
    Ok(())
}

#[tokio::test]
async fn generation_is_checked_before_candidates_after_routes_and_before_delivery() -> Result<()> {
    for trigger in ["initial", "q0.route", "union.rerank"] {
        let (directory, original_generation, _) = fixture("notes.txt", "\n", false).await?;
        let pointer = directory.path().join(STATE).join("CURRENT.json");
        let original_pointer = fs::read(&pointer)?;
        index(directory.path(), 10).await?;
        let replacement_pointer = fs::read(&pointer)?;
        if trigger != "initial" {
            fs::write(&pointer, &original_pointer)?;
        }
        let mock = mock(&["cobalt"], [0; 3], [200; 4], 3.0, false).await?;
        let observer = |event: &PlannedSearchEvent| {
            if event.operation_id == trigger && event.event == "after_reply" {
                fs::write(&pointer, &replacement_pointer)?;
            }
            Ok(())
        };
        let error = search_planned_with_client_and_observer(
            directory.path(),
            "cobalt",
            &[],
            &options("notes.txt"),
            &original_generation,
            &mock.client,
            &observer,
        )
        .await
        .unwrap_err();
        let failure = error.downcast_ref::<PlannedSearchError>().unwrap();
        assert!(failure.cause.contains("generation changed"));
        assert_eq!(
            failure.metrics.jev_requests,
            match trigger {
                "initial" => 0,
                "q0.route" => 1,
                _ => 2,
            }
        );
        assert_eq!(
            failure.coverage.reranked_candidates,
            usize::from(trigger == "union.rerank")
        );
    }
    Ok(())
}

#[tokio::test]
async fn stale_source_is_excluded_at_start_and_rechecked_after_final_reply() -> Result<()> {
    for trigger in ["initial", "union.rerank"] {
        let (directory, generation, _) = fixture("notes.txt", "\n", false).await?;
        let source = directory.path().join("notes.txt");
        if trigger == "initial" {
            fs::write(&source, "changed")?;
        }
        let mock = mock(&["cobalt"], [0; 3], [200; 4], 3.0, false).await?;
        let observer = |event: &PlannedSearchEvent| {
            if event.operation_id == trigger && event.event == "after_reply" {
                fs::write(&source, "changed")?;
            }
            Ok(())
        };
        let result = search_planned_with_client_and_observer(
            directory.path(),
            "cobalt",
            &[],
            &options("notes.txt"),
            &generation,
            &mock.client,
            &observer,
        )
        .await;
        if trigger == "initial" {
            let error = result.unwrap_err();
            let failure = error.downcast_ref::<PlannedSearchError>().unwrap();
            assert_eq!(failure.metrics.jev_calls_attempted, 0);
            assert!(failure.cause.contains("no candidates"));
            assert_eq!(
                failure.coverage.views[0].coverage.stale_files,
                ["notes.txt"]
            );
        } else {
            let report = result?;
            assert!(report.hits.is_empty());
            assert_eq!(report.source_fresh, None);
            assert_eq!(report.coverage.stale_files, ["notes.txt"]);
            assert_eq!(report.metrics.jev_requests, 2);
        }
    }
    Ok(())
}

#[tokio::test]
async fn invalid_plan_bounds_are_rejected_without_call_admission() -> Result<()> {
    let (directory, generation, _) = fixture("notes.txt", "\n", false).await?;
    let client = JevClient::with_endpoint("invented-key", None, "http://127.0.0.1:1/decisions")?;
    let events = Arc::new(Mutex::new(Vec::new()));
    for alternatives in [
        vec!["a".into(), "b".into(), "c".into()],
        vec![" \t".into()],
        vec!["文".repeat(342)],
        vec![" cobalt ".into()],
        vec!["a".into(), " a ".into()],
    ] {
        assert!(
            search_planned_with_client_and_observer(
                directory.path(),
                "cobalt",
                &alternatives,
                &options("notes.txt"),
                &generation,
                &client,
                &event_sink(events.clone())
            )
            .await
            .is_err()
        );
    }
    let invalid = SearchOptions {
        max_candidates: 25,
        ..options("notes.txt")
    };
    assert!(
        search_planned_with_client_and_observer(
            directory.path(),
            "cobalt",
            &[],
            &invalid,
            &generation,
            &client,
            &event_sink(events.clone())
        )
        .await
        .is_err()
    );
    assert!(events.lock().unwrap().is_empty());
    Ok(())
}

#[tokio::test]
async fn escaped_union_payload_obeys_jev_byte_cap_and_keeps_route_usage() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let text: String = (0..24)
        .map(|index| format!("# Invented Section {index}\n{}\n", "\"\\".repeat(750)))
        .collect();
    fs::write(directory.path().join("quoted.md"), text)?;
    let generation = index(directory.path(), 10).await?.generation;
    let mock = mock(&["cobalt", "黄鹂", "zephyr"], [0; 3], [200; 4], 3.0, false).await?;
    let error = search_planned_with_client_and_observer(
        directory.path(),
        "cobalt",
        &["黄鹂".into(), "zephyr".into()],
        &options("quoted.md"),
        &generation,
        &mock.client,
        &|_| Ok(()),
    )
    .await
    .unwrap_err();
    let failure = error.downcast_ref::<PlannedSearchError>().unwrap();
    assert_eq!(failure.stage, "union.rerank");
    assert!(failure.cause.contains("byte limit"));
    assert_eq!(
        (
            failure.metrics.jev_calls_attempted,
            failure.metrics.jev_requests
        ),
        (4, 3)
    );
    assert_eq!(failure.coverage.union_candidates, Some(24));
    assert_eq!(mock.requests.lock().unwrap().len(), 3);
    assert!(
        mock.requests
            .lock()
            .unwrap()
            .iter()
            .all(|body| !evidence_request(body))
    );
    Ok(())
}

#[tokio::test]
async fn final_rerank_failure_and_cancellation_retain_all_route_receipts_once() -> Result<()> {
    for status in [503, 0] {
        let (directory, generation, _) = fixture("notes.txt", "\n", false).await?;
        let mock = mock(
            &["cobalt", "黄鹂", "zephyr"],
            [0; 3],
            [200, 200, 200, status],
            3.0,
            false,
        )
        .await?;
        let events = Arc::new(Mutex::new(Vec::new()));
        let recorded = events.clone();
        let (sender, receiver) = tokio::sync::oneshot::channel();
        let sender = Mutex::new(Some(sender));
        let observer = move |event: &PlannedSearchEvent| {
            recorded.lock().unwrap().push(event.clone());
            if event.operation_id == "union.rerank"
                && event.event == "before_call"
                && let Some(sender) = sender.lock().unwrap().take()
            {
                let _ = sender.send(());
            }
            Ok(())
        };
        let options = options("notes.txt");
        let alternatives = ["黄鹂".into(), "zephyr".into()];
        if status == 503 {
            let error = search_planned_with_client_and_observer(
                directory.path(),
                "cobalt",
                &alternatives,
                &options,
                &generation,
                &mock.client,
                &observer,
            )
            .await
            .unwrap_err();
            let failure = error.downcast_ref::<PlannedSearchError>().unwrap();
            assert_eq!(failure.stage, "union.rerank");
            assert_eq!(
                (
                    failure.metrics.jev_calls_attempted,
                    failure.metrics.jev_requests
                ),
                (4, 3)
            );
            assert_eq!(failure.coverage.union_candidates, Some(3));
            assert!(failure.cause.contains("HTTP 503"));
            assert_eq!(mock.requests.lock().unwrap().len(), 4);
        } else {
            tokio::time::timeout(Duration::from_secs(5), async {
                tokio::select! {
                    result = search_planned_with_client_and_observer(directory.path(), "cobalt", &alternatives, &options, &generation, &mock.client, &observer) => panic!("unexpected completed plan: {result:?}"),
                    result = receiver => result.unwrap(),
                }
            }).await?;
        }
        let latest: BTreeMap<_, _> = events
            .lock()
            .unwrap()
            .iter()
            .map(|event| (event.operation_id.clone(), event.clone()))
            .collect();
        assert_eq!(latest.len(), 4);
        assert_eq!(
            latest["union.rerank"].event,
            if status == 503 {
                "failed"
            } else {
                "interrupted"
            }
        );
        assert_eq!(
            latest
                .values()
                .map(|event| event.metrics.jev_calls_attempted)
                .sum::<usize>(),
            4
        );
        assert_eq!(
            latest
                .values()
                .map(|event| event.metrics.jev_requests)
                .sum::<usize>(),
            3
        );
        assert_eq!(
            latest
                .values()
                .flat_map(|event| &event.metrics.jev_usage)
                .count(),
            3
        );
        assert!(latest["union.rerank"].provider_response_id.is_none());
    }
    Ok(())
}

#[tokio::test]
async fn before_call_observer_failure_counts_admission_without_claiming_a_provider_reply()
-> Result<()> {
    let (directory, generation, _) = fixture("notes.txt", "\n", false).await?;
    let mock = mock(&["cobalt"], [0; 3], [200; 4], 3.0, false).await?;
    let observer = |event: &PlannedSearchEvent| {
        if event.event == "before_call" {
            bail!("private-ledger-sentinel");
        }
        Ok(())
    };
    let error = search_planned_with_client_and_observer(
        directory.path(),
        "cobalt",
        &[],
        &options("notes.txt"),
        &generation,
        &mock.client,
        &observer,
    )
    .await
    .unwrap_err();
    let failure = error.downcast_ref::<PlannedSearchError>().unwrap();
    assert_eq!(
        (
            failure.metrics.jev_calls_attempted,
            failure.metrics.jev_requests
        ),
        (1, 0)
    );
    assert_eq!(failure.operations.len(), 1);
    assert_eq!(failure.operations[0].event, "failed");
    assert!(failure.metrics.jev_usage.is_empty());
    assert!(mock.requests.lock().unwrap().is_empty());
    assert!(!failure.accounting_complete);
    Ok(())
}
