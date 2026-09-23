use super::*;

const QUESTION: &str = "Which ratio applies to the two counters?";
const SOURCE: &str = "# Field notebook\r\n## Initial note\r\nThe amber and violet counters use the ratio defined for that pair.\r\n## Ratio definition\r\nThat pair uses 5:3, recorded as 文档 🧭.\r\n";

fn config(enabled: bool, budget: usize) -> HostConfig {
    HostConfig {
        source_continuation: enabled,
        document: Some("notes.md".into()),
        query_plan: Some(QueryPlanConfig::default()),
        max_tool_calls: budget,
        ..Default::default()
    }
}

#[test]
fn source_continuation_is_scoped_planned_ask_only_and_does_not_reach_planner() {
    let mut selected = HostConfig {
        source_continuation: true,
        ..Default::default()
    };
    assert!(validate_config(&selected, QUESTION).is_err());
    selected.query_plan = Some(QueryPlanConfig::default());
    assert!(validate_config(&selected, QUESTION).is_err());
    selected.document = Some("notes.md".into());
    validate_config(&selected, QUESTION).unwrap();
    let planner = planner_runtime_config(&selected, selected.query_plan.as_ref().unwrap());
    assert!(!planner.source_continuation);
    assert_eq!(planner.timeout_secs, selected.timeout_secs);
    assert_eq!(planner.max_tool_calls, selected.max_tool_calls);
    assert_eq!(
        source_continuation_receipt(&selected).unwrap()["guidance_sha256"],
        crate::retrieval::hash(protocol::SOURCE_CONTINUATION_GUIDANCE.as_bytes())
    );
    assert!(source_continuation_receipt(&HostConfig::default()).is_none());
    let mut unchanged = "Existing instructions.\n".to_owned();
    protocol::append_source_continuation(&mut unchanged, false, true);
    assert_eq!(unchanged, "Existing instructions.\n");
    protocol::append_source_continuation(&mut unchanged, true, false);
    assert_eq!(unchanged, "Existing instructions.\n");
}

#[derive(Clone, Copy)]
enum Continuation {
    Resolve,
    Exhausted,
    Stale,
}

