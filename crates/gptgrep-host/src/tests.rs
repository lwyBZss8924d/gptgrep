use super::*;
#[path = "mandatory_tests.rs"]
mod mandatory;
#[path = "protocol_error_tests.rs"]
mod protocol_errors;
#[path = "source_continuation_tests.rs"]
mod source_continuation;
#[path = "tool_budget_tests.rs"]
mod tool_budget;
use crate::{protocol, retrieval::Evidence};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream, ReadHalf, WriteHalf};

#[test]
fn service_tier_configuration_preserves_supported_values() {
    assert_eq!(HostConfig::default().service_tier, "fast");
    for tier in ["fast", "priority", "flex", "default"] {
        let config = HostConfig {
            service_tier: tier.into(),
            ..HostConfig::default()
        };
        validate_config(&config, "query").unwrap();
        assert_eq!(config.service_tier, tier);
    }
    for tier in ["", "auto", "scale", "FAST", " fast", "fast\n"] {
        let config = HostConfig {
            service_tier: tier.into(),
            ..HostConfig::default()
        };
        assert!(validate_config(&config, "query").is_err());
    }
}

#[tokio::test]
async fn service_tier_acknowledgement_preserves_absence_and_fast_aliases() {
    let cwd = tempfile::tempdir().unwrap();
    for (requested, reported) in [
        ("fast", None),
        ("fast", Some(Value::Null)),
        ("fast", Some(json!("priority"))),
        ("fast", Some(json!("fast"))),
        ("priority", Some(json!("fast"))),
        ("flex", Some(json!("flex"))),
        ("default", Some(json!("default"))),
    ] {
        let expected = reported.as_ref().and_then(Value::as_str).map(str::to_owned);
        let (client, server) = tokio::io::duplex(65536);
        let (reader, writer) = tokio::io::split(client);
        let (server_reader, mut server_writer) = tokio::io::split(server);
        let server = tokio::spawn(async move {
            let mut reader = BufReader::new(server_reader);
            handshake_thread(&mut reader, &mut server_writer, false, requested, reported).await;
            handshake_turn(&mut reader, &mut server_writer, false, requested).await;
            send(&mut server_writer, json!({"method":"turn/completed","params":{
                "threadId":"thread-native","turn":{"id":"turn-native","status":"completed","items":[
                {"type":"agentMessage","phase":"final_answer","text":"{}"}]}
            }})).await;
        });
        let config = HostConfig {
            service_tier: requested.into(),
            ..HostConfig::default()
        };
        let state = json!({});
        let schema = json!({});
        let outcome = protocol::run(
            BufReader::new(reader),
            writer,
            cwd.path(),
            &config,
            protocol::Workflow::Completion {
                instructions: "Return an empty object.",
                state: &state,
                schema: &schema,
            },
            None,
        )
        .await
        .unwrap();
        server.await.unwrap();
        assert_eq!(outcome.service_tier, expected);
        assert_eq!(
            outcome
                .warnings
                .iter()
                .any(|warning| warning.contains("did not report an effective thread service tier")),
            expected.is_none()
        );
    }
}

#[tokio::test]
async fn changed_or_malformed_service_tier_stops_before_inference_without_fallback() {
    let cwd = tempfile::tempdir().unwrap();
    for reported in [
        json!("default"),
        json!("flex"),
        json!(""),
        json!(true),
        json!([]),
    ] {
        let (client, server) = tokio::io::duplex(65536);
        let (reader, writer) = tokio::io::split(client);
        let (server_reader, mut server_writer) = tokio::io::split(server);
        let server = tokio::spawn(async move {
            let mut reader = BufReader::new(server_reader);
            handshake_thread(
                &mut reader,
                &mut server_writer,
                false,
                "fast",
                Some(reported),
            )
            .await;
            let mut next = String::new();
            assert_eq!(
                reader.read_line(&mut next).await.unwrap(),
                0,
                "Unexpected inference or fallback request: {next}"
            );
        });
        let state = json!({});
        let schema = json!({});
        let error = protocol::run(
            BufReader::new(reader),
            writer,
            cwd.path(),
            &HostConfig::default(),
            protocol::Workflow::Completion {
                instructions: "Return an empty object.",
                state: &state,
                schema: &schema,
            },
            None,
        )
        .await
        .err()
        .unwrap();
        assert!(error.to_string().contains("service tier"), "{error}");
        server.await.unwrap();
    }
}

#[tokio::test]
async fn broader_effective_permissions_are_rejected_before_turn_start() {
    let root = fixture().await;
    let mut evidence = Evidence::open(root.path(), None).unwrap();
    let (client, server) = tokio::io::duplex(65536);
    let (reader, writer) = tokio::io::split(client);
    let (server_reader, mut server_writer) = tokio::io::split(server);
    let server = tokio::spawn(async move {
        let mut reader = BufReader::new(server_reader);
        receive(&mut reader).await;
        send(&mut server_writer, json!({"id":1,"result":{}})).await;
        receive(&mut reader).await;
        receive(&mut reader).await;
        send(
            &mut server_writer,
            json!({"id":100,"result":{"account":{"type":"chatgpt"}}}),
        )
        .await;
        receive(&mut reader).await;
        send(&mut server_writer, json!({"id":2,"result":{"config":{}}})).await;
        receive(&mut reader).await;
        send(&mut server_writer,json!({"id":3,"result":{"thread":{"id":"thread-native"},
            "model":DEFAULT_MODEL,"modelProvider":"openai","reasoningEffort":"max","approvalPolicy":"never",
            "sandbox":{"type":"dangerFullAccess"}}})).await;
    });
    let config = HostConfig::default();
    let error = protocol::drive(
        BufReader::new(reader),
        writer,
        root.path(),
        "query",
        None,
        &config,
        &mut evidence,
    )
    .await
    .err()
    .unwrap();
    assert!(error.to_string().contains("read-only sandbox"));
    assert!(evidence.receipts.is_empty());
    server.await.unwrap();
}