async fn exercise(enabled: bool, mode: Continuation) -> (String, usize) {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("notes.md"), SOURCE).unwrap();
    std::fs::write(
        root.path().join("outside.md"),
        "# Other document\nUnrelated material.\n",
    )
    .unwrap();
    gptgrep_core::index(root.path(), 8).await.unwrap();
    let budget = if matches!(mode, Continuation::Exhausted) {
        2
    } else {
        3
    };
    let config = config(enabled, budget);
    validate_config(&config, QUESTION).unwrap();
    let mut evidence = Evidence::open(root.path(), None).unwrap();
    let accounting =
        crate::jev_accounting::Accounting::create(root.path(), &evidence.generation).unwrap();
    evidence.configure(&config, accounting, None).unwrap();
    let (client, server) = tokio::io::duplex(65536);
    let (reader, writer) = tokio::io::split(client);
    let (server_reader, mut server_writer) = tokio::io::split(server);
    let document = root.path().join("notes.md");
    let server = tokio::spawn(async move {
        let mut reader = BufReader::new(server_reader);
        let thread = handshake_thread(
            &mut reader,
            &mut server_writer,
            true,
            "fast",
            Some(json!("priority")),
        )
        .await;
        let instructions = thread["params"]["baseInstructions"]
            .as_str()
            .unwrap()
            .to_owned();
        assert_eq!(
            instructions.contains(protocol::SOURCE_CONTINUATION_GUIDANCE),
            enabled
        );
        let turn = handshake_turn(&mut reader, &mut server_writer, true, "fast").await;
        let state: Value =
            serde_json::from_str(turn["params"]["input"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(state["document_scope"], "notes.md");
        let tree = call(
            &mut reader,
            &mut server_writer,
            10,
            "gptgrep_tree",
            json!({"path":"notes.md"}),
        )
        .await;
        let nodes = tree["nodes"].as_array().unwrap();
        let first = nodes
            .iter()
            .find(|node| node["title"] == "Initial note")
            .unwrap()["node_id"]
            .as_str()
            .unwrap()
            .to_owned();
        let second = nodes
            .iter()
            .find(|node| node["title"] == "Ratio definition")
            .unwrap()["node_id"]
            .as_str()
            .unwrap()
            .to_owned();
        let first_read = call(
            &mut reader,
            &mut server_writer,
            11,
            "gptgrep_read",
            json!({"node_id":first}),
        )
        .await;
        assert!(first_read["next_offset"].is_null());
        let first_hit = &first_read["evidence"];
        assert_eq!(first_hit["node_coverage"]["complete"], true);
        assert!(!first_hit["text"].as_str().unwrap().contains("5:3"));
        let start = first_hit["byte_start"].as_u64().unwrap() as usize;
        let end = first_hit["byte_end"].as_u64().unwrap() as usize;
        assert_eq!(first_hit["text"], &SOURCE[start..end]);
        if matches!(mode, Continuation::Stale) {
            std::fs::write(&document, "# Changed source\r\nNo retained ratio.\r\n").unwrap();
        }
        send(&mut server_writer, json!({"id":12,"method":"item/tool/call","params":{
            "threadId":"thread-native","turnId":"turn-native","callId":"adjacent-read","namespace":"gptgrep",
            "tool":"gptgrep_read","arguments":{"node_id":second}
        }})).await;
        let response = receive(&mut reader).await;
        assert_eq!(response["id"], 12);
        let payload: Value = serde_json::from_str(
            response["result"]["contentItems"][0]["text"]
                .as_str()
                .unwrap(),
        )
        .unwrap();
        let answer = if matches!(mode, Continuation::Resolve) {
            assert_eq!(response["result"]["success"], true);
            let hit = &payload["evidence"];
            assert!(hit["text"].as_str().unwrap().contains("文档 🧭"));
            assert!(hit["text"].as_str().unwrap().contains("\r\n"));
            let start = hit["byte_start"].as_u64().unwrap() as usize;
            let end = hit["byte_end"].as_u64().unwrap() as usize;
            assert_eq!(hit["text"], &SOURCE[start..end]);
            json!({"answer":"The pair uses 5:3.","citations":[first,second],"insufficient_evidence":false})
        } else {
            assert_eq!(response["result"]["success"], false);
            if matches!(mode, Continuation::Exhausted) {
                assert_eq!(payload["error"], "tool_budget_exhausted");
                assert_eq!(payload["tool_budget"]["admitted_tool_calls"], 2);
            } else {
                assert_eq!(payload["error"], "evidence_unavailable");
            }
            json!({"answer":"The required ratio remains unresolved within the available verified evidence.","citations":[],"insufficient_evidence":true})
        };
        send(&mut server_writer, json!({"method":"turn/completed","params":{"threadId":"thread-native",
            "turn":{"id":"turn-native","status":"completed","items":[{"type":"agentMessage","phase":"final_answer","text":answer.to_string()}]}}})).await;
        (instructions, first, second)
    });
    let outcome = protocol::drive(
        BufReader::new(reader),
        writer,
        root.path(),
        QUESTION,
        None,
        &config,
        &mut evidence,
    )
    .await
    .unwrap();
    let (instructions, first, second) = server.await.unwrap();
    let (_, citations, insufficient) = evidence.finish(&outcome.answer).unwrap();
    assert_eq!(insufficient, !matches!(mode, Continuation::Resolve));
    if matches!(mode, Continuation::Resolve) {
        assert_eq!(citations.len(), 2);
        assert!(
            citations
                .iter()
                .all(|citation| citation.path == "notes.md" && citation.next_offset.is_none())
        );
    } else {
        assert!(citations.is_empty());
        assert!(evidence.receipts.last().unwrap().evidence.is_empty());
        assert!(evidence.finish(&json!({"answer":"Unverified claim.","citations":[second],"insufficient_evidence":false}).to_string()).is_err());
        if matches!(mode, Continuation::Stale) {
            assert!(evidence.finish(&json!({"answer":"Old claim.","citations":[first],"insufficient_evidence":false}).to_string()).is_err());
        }
    }
    assert_eq!(evidence.receipts.len(), 3);
    (instructions, citations.len())
}

#[tokio::test]
async fn source_continuation_cross_node_relation_preserves_utf8_crlf_and_issued_spans() {
    let (disabled, _) = exercise(false, Continuation::Resolve).await;
    let (enabled, _) = exercise(true, Continuation::Resolve).await;
    assert_eq!(
        enabled,
        format!("{disabled}\n{}", protocol::SOURCE_CONTINUATION_GUIDANCE)
    );
}

#[tokio::test]
async fn source_continuation_budget_refusal_cannot_issue_the_unread_adjacent_node() {
    exercise(true, Continuation::Exhausted).await;
}

#[tokio::test]
async fn source_continuation_stale_source_cannot_supply_followup_evidence() {
    exercise(true, Continuation::Stale).await;
}

#[tokio::test]
async fn source_continuation_cannot_widen_scope_or_increase_read_output_limits() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("notes.md"), SOURCE).unwrap();
    std::fs::write(
        root.path().join("outside.md"),
        "# Other source\nA separate claim.\n",
    )
    .unwrap();
    gptgrep_core::index(root.path(), 8).await.unwrap();
    let mut evidence = Evidence::open(root.path(), None).unwrap();
    let accounting =
        crate::jev_accounting::Accounting::create(root.path(), &evidence.generation).unwrap();
    evidence
        .configure(&config(true, 3), accounting, None)
        .unwrap();
    let denied = evidence
        .call("outside", "gptgrep_tree", json!({"path":"outside.md"}))
        .await
        .unwrap();
    assert_eq!(denied["success"], false);
    let tree = evidence
        .call("tree", "gptgrep_tree", json!({"path":"notes.md"}))
        .await
        .unwrap();
    let tree: Value =
        serde_json::from_str(tree["contentItems"][0]["text"].as_str().unwrap()).unwrap();
    let id = tree["nodes"][0]["node_id"].as_str().unwrap();
    let denied = evidence
        .call(
            "oversized",
            "gptgrep_read",
            json!({"node_id":id,"max_bytes":10000}),
        )
        .await
        .unwrap();
    assert_eq!(denied["success"], false);
    assert!(
        evidence
            .receipts
            .iter()
            .all(|receipt| receipt.evidence.is_empty())
    );
}