#[tokio::test]
async fn unexpected_execution_item_aborts_the_workflow() {
    let root = fixture().await;
    let mut evidence = Evidence::open(root.path(), None).unwrap();
    let (client, server) = tokio::io::duplex(65536);
    let (reader, writer) = tokio::io::split(client);
    let (server_reader, mut server_writer) = tokio::io::split(server);
    let server = tokio::spawn(async move {
        let mut reader = BufReader::new(server_reader);
        handshake(&mut reader, &mut server_writer).await;
        send(
            &mut server_writer,
            json!({"method":"turn/completed","params":{
                "threadId":"thread-native","turn":{"id":"turn-native","status":"completed",
                "items":[{"type":"commandExecution","command":"private-command-sentinel"}]}
            }}),
        )
        .await;
    });
    let config = HostConfig::default();
    let error = protocol::drive(
        BufReader::new(reader),
        writer,
        root.path(),
        "query",
        None,
        &config,
        &mut evidence,
    )
    .await
    .err()
    .unwrap();
    assert!(error.to_string().contains("restricted workflow"));
    assert!(!error.to_string().contains("private-command-sentinel"));
    server.await.unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn timeout_kills_and_reaps_only_the_owned_mock_process() {
    use std::os::unix::fs::PermissionsExt;
    let home = tempfile::tempdir().unwrap();
    let executable = home.path().join("mock-codex");
    let pid_file = home.path().join("child.pid");
    std::fs::write(
        &executable,
        r#"#!/bin/sh
printf '%s' "$$" > "$CODEX_HOME/child.pid"
exec sleep 60
"#,
    )
    .unwrap();
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
    let profile = home.path().join("config.toml");
    std::fs::write(&profile, "# unchanged test profile\n").unwrap();
    let config = HostConfig {
        codex_bin: executable.to_string_lossy().into_owned(),
        codex_home: home.path().to_owned(),
        // Allow bounded process startup under parallel compiler/test load. The
        // production timeout remains active from the original start instant.
        timeout_secs: 5,
        ..HostConfig::default()
    };
    let (result, ready) = tokio::join!(
        complete_json("Return an empty object.", json!({}), json!({}), &config),
        wait_for_live_mock_pid(&pid_file),
    );
    let error = result.unwrap_err();
    assert!(error.to_string().contains("time limit"), "{error}");
    let pid = ready.expect("owned mock did not become ready before its independent startup limit");
    let status = std::process::Command::new("/bin/kill")
        .args(["-0", pid.trim()])
        .output()
        .unwrap();
    assert!(
        !status.status.success(),
        "owned child was still running after timeout"
    );
    assert_eq!(
        std::fs::read_to_string(profile).unwrap(),
        "# unchanged test profile\n"
    );
}

#[cfg(unix)]
async fn wait_for_live_mock_pid(pid_file: &Path) -> Result<String> {
    tokio::time::timeout(Duration::from_secs(4), async {
        loop {
            if let Ok(value) = std::fs::read_to_string(pid_file)
                && let Ok(pid) = value.trim().parse::<i32>()
                && pid > 1
            {
                // Signal zero only observes the PID written by this private fixture.
                if unsafe { libc::kill(pid, 0) } == 0 {
                    return Ok(value);
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .map_err(|_| anyhow!("Mock runtime did not publish a live PID within the startup limit"))?
}

type Reader = BufReader<ReadHalf<DuplexStream>>;
type Writer = WriteHalf<DuplexStream>;

#[tokio::test]
async fn node_read_continuation_preserves_markdown_unicode_and_crlf_bytes() {
    let root = tempfile::tempdir().unwrap();
    let mut source = String::from("# Large Unicode document\r\n");
    for index in 0..900 {
        source.push_str(&format!(
            "row {index:04}: 文档内容 🧭 remains exact across windows.\r\n"
        ));
    }
    source.push_str("Final retained CRLF.\r\n");
    std::fs::write(root.path().join("windows.md"), &source).unwrap();
    gptgrep_core::index(root.path(), 10).await.unwrap();
    let mut evidence = Evidence::open(root.path(), None).unwrap();
    let tree = evidence
        .call("tree", "gptgrep_tree", json!({"path":"windows.md"}))
        .await
        .unwrap();
    let tree: Value =
        serde_json::from_str(tree["contentItems"][0]["text"].as_str().unwrap()).unwrap();
    let id = tree["nodes"][0]["node_id"].as_str().unwrap();
    let mut offset = 0usize;
    let mut reconstructed = String::new();
    let mut windows = 0;
    loop {
        let reply = evidence
            .call(
                &format!("read-{windows}"),
                "gptgrep_read",
                json!({"node_id":id,"offset_bytes":offset,"max_bytes":2047}),
            )
            .await
            .unwrap();
        assert_eq!(reply["success"], true, "{reply}");
        let value: Value =
            serde_json::from_str(reply["contentItems"][0]["text"].as_str().unwrap()).unwrap();
        let hit = &value["evidence"];
        let text = hit["text"].as_str().unwrap();
        assert!(!text.is_empty());
        assert_eq!(hit["node_offset"], offset);
        let start = hit["byte_start"].as_u64().unwrap() as usize;
        let end = hit["byte_end"].as_u64().unwrap() as usize;
        assert_eq!(&source[start..end], text);
        assert_eq!(
            hit["node_coverage"],
            json!({"complete":false,"unread_before_bytes":offset,"unread_after_bytes":source.len()-end})
        );
        let receipt = evidence.receipts.last().unwrap();
        assert_eq!(
            serde_json::to_value(receipt.evidence[0].node_coverage).unwrap(),
            hit["node_coverage"]
        );
        reconstructed.push_str(text);
        windows += 1;
        assert!(windows < 100);
        match value["next_offset"].as_u64() {
            Some(next) => {
                assert!(next as usize > offset);
                offset = next as usize;
            }
            None => break,
        }
    }
    assert!(windows > 3);
    assert_eq!(reconstructed, source);
    let final_answer=json!({"answer":"The complete node was inspected.","citations":[id],"insufficient_evidence":false}).to_string();
    let (_, citations, _) = evidence.finish(&final_answer).unwrap();
    assert_eq!(citations.len(), windows);
    assert!(citations.last().unwrap().node_offset > 6144);
    assert_eq!(citations.last().unwrap().next_offset, None);
    assert!(
        citations
            .iter()
            .all(|citation| !citation.node_coverage.unwrap().complete)
    );
    let eof = evidence
        .call(
            "eof",
            "gptgrep_read",
            json!({"node_id":id,"offset_bytes":source.len(),"max_bytes":4}),
        )
        .await
        .unwrap();
    assert_eq!(eof["success"], true);
    let value: Value =
        serde_json::from_str(eof["contentItems"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(value["evidence"]["text"], "");
    assert!(value["next_offset"].is_null());
    assert_eq!(
        value["evidence"]["node_coverage"],
        json!({"complete":false,"unread_before_bytes":source.len(),"unread_after_bytes":0})
    );
    assert!(evidence.receipts.last().unwrap().evidence.is_empty());
    let inside_scalar = source.find('文').unwrap() + 1;
    let invalid = evidence
        .call(
            "unaligned",
            "gptgrep_read",
            json!({"node_id":id,"offset_bytes":inside_scalar,"max_bytes":4}),
        )
        .await
        .unwrap();
    assert_eq!(invalid["success"], false);
}

#[tokio::test]
async fn final_citation_replays_a_late_window_past_the_old_prefix_caps() {
    let root = tempfile::tempdir().unwrap();
    let mut source = String::from("文档导读\r\n");
    source.push_str(&"context before the answer\r\n".repeat(3200));
    let offset = source.len();
    source.push_str("TAIL_RESEARCH_FACT: retained for 37 days.\r\n");
    assert!(offset > 65536);
    std::fs::write(root.path().join("late.md"), &source).unwrap();
    gptgrep_core::index(root.path(), 10).await.unwrap();
    let catalog = gptgrep_core::tree(root.path(), Path::new("late.md")).unwrap();
    let doc_id = catalog["document"]["id"].as_str().unwrap();
    let local_id = catalog["document"]["nodes"][0]["id"].as_str().unwrap();
    let id = format!("{doc_id}:{local_id}");
    // A one-byte prefix cannot hold the first scalar; the selected late window is valid.
    assert!(gptgrep_core::read_node(root.path(), &id, 1).is_err());
    let mut evidence = Evidence::open(root.path(), Some(&id)).unwrap();
    let reply = evidence
        .call(
            "tail",
            "gptgrep_read",
            json!({"node_id":id,"offset_bytes":offset,"max_bytes":1024}),
        )
        .await
        .unwrap();
    assert_eq!(reply["success"], true, "{reply}");
    let value: Value =
        serde_json::from_str(reply["contentItems"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(value["evidence"]["text"], &source[offset..]);
    assert_eq!(
        value["evidence"]["node_coverage"],
        json!({"complete":false,"unread_before_bytes":offset,"unread_after_bytes":0})
    );
    let final_answer =
        json!({"answer":"Retained for 37 days.","citations":[id],"insufficient_evidence":false})
            .to_string();
    let (_, citations, _) = evidence.finish(&final_answer).unwrap();
    assert_eq!(citations.len(), 1);
    assert_eq!(citations[0].node_offset, offset);
    assert_eq!(citations[0].byte_start, offset);
    assert_eq!(citations[0].byte_end, source.len());
    assert_eq!(
        serde_json::to_value(citations[0].node_coverage).unwrap(),
        value["evidence"]["node_coverage"]
    );
    assert_eq!(
        citations[0].excerpt_sha256,
        crate::retrieval::hash(&source.as_bytes()[offset..])
    );
    source.push_str("Source changed after the answer.\r\n");
    std::fs::write(root.path().join("late.md"), source).unwrap();
    assert!(evidence.finish(&final_answer).is_err());
}

#[tokio::test]
async fn search_evidence_keeps_a_replayable_node_relative_cursor() {
    let root = tempfile::tempdir().unwrap();
    let source = format!(
        "# Long line\r\n{}LATE_ANCHOR 文档\r\n",
        "prefix ".repeat(11000)
    );
    std::fs::write(root.path().join("search-window.md"), &source).unwrap();
    gptgrep_core::index(root.path(), 10).await.unwrap();
    let mut evidence = Evidence::open(root.path(), None).unwrap();
    let reply = evidence
        .call(
            "search",
            "gptgrep_search",
            json!({"query":"LATE_ANCHOR","mode":"regex","limit":1}),
        )
        .await
        .unwrap();
    assert_eq!(reply["success"], true, "{reply}");
    let value: Value =
        serde_json::from_str(reply["contentItems"][0]["text"].as_str().unwrap()).unwrap();
    let hit = &value["hits"][0];
    assert!(hit["node_offset"].as_u64().unwrap() > 65536);
    let id = hit["node_id"].as_str().unwrap();
    let final_answer =
        json!({"answer":"The anchor is present.","citations":[id],"insufficient_evidence":false})
            .to_string();
    let (_, citations, _) = evidence.finish(&final_answer).unwrap();
    assert_eq!(citations.len(), 1);
    assert_eq!(
        citations[0].node_offset,
        hit["node_offset"].as_u64().unwrap() as usize
    );
    assert_eq!(
        citations[0].excerpt_sha256,
        crate::retrieval::hash(hit["text"].as_str().unwrap().as_bytes())
    );
}

#[tokio::test]
async fn unclipped_search_node_coverage_survives_payload_and_citation_serialization() {
    let root = tempfile::tempdir().unwrap();
    let source = format!(
        "{}COPPER_OTTER stores 83 glass beads.\r\n{}",
        "quiet 文🙂\r\n".repeat(23),
        "blue moss remains still.\r\n".repeat(41)
    );
    std::fs::write(root.path().join("invented-record.txt"), &source).unwrap();
    gptgrep_core::index(root.path(), 10).await.unwrap();
    for mode in ["regex", "lexical"] {
        let mut evidence = Evidence::open(root.path(), None).unwrap();
        let reply = evidence
            .call(
                "search",
                "gptgrep_search",
                json!({"query":"COPPER_OTTER","mode":mode,"limit":1}),
            )
            .await
            .unwrap();
        assert_eq!(reply["success"], true, "{reply}");
        assert!(serde_json::to_vec(&reply).unwrap().len() <= crate::retrieval::MAX_TOOL_BYTES);
        let value: Value =
            serde_json::from_str(reply["contentItems"][0]["text"].as_str().unwrap()).unwrap();
        let hit = &value["hits"][0];
        let start = hit["byte_start"].as_u64().unwrap() as usize;
        let end = hit["byte_end"].as_u64().unwrap() as usize;
        assert!(start > 0 && end < source.len());
        assert_eq!(hit["text"], &source[start..end]);
        assert_eq!(hit["text_truncated"], false);
        assert_eq!(hit["next_offset"], end);
        assert_eq!(
            hit["node_coverage"],
            json!({"complete":false,"unread_before_bytes":start,"unread_after_bytes":source.len()-end})
        );
        assert_eq!(value["metrics"]["jev_calls_attempted"], 0);
        assert_eq!(value["metrics"]["jev_requests"], 0);
        let receipt = evidence.receipts.last().unwrap();
        assert_eq!(receipt.evidence.len(), 1);
        let mut serialized = serde_json::to_value(&receipt.evidence[0]).unwrap();
        assert_eq!(serialized["node_coverage"], hit["node_coverage"]);
        let roundtrip: crate::retrieval::Citation =
            serde_json::from_value(serialized.clone()).unwrap();
        assert_eq!(roundtrip.node_coverage, receipt.evidence[0].node_coverage);
        serialized.as_object_mut().unwrap().remove("node_coverage");
        let legacy: crate::retrieval::Citation = serde_json::from_value(serialized).unwrap();
        assert_eq!(legacy.node_coverage, None);
        let answer = json!({"answer":"The otter stores glass beads.","citations":[hit["node_id"]],"insufficient_evidence":false}).to_string();
        let (_, citations, _) = evidence.finish(&answer).unwrap();
        assert_eq!(citations.len(), 1);
        assert_eq!(
            (citations[0].byte_start, citations[0].byte_end),
            (start, end)
        );
        assert_eq!(
            citations[0].excerpt_sha256,
            crate::retrieval::hash(&source.as_bytes()[start..end])
        );
        assert_eq!(citations[0].node_coverage, roundtrip.node_coverage);
    }
}

#[test]
fn tool_descriptions_expose_window_local_node_coverage_without_new_arguments() {
    let tools = crate::retrieval::tools();
    for (name, args) in [
        ("gptgrep_search", json!({"query":"invented"})),
        (
            "gptgrep_read",
            json!({"node_id":"synthetic:node","offset_bytes":0,"max_bytes":1}),
        ),
    ] {
        let spec = tools[0]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|spec| spec["name"] == name)
            .unwrap();
        assert!(jsonschema::is_valid(&spec["inputSchema"], &args));
        let description = spec["description"].as_str().unwrap();
        for field in [
            "node_coverage",
            "complete",
            "unread_before_bytes",
            "unread_after_bytes",
        ] {
            assert!(description.contains(field));
            assert!(spec["inputSchema"]["properties"].get(field).is_none());
        }
        assert!(description.contains("that window only, not prior reads"));
        assert!(description.contains("null means"));
        assert!(description.contains("text_truncated=false does not imply"));
    }
}

#[tokio::test]
async fn argument_errors_report_the_actual_bounds_and_allow_recovery() {
    let root = fixture().await;
    let mut evidence = Evidence::open(root.path(), None).unwrap();
    for (tool, args, expected) in [
        ("gptgrep_catalog", json!({"limit":100}), "1..30"),
        (
            "gptgrep_search",
            json!({"query":"retention","limit":20}),
            "1..3",
        ),
        (
            "gptgrep_search",
            json!({"query":"retention","limit":50}),
            "1..3",
        ),
    ] {
        let response = evidence.call(tool, tool, args).await.unwrap();
        assert_eq!(response["success"], false);
        let error: Value =
            serde_json::from_str(response["contentItems"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(error["error"], "invalid_arguments");
        assert!(error["message"].as_str().unwrap().contains(expected));
    }
    let catalog = evidence
        .call("catalog", "gptgrep_catalog", json!({}))
        .await
        .unwrap();
    assert_eq!(catalog["success"], true);
    let tree = evidence
        .call("tree", "gptgrep_tree", json!({"path":"handbook.md"}))
        .await
        .unwrap();
    let data: Value =
        serde_json::from_str(tree["contentItems"][0]["text"].as_str().unwrap()).unwrap();
    let id = data["nodes"][0]["node_id"].as_str().unwrap();
    let response = evidence
        .call(
            "large_read",
            "gptgrep_read",
            json!({"node_id":id,"max_bytes":10000}),
        )
        .await
        .unwrap();
    assert_eq!(response["success"], false);
    let error: Value =
        serde_json::from_str(response["contentItems"][0]["text"].as_str().unwrap()).unwrap();
    assert!(error["message"].as_str().unwrap().contains("1..6144"));
    assert!(error["message"].as_str().unwrap().contains("4096"));
    assert_eq!(
        evidence
            .call("read", "gptgrep_read", json!({"node_id":id}))
            .await
            .unwrap()["success"],
        true
    );
}

#[tokio::test]
async fn insufficient_evidence_requires_an_actual_evidence_operation() {
    let root = fixture().await;
    let mut evidence = Evidence::open(root.path(), None).unwrap();
    let response = r#"{"answer":"Not found","citations":[],"insufficient_evidence":true}"#;
    let error = evidence.finish(response).unwrap_err();
    assert!(error.downcast_ref::<HostCapabilityError>().is_some());
    evidence
        .call(
            "search",
            "gptgrep_search",
            json!({"query":"absent_unique_term","mode":"regex"}),
        )
        .await
        .unwrap();
    assert!(evidence.finish(response).is_ok());
}

#[tokio::test]
async fn oversized_tool_output_is_recoverable_and_does_not_issue_hidden_evidence() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(
        root.path().join("quoted.md"),
        format!("# Quoted\n{}", "\"".repeat(5000)),
    )
    .unwrap();
    gptgrep_core::index(root.path(), 10).await.unwrap();
    let mut evidence = Evidence::open(root.path(), None).unwrap();
    let tree = evidence
        .call("tree", "gptgrep_tree", json!({"path":"quoted.md"}))
        .await
        .unwrap();
    let tree: Value =
        serde_json::from_str(tree["contentItems"][0]["text"].as_str().unwrap()).unwrap();
    let id = tree["nodes"][0]["node_id"].as_str().unwrap();
    let large = evidence
        .call(
            "large",
            "gptgrep_read",
            json!({"node_id":id,"max_bytes":6144}),
        )
        .await
        .unwrap();
    assert_eq!(large["success"], false);
    assert!(serde_json::to_vec(&large).unwrap().len() <= crate::retrieval::MAX_TOOL_BYTES);
    assert!(evidence.receipts.last().unwrap().evidence.is_empty());
    let response =
        json!({"answer":"Quoted text","citations":[id],"insufficient_evidence":false}).to_string();
    assert!(evidence.finish(&response).is_err());
    let small = evidence
        .call(
            "small",
            "gptgrep_read",
            json!({"node_id":id,"max_bytes":128}),
        )
        .await
        .unwrap();
    assert_eq!(small["success"], true);
    assert!(evidence.finish(&response).is_ok());
}

#[cfg(unix)]
#[tokio::test]
async fn timeout_terminates_launcher_descendants() {
    use std::os::unix::fs::PermissionsExt;
    let home = tempfile::tempdir().unwrap();
    let executable = home.path().join("launcher");
    let pid_file = home.path().join("descendant.pid");
    std::fs::write(
        &executable,
        r#"#!/bin/sh
sleep 60 &
descendant=$!
printf '%s' "$descendant" > "$CODEX_HOME/descendant.pid"
trap 'wait "$descendant"; exit 0' TERM
wait "$descendant"
"#,
    )
    .unwrap();
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
    let config = HostConfig {
        codex_bin: executable.to_string_lossy().into_owned(),
        codex_home: home.path().to_owned(),
        timeout_secs: 5,
        ..HostConfig::default()
    };
    let (result, ready) = tokio::join!(
        complete_json("Return an empty object.", json!({}), json!({}), &config),
        wait_for_live_mock_pid(&pid_file),
    );
    let error = result.unwrap_err();
    assert!(error.to_string().contains("time limit"), "{error}");
    let pid = ready.expect("mock descendant did not become ready before its startup limit");
    let status = std::process::Command::new("/bin/kill")
        .args(["-0", pid.trim()])
        .output()
        .unwrap();
    assert!(
        !status.status.success(),
        "launcher descendant survived cleanup"
    );
}

#[tokio::test]
async fn pure_completion_passes_schema_and_state_without_an_index() {
    let cwd = tempfile::tempdir().unwrap();
    let state = json!({"prompt":"Keep these exact lines:\n原始 \"heading\"","history":[{"role":"user","content":"original"}]});
    let schema = json!({"type":"object","properties":{"text":{"type":"string"}},"required":["text"],"additionalProperties":false});
    let (client, server) = tokio::io::duplex(65536);
    let (reader, writer) = tokio::io::split(client);
    let (server_reader, mut server_writer) = tokio::io::split(server);
    let expected_state = state.clone();
    let expected_schema = schema.clone();
    let server = tokio::spawn(async move {
        let mut reader = BufReader::new(server_reader);
        let turn = handshake_mode(&mut reader, &mut server_writer, false).await;
        assert_eq!(turn["params"]["outputSchema"], expected_schema);
        let actual: Value =
            serde_json::from_str(turn["params"]["input"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(actual, expected_state);
        send(&mut server_writer,json!({"method":"turn/completed","params":{
            "threadId":"thread-native","turn":{"id":"turn-native","status":"completed","items":[
            {"type":"agentMessage","phase":"final_answer","text":"{\"text\":\"原始 heading\"}"}]}
        }})).await;
    });
    let config = HostConfig::default();
    let outcome = protocol::run(
        BufReader::new(reader),
        writer,
        cwd.path(),
        &config,
        protocol::Workflow::Completion {
            instructions: "Return the requested text.",
            state: &state,
            schema: &schema,
        },
        None,
    )
    .await
    .unwrap();
    server.await.unwrap();
    let (validator, _) =
        crate::completion::prepare("Return text.", &state, &schema, config.max_input_bytes)
            .unwrap();
    assert_eq!(
        crate::completion::validate_output(&outcome.answer, &validator).unwrap(),
        json!({"text":"原始 heading"})
    );
    assert!(!cwd.path().join(".gptgrep").exists());
}

#[tokio::test]
async fn pure_completion_rejects_any_dynamic_tool_request() {
    let cwd = tempfile::tempdir().unwrap();
    let (client, server) = tokio::io::duplex(65536);
    let (reader, writer) = tokio::io::split(client);
    let (server_reader, mut server_writer) = tokio::io::split(server);
    let server = tokio::spawn(async move {
        let mut reader = BufReader::new(server_reader);
        handshake_mode(&mut reader, &mut server_writer, false).await;
        send(
            &mut server_writer,
            json!({"id":9,"method":"item/tool/call","params":{
                "threadId":"thread-native","turnId":"turn-native","callId":"no-tools",
                "namespace":"gptgrep","tool":"gptgrep_read","arguments":{"node_id":"any"}
            }}),
        )
        .await;
        assert_eq!(receive(&mut reader).await["error"]["code"], -32601);
    });
    let config = HostConfig::default();
    let state = json!({});
    let schema = json!({});
    let error = protocol::run(
        BufReader::new(reader),
        writer,
        cwd.path(),
        &config,
        protocol::Workflow::Completion {
            instructions: "Return JSON.",
            state: &state,
            schema: &schema,
        },
        None,
    )
    .await
    .err()
    .unwrap();
    assert!(error.to_string().contains("Pure completion"));
    server.await.unwrap();
}

#[test]
fn private_trace_contains_only_bounded_protocol_metadata() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("trace.jsonl");
    let mut trace = crate::trace::Trace::open(Some(&path)).unwrap().unwrap();
    for _ in 0..300 {
        trace
            .record(
                "send",
                &json!({"id":1,"method":"thread/start","params":{
                    "dynamicTools":crate::retrieval::tools(),"config":{"key":"private-sentinel"},
                    "state":"private-prompt-sentinel","arguments":{"token":"private-sentinel"}
                }}),
            )
            .unwrap();
    }
    drop(trace);
    let content = std::fs::read_to_string(&path).unwrap();
    assert!(!content.contains("private-sentinel"));
    assert!(!content.contains("private-prompt-sentinel"));
    assert!(content.contains("gptgrep.gptgrep_read"));
    assert!(content.contains("trace_truncated"));
    assert!(content.len() <= 64 * 1024 + 32);
    assert!(crate::trace::Trace::open(Some(&path)).is_err());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}

#[test]
fn process_uses_only_the_selected_profile_and_does_not_modify_it() {
    let home = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let config = home.path().join("config.toml");
    std::fs::write(&config, "# profile sentinel\n").unwrap();
    let command = process_command(Path::new("codex"), cwd.path(), home.path()).unwrap();
    let env: std::collections::BTreeMap<_, _> = command.as_std().get_envs().collect();
    assert_eq!(
        env[std::ffi::OsStr::new("CODEX_HOME")],
        Some(home.path().as_os_str())
    );
    for key in [
        "OPENAI_API_KEY",
        "CODEX_API_KEY",
        "CODEX_SQLITE_HOME",
        "CODEX_THREAD_ID",
    ] {
        assert_eq!(env[std::ffi::OsStr::new(key)], None);
    }
    let args: Vec<_> = command
        .as_std()
        .get_args()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    assert!(args.iter().any(|arg| arg
        == &format!(
            "sqlite_home={}",
            serde_json::to_string(home.path()).unwrap()
        )));
    assert_eq!(
        std::fs::read_to_string(config).unwrap(),
        "# profile sentinel\n"
    );
}

async fn receive(reader: &mut Reader) -> Value {
    let mut line = String::new();
    assert!(reader.read_line(&mut line).await.unwrap() > 0);
    serde_json::from_str(&line).unwrap()
}
async fn send(writer: &mut Writer, value: Value) {
    writer
        .write_all(format!("{value}\n").as_bytes())
        .await
        .unwrap();
    writer.flush().await.unwrap();
}
async fn handshake(reader: &mut Reader, writer: &mut Writer) {
    let _ = handshake_mode(reader, writer, true).await;
}
async fn handshake_mode(reader: &mut Reader, writer: &mut Writer, dynamic: bool) -> Value {
    handshake_thread(reader, writer, dynamic, "fast", Some(json!("priority"))).await;
    handshake_turn(reader, writer, dynamic, "fast").await
}
async fn handshake_thread(
    reader: &mut Reader,
    writer: &mut Writer,
    dynamic: bool,
    requested_tier: &str,
    effective_tier: Option<Value>,
) -> Value {
    let init = receive(reader).await;
    assert_eq!(init["method"], "initialize");
    assert_eq!(init["params"]["capabilities"]["experimentalApi"], true);
    send(writer, json!({"id":1,"result":{"userAgent":"mock"}})).await;
    assert_eq!(receive(reader).await["method"], "initialized");
    let account = receive(reader).await;
    assert_eq!(account["method"], "account/read");
    assert_eq!(account["params"]["refreshToken"], false);
    send(writer,json!({"id":100,"result":{"account":{"type":"chatgpt","email":"private-account-sentinel"}}})).await;
    assert_eq!(receive(reader).await["method"], "config/read");
    send(writer,json!({"id":2,"result":{"config":{"mcp_servers":{"example":{"command":"private-config-sentinel"}}}}})).await;
    let thread = receive(reader).await;
    assert_eq!(thread["method"], "thread/start");
    let params = &thread["params"];
    assert_eq!(params["model"], DEFAULT_MODEL);
    assert_eq!(params["config"]["model_reasoning_effort"], "max");
    assert_eq!(params["serviceTier"], requested_tier);
    assert_eq!(params["config"]["service_tier"], requested_tier);
    if matches!(requested_tier, "fast" | "priority") {
        assert_eq!(params["config"]["features.fast_mode"], true);
    }
    assert_eq!(params["ephemeral"], true);
    assert_eq!(params["environments"], json!([]));
    assert_eq!(
        params["config"]["mcp_servers"],
        json!({"example":{"enabled":false}})
    );
    assert!(!thread.to_string().contains("private-config-sentinel"));
    assert_eq!(params["config"]["features.shell_tool"], false);
    assert_eq!(params["config"]["features.plugins"], false);
    if dynamic {
        assert_eq!(params["dynamicTools"].as_array().unwrap().len(), 1);
        assert_eq!(params["dynamicTools"][0]["name"], "gptgrep");
        assert_eq!(
            params["dynamicTools"][0]["tools"].as_array().unwrap().len(),
            4
        );
        assert_eq!(
            params["config"]["features.code_mode.direct_only_tool_namespaces"],
            json!(["gptgrep"])
        );
        assert_eq!(params["config"]["features.code_mode_host.enabled"], false);
        assert_eq!(
            params["config"]["features.code_mode_host.disable_in_process_fallback"],
            false
        );
    } else {
        assert_eq!(params["dynamicTools"], json!([]));
    }
    let mut result = json!({
        "thread":{"id":"thread-native"},"model":DEFAULT_MODEL,"modelProvider":"openai","reasoningEffort":"max",
        "approvalPolicy":"never","sandbox":{"type":"readOnly","networkAccess":false}
    });
    if let Some(tier) = effective_tier {
        result["serviceTier"] = tier;
    }
    send(writer, json!({"id":3,"result":result})).await;
    thread
}
async fn handshake_turn(
    reader: &mut Reader,
    writer: &mut Writer,
    dynamic: bool,
    requested_tier: &str,
) -> Value {
    let turn = receive(reader).await;
    assert_eq!(turn["method"], "turn/start");
    assert_eq!(turn["params"]["effort"], "max");
    assert_eq!(turn["params"]["serviceTier"], requested_tier);
    assert!(turn["params"].get("serviceTierForTurn").is_none());
    if dynamic {
        assert_eq!(
            turn["params"]["outputSchema"]["additionalProperties"],
            false
        );
    }
    send(
        writer,
        json!({"id":4,"result":{"turn":{"id":"turn-native","status":"inProgress"}}}),
    )
    .await;
    turn
}
async fn call(reader: &mut Reader, writer: &mut Writer, id: u64, tool: &str, args: Value) -> Value {
    send(
        writer,
        json!({"id":id,"method":"item/tool/call","params":{
            "threadId":"thread-native","turnId":"turn-native","callId":format!("call-{id}"),
            "namespace":"gptgrep","tool":tool,"arguments":args
        }}),
    )
    .await;
    let response = receive(reader).await;
    assert_eq!(response["id"], id);
    assert_eq!(response["result"]["success"], true);
    serde_json::from_str(
        response["result"]["contentItems"][0]["text"]
            .as_str()
            .unwrap(),
    )
    .unwrap()
}
async fn finish(writer: &mut Writer, citations: Vec<String>) {
    let answer = json!({"answer":"Records are retained for seven years.","citations":citations,"insufficient_evidence":false});
    send(writer,json!({"method":"thread/tokenUsage/updated","params":{
        "threadId":"thread-native","turnId":"turn-native","tokenUsage":{"total":{"totalTokens":123}}
    }})).await;
    send(writer,json!({"method":"item/completed","params":{
        "threadId":"thread-native","turnId":"turn-native","item":{"type":"agentMessage","id":"answer","phase":"final_answer","text":answer.to_string()}
    }})).await;
    send(
        writer,
        json!({"method":"turn/completed","params":{
            "threadId":"thread-native","turn":{"id":"turn-native","status":"completed","items":[]}
        }}),
    )
    .await;
}
async fn fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("handbook.md"),"# Handbook\n\n## Retention\nRecords are retained for seven years.\n\n## Deletion\nApproved deletion requires a review.\n").unwrap();
    gptgrep_core::index(dir.path(), 10).await.unwrap();
    dir
}

#[tokio::test]
async fn mock_model_drives_real_catalog_tree_read_and_checked_citations() {
    let root = fixture().await;
    let mut evidence = Evidence::open(root.path(), None).unwrap();
    let (client, server) = tokio::io::duplex(65536);
    let (reader, writer) = tokio::io::split(client);
    let (server_reader, mut server_writer) = tokio::io::split(server);
    let server = tokio::spawn(async move {
        let mut reader = BufReader::new(server_reader);
        handshake(&mut reader, &mut server_writer).await;
        send(&mut server_writer,json!({"method":"error","params":{
            "threadId":"thread-native","turnId":"turn-native","willRetry":true,
            "error":{"message":"private-transient-error","codexErrorInfo":{"responseStreamDisconnected":{"httpStatusCode":502}}}
        }})).await;
        send(&mut server_writer,json!({"id":99,"method":"unknown/request","params":{"secret":"private-config-sentinel"}})).await;
        assert_eq!(receive(&mut reader).await["error"]["code"], -32601);
        let catalog = call(
            &mut reader,
            &mut server_writer,
            10,
            "gptgrep_catalog",
            json!({}),
        )
        .await;
        let path = catalog["documents"][0]["path"].as_str().unwrap();
        let tree = call(
            &mut reader,
            &mut server_writer,
            11,
            "gptgrep_tree",
            json!({"path":path}),
        )
        .await;
        let id = tree["nodes"][0]["node_id"].as_str().unwrap().to_owned();
        let excerpt = call(
            &mut reader,
            &mut server_writer,
            12,
            "gptgrep_read",
            json!({"node_id":id}),
        )
        .await;
        assert!(
            excerpt["evidence"]["text"]
                .as_str()
                .unwrap()
                .contains("seven years")
        );
        finish(&mut server_writer, vec![id]).await;
    });
    let config = HostConfig::default();
    let outcome = protocol::drive(
        BufReader::new(reader),
        writer,
        root.path(),
        "retention?",
        None,
        &config,
        &mut evidence,
    )
    .await
    .unwrap();
    server.await.unwrap();
    let (answer, citations, insufficient) = evidence.finish(&outcome.answer).unwrap();
    assert_eq!(outcome.thread_id, "thread-native");
    assert_eq!(outcome.turn_id, "turn-native");
    assert_eq!(outcome.model, DEFAULT_MODEL);
    assert_eq!(outcome.effort.as_deref(), Some("max"));
    assert_eq!(outcome.service_tier.as_deref(), Some("priority"));
    assert!(
        outcome
            .warnings
            .iter()
            .any(|warning| warning.contains("1 transient retry notifications"))
    );
    assert_eq!(outcome.usage.unwrap()["total"]["totalTokens"], 123);
    assert_eq!(answer, "Records are retained for seven years.");
    assert!(!insufficient);
    assert_eq!(evidence.receipts.len(), 3);
    assert_eq!(citations.len(), 1);
    assert_eq!(citations[0].source_sha256.len(), 64);
    assert!(citations[0].byte_end > citations[0].byte_start);
    assert!(
        outcome
            .warnings
            .iter()
            .any(|message| message.contains("denied"))
    );
}

#[tokio::test]
async fn fabricated_citation_and_changed_source_are_rejected() {
    let root = fixture().await;
    let mut evidence = Evidence::open(root.path(), None).unwrap();
    assert!(
        evidence
            .finish(
                r#"{"answer":"Claim","citations":["invented:node"],"insufficient_evidence":false}"#
            )
            .is_err()
    );
    let tree = evidence
        .call("tree", "gptgrep_tree", json!({"path":"handbook.md"}))
        .await
        .unwrap();
    let body: Value =
        serde_json::from_str(tree["contentItems"][0]["text"].as_str().unwrap()).unwrap();
    let id = body["nodes"][0]["node_id"].as_str().unwrap();
    evidence
        .call("read", "gptgrep_read", json!({"node_id":id}))
        .await
        .unwrap();
    let answer =
        json!({"answer":"Claim","citations":[id],"insufficient_evidence":false}).to_string();
    assert!(evidence.finish(&answer).is_ok());
    std::fs::write(root.path().join("handbook.md"), "# Changed\nnew evidence").unwrap();
    assert!(evidence.finish(&answer).is_err());
}

#[tokio::test]
async fn replacing_current_generation_invalidates_even_an_uncertain_answer() {
    let root = fixture().await;
    let evidence = Evidence::open(root.path(), None).unwrap();
    gptgrep_core::index(root.path(), 10).await.unwrap();
    let error = evidence
        .finish(r#"{"answer":"Insufficient evidence","citations":[],"insufficient_evidence":true}"#)
        .unwrap_err();
    assert!(error.to_string().contains("generation"));
}

#[tokio::test]
async fn stalled_transport_is_cancellable() {
    let root = fixture().await;
    let mut evidence = Evidence::open(root.path(), None).unwrap();
    let (client, _server) = tokio::io::duplex(65536);
    let (reader, writer) = tokio::io::split(client);
    let config = HostConfig::default();
    let result = tokio::time::timeout(
        Duration::from_millis(10),
        protocol::drive(
            BufReader::new(reader),
            writer,
            root.path(),
            "query",
            None,
            &config,
            &mut evidence,
        ),
    )
    .await;
    assert!(result.is_err());
}

#[tokio::test]
async fn malformed_transport_does_not_echo_contents() {
    let root = fixture().await;
    let mut evidence = Evidence::open(root.path(), None).unwrap();
    let (client, mut server) = tokio::io::duplex(65536);
    let (reader, writer) = tokio::io::split(client);
    server
        .write_all(b"private-config-sentinel\n")
        .await
        .unwrap();
    let config = HostConfig::default();
    let error = protocol::drive(
        BufReader::new(reader),
        writer,
        root.path(),
        "query",
        None,
        &config,
        &mut evidence,
    )
    .await
    .err()
    .unwrap();
    assert!(!error.to_string().contains("private-config-sentinel"));
}
