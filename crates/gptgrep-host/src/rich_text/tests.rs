//! Ordinary in-memory actual Rpc and synthetic loopback Decisions fixtures.
use super::*;
use anyhow::Context;
use gptgrep_core::{GenerationPin, RichGenerationWriter};
use gptgrep_jev::ExecutionContext;
use gptgrep_parse::*;
use std::{collections::BTreeMap, fs, path::Path, sync::Arc, time::SystemTime};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::TcpListener,
};
pub(crate) async fn candidate(
    root: &Path,
    texts: &[&str],
) -> Result<(tempfile::TempDir, RichParsedDocument)> {
    let staging = tempfile::tempdir()?;
    let staging_path = staging.path().canonicalize()?;
    let source = format!(
        "synthetic source bytes for artifact custody tests: {}",
        texts.join("")
    );
    let source = source.as_bytes();
    fs::write(root.join("source.pdf"), source)?;
    fs::write(staging_path.join("prepared.pdf"), source)?;
    let sha = hash(source);
    let profile = hash(b"synthetic parser recipe");
    let mut artifacts = Vec::new();
    let mut pages = Vec::new();
    let mut create =
        |kind: ArtifactKind, page: Option<u32>, bytes: &[u8], name: &str| -> Result<ArtifactRef> {
            let artifact = ArtifactRef {
                id: gptgrep_parse::artifact_id(&sha, &profile, &kind, page, None, &hash(bytes))?,
                kind,
                relative_path: name.into(),
                sha256: hash(bytes),
                profile_sha256: profile.clone(),
                bytes: bytes.len() as u64,
                mime: if name.ends_with(".txt") {
                    "text/plain; charset=utf-8"
                } else {
                    "application/json"
                }
                .into(),
                page,
                region_id: None,
            };
            fs::write(staging_path.join(name), bytes)?;
            artifacts.push(artifact.clone());
            Ok(artifact)
        };
    for (index, text) in texts.iter().enumerate() {
        let number = index as u32 + 1;
        let native_text = create(
            ArtifactKind::NativeText,
            Some(number),
            text.as_bytes(),
            &format!("p{number}-native.txt"),
        )?;
        let geometry = PageGeometry {
            width_points: 100.0,
            height_points: 200.0,
            rotation_quarter_turns: 0,
            user_unit: 1.0,
            source_to_page: AffineTransform::IDENTITY,
            page_to_source: AffineTransform::IDENTITY,
        };
        let complexity = serde_json::from_value(
            json!({"page_number":number,"text_length":text.len(),"text_coverage":0.5,
            "has_substantial_images":false,"image_block_count":0,"image_coverage":0.0,"largest_image_coverage":0.0,
            "full_page_image":false,"uncovered_vector_area":null,"is_garbled":false,"page_area":20000.0,
            "needs_ocr":false,"reasons":[],"layout":null}),
        )?;
        let capabilities =
            BTreeMap::from([("annotation_count".into(), FeatureState::Present("0".into()))]);
        let metadata_artifact = create(
            ArtifactKind::NativeMetadata,
            Some(number),
            &serde_json::to_vec(
                &json!({"page":{"page_number":number,"page_width":100.0,"page_height":200.0,"text_items":[]},"objects":[],
                "capabilities":capabilities,"complexity":complexity,"geometry":geometry,"image_rects":[]}),
            )?,
            &format!("p{number}-native.json"),
        )?;
        pages.push(NativePage {
            number,
            label: FeatureState::Absent,
            width_points: 100.0,
            height_points: 200.0,
            geometry,
            native_text,
            metadata_artifact,
            renditions: vec![],
            complexity,
            coverage: CoverageState::Native,
            capabilities,
        });
    }
    ensure!(texts.len() == 1, "one-page ordinary host fixture");
    fs::write(staging_path.join("fixture.txt"), texts[0])?;
    let mut document = gptgrep_parse::parse_path(&staging_path.join("fixture.txt")).await?;
    document.title = "synthetic".into();
    document.parser = RICH_PARSER.into();
    create(
        ArtifactKind::MergedText,
        None,
        document.text.as_bytes(),
        "merged.txt",
    )?;
    create(ArtifactKind::Layout, None, b"[]", "layout.json")?;
    let unmapped_merged_lines = document.text.lines().count().max(1);
    Ok((
        staging,
        RichParsedDocument {
            summary: PreparedSummary {
                source_sha256: sha.clone(),
                prepared_sha256: sha,
                source_bytes: source.len() as u64,
                prepared_bytes: source.len() as u64,
                format: "pdf".into(),
                parser_identity: RICH_PARSER.into(),
                preparation_profile_sha256: hash(&serde_json::to_vec(&(
                    "gptgrep.prepare.identity-pdf.v1",
                    gptgrep_parse::LITEPARSE_REVISION,
                ))?),
                conversion: FeatureState::Present(gptgrep_parse::ConversionIdentity {
                    converter: "identity-pdf".into(),
                    version: gptgrep_parse::LITEPARSE_REVISION.into(),
                    invocations: 0,
                }),
                image_geometry: FeatureState::Absent,
            },
            document,
            pages,
            artifacts,
            native_profile_sha256: profile,
            segments: vec![],
            ocr_tasks: vec![],
            ocr_results: vec![],
            coverage: texts
                .iter()
                .enumerate()
                .map(|(i, _)| (i as u32 + 1, CoverageState::Native))
                .collect(),
            unmapped_merged_lines,
        },
    ))
}

async fn publish(root: &Path, texts: &[&str]) -> Result<GenerationPin> {
    let (staging, parsed) = candidate(root, texts).await?;
    let mut writer = RichGenerationWriter::begin(root, ResourceBudget::default())?;
    writer.add_document("source.pdf", &staging.path().canonicalize()?, parsed)?;
    writer.publish()
}

pub(crate) fn context(limit: u32) -> Result<ExecutionContext> {
    ExecutionContext::new(
        "ordinary-rich-rpc-fixture",
        &"a".repeat(64),
        SystemTime::now() + Duration::from_secs(60),
        limit,
    )
}
async fn mock_with_score(
    count: usize,
    action: Arc<dyn Fn(usize) -> Result<()> + Send + Sync>,
    fail_second: bool,
    score: f64,
) -> Result<(String, tokio::task::JoinHandle<Result<Vec<Value>>>)> {
    mock_with_score_and_delay(count, action, fail_second, score, Duration::ZERO).await
}
async fn mock_with_score_and_delay(
    count: usize,
    action: Arc<dyn Fn(usize) -> Result<()> + Send + Sync>,
    fail_second: bool,
    score: f64,
    response_delay: Duration,
) -> Result<(String, tokio::task::JoinHandle<Result<Vec<Value>>>)> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let endpoint = format!("http://{}/decisions", listener.local_addr()?);
    let task = tokio::spawn(async move {
        let mut requests = Vec::new();
        for ordinal in 0..count {
            let (mut socket, _) = listener.accept().await?;
            let mut bytes = Vec::new();
            let (header_end, length) = loop {
                let mut buffer = [0u8; 4096];
                let n = socket.read(&mut buffer).await?;
                ensure!(n > 0, "request header closed");
                bytes.extend_from_slice(&buffer[..n]);
                ensure!(bytes.len() <= 131072, "mock request admission");
                if let Some(end) = bytes.windows(4).position(|s| s == b"\r\n\r\n") {
                    let header = std::str::from_utf8(&bytes[..end])?;
                    let len = header
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(|v| v.trim().parse::<usize>())
                        })
                        .context("content length")??;
                    break (end + 4, len);
                }
            };
            while bytes.len() < header_end + length {
                let mut buffer = [0u8; 4096];
                let n = socket.read(&mut buffer).await?;
                ensure!(n > 0, "request body closed");
                bytes.extend_from_slice(&buffer[..n]);
            }
            let request: Value = serde_json::from_slice(&bytes[header_end..header_end + length])?;
            action(ordinal)?;
            tokio::time::sleep(response_delay).await;
            let status = if fail_second && ordinal == 1 {
                500
            } else {
                200
            };
            let answers = request["questions"]
                .as_object()
                .context("questions")?
                .keys()
                .map(|key| {
                    (
                        key.clone(),
                        json!({"type":"score","score":score,"confidence":0.75}),
                    )
                })
                .collect::<serde_json::Map<_, _>>();
            let body = serde_json::to_vec(
                &json!({"model":"openai/gpt-6-luna-decisions-20261006","answers":answers,
                "usage":{"input_tokens":5,"output_tokens":2,"cost":0.001,"unknown_detail":"x".repeat(131072)},"id":"synthetic-response","provider":"synthetic-loopback"}),
            )?;
            socket.write_all(format!("HTTP/1.1 {status} Synthetic\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",body.len()).as_bytes()).await?;
            socket.write_all(&body).await?;
            requests.push(request);
        }
        Ok(requests)
    });
    Ok((endpoint, task))
}
pub(crate) async fn mock(
    count: usize,
    action: Arc<dyn Fn(usize) -> Result<()> + Send + Sync>,
    fail_second: bool,
) -> Result<(String, tokio::task::JoinHandle<Result<Vec<Value>>>)> {
    mock_with_score(count, action, fail_second, 3.0).await
}
pub(crate) fn client(endpoint: &str, context: ExecutionContext) -> Result<JevClient> {
    Ok(
        JevClient::with_endpoint("ordinary-fixture-not-a-secret", None, endpoint)?
            .with_execution_context(context),
    )
}

async fn prepared_fixture() -> Result<(
    tempfile::TempDir,
    PreparedRichTextHost,
    ExecutionContext,
    tokio::task::JoinHandle<Result<Vec<Value>>>,
)> {
    prepared_fixture_with_scope(false).await
}

async fn prepared_fixture_with_scope(
    scoped: bool,
) -> Result<(
    tempfile::TempDir,
    PreparedRichTextHost,
    ExecutionContext,
    tokio::task::JoinHandle<Result<Vec<Value>>>,
)> {
    let dir = tempfile::tempdir()?;
    let root = dir.path().canonicalize()?;
    publish(&root, &["alpha\r\n中 tail\n"]).await?;
    let reader = RichGenerationReader::open(&root)?;
    let execution = context(2)?;
    let (endpoint, server) = mock(2, Arc::new(|_| Ok(())), false).await?;
    let document = if scoped {
        Some(reader.manifest()?.documents[0].document.path.clone())
    } else {
        None
    };
    let config = HostConfig {
        document: document.clone(),
        ..Default::default()
    };
    let mut options = RichTextHostOptions::default();
    options.retrieval.document = document;
    let prepared = prepare_rich_text_host(
        reader,
        client(&endpoint, execution.clone())?,
        "source claim",
        &config,
        &options,
    )?;
    Ok((dir, prepared, execution, server))
}

pub(crate) fn assert_final_reader_reservation(
    root: &Path,
    generation: &str,
    question: &str,
    document_scope: Option<&str>,
) -> Result<()> {
    let ledgers =
        fs::read_dir(root.join(".gptgrep/host-attempts"))?.collect::<std::io::Result<Vec<_>>>()?;
    assert_eq!(ledgers.len(), 1);
    let rows = fs::read_to_string(ledgers[0].path())?
        .lines()
        .map(serde_json::from_str::<Value>)
        .collect::<serde_json::Result<Vec<_>>>()?;
    let bound = rows
        .iter()
        .position(|row| row["event"] == "workflow_bound")
        .context("rich reader workflow must be bound")?;
    let reserved = rows
        .iter()
        .position(|row| row["event"] == "model_attempt_reserved")
        .context("rich reader model attempt must be reserved")?;
    assert!(bound < reserved);
    let workflow = json!({"query_sha256":hash(question.as_bytes()),
        "document_scope":document_scope,"generation":generation});
    assert_eq!(rows[bound]["workflow"], workflow);
    assert_eq!(rows[reserved]["workflow"], workflow);
    assert_eq!(rows[reserved]["model_attempt_limit"], 1);
    assert_eq!(
        rows[reserved]["model_attempts"].as_array().unwrap().len(),
        1
    );
    let attempt = &rows[reserved]["receipt"]["model_attempt"];
    assert_eq!(attempt["attempt_id"], "final_reader-1");
    assert_eq!(attempt["role"], "final_reader");
    assert_eq!(attempt["status"], "reserved");
    assert_eq!(
        rows.iter()
            .filter(|row| row["event"] == "model_attempt_reserved")
            .count(),
        1
    );
    Ok(())
}

#[tokio::test]
async fn prepared_rich_text_binds_workflow_and_reserves_final_reader_before_native_start()
-> Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path().canonicalize()?;
    let generation = publish(&root, &["alpha\r\n中 tail\n"]).await?;
    let execution = context(2)?;
    let (endpoint, server) = mock(2, Arc::new(|_| Ok(())), false).await?;
    let question = "Describe the source statement.";
    let config = HostConfig {
        document: Some("source.pdf".into()),
        codex_home: root.join("missing-owned-codex-home"),
        codex_bin: root
            .join("missing-owned-runtime")
            .to_string_lossy()
            .into_owned(),
        ..Default::default()
    };
    let mut options = RichTextHostOptions::default();
    options.retrieval.document = config.document.clone();
    assert!(!config.codex_home.exists());
    let prepared = prepare_rich_text_host(
        RichGenerationReader::open(&root)?,
        client(&endpoint, execution.clone())?,
        question,
        &config,
        &options,
    )?;
    // Real rich execution crosses accounting admission, then the owned absent
    // home fails before native command construction. Only the local mock runs.
    let error = prepared
        .execute(&|_| Ok(()))
        .await
        .err()
        .context("absent home")?;
    assert_eq!(server.await??.len(), 2);
    assert_eq!(error.code, "host_rich_reader_failed");
    assert_eq!(error.stage, "reader");
    assert_eq!(error.model_attempts.len(), 1);
    assert_eq!(error.model_attempts[0].role, "final_reader");
    assert_eq!(error.model_attempts[0].status, "failed");
    assert!(error.model_attempts[0].thread_id.is_none());
    assert!(error.model_attempts[0].turn_id.is_none());
    assert!(!config.codex_home.exists());
    assert_eq!(execution.snapshot()?.submission_observations, 2);
    assert_final_reader_reservation(
        &root,
        &generation.generation,
        question,
        config.document.as_deref(),
    )
}

async fn genuine(prepared: PreparedRichTextHost) -> Result<RichEvidence> {
    let initial = prepared.bootstrap.execute().await?;
    Ok(RichEvidence::from_initial(
        initial,
        prepared.options.max_citations,
        prepared.deadline,
        prepared.options.retrieval.document,
    )?)
}
async fn line(reader: &mut (impl tokio::io::AsyncBufRead + Unpin)) -> Result<Value> {
    let mut text = String::new();
    ensure!(
        reader.read_line(&mut text).await? > 0,
        "ordinary mock closed"
    );
    Ok(serde_json::from_str(&text)?)
}
async fn emit(writer: &mut (impl tokio::io::AsyncWrite + Unpin), value: Value) -> Result<()> {
    let mut bytes = serde_json::to_vec(&value)?;
    bytes.push(b'\n');
    writer.write_all(&bytes).await?;
    writer.flush().await?;
    Ok(())
}
#[derive(Clone, Copy)]
enum ArgumentMode {
    PageLimit,
    EmptyDocument,
    UnknownField,
    ReplayRpc,
    ReplayCall,
    Budget,
    ForeignThread,
    ForeignTurn,
    Namespace,
    Document,
    ScopedCursor,
    Artifact,
    Oversized,
}
#[derive(Clone, Copy)]
enum NativeMode {
    Arguments(ArgumentMode),
    Normal,
    BootstrapCitation,
    ForbiddenNamespace,
    ReplayedId,
    ReselectTurn,
    Render,
    Ocr,
}
async fn native_mock(
    stream: tokio::io::DuplexStream,
    artifact: String,
    mode: NativeMode,
) -> Result<Vec<Value>> {
    let (read, mut write) = tokio::io::split(stream);
    let mut read = BufReader::new(read);
    let mut sent = Vec::new();
    loop {
        let request = line(&mut read).await?;
        sent.push(request.clone());
        match request["method"].as_str() {
            Some("initialize") => emit(&mut write, json!({"id":1,"result":{}})).await?,
            Some("initialized") => {}
            Some("account/read") => {
                emit(
                    &mut write,
                    json!({"id":100,"result":{"account":{"type":"chatgpt"}}}),
                )
                .await?
            }
            Some("config/read") => {
                emit(
                    &mut write,
                    json!({"id":2,"result":{"config":{"mcp_servers":{}}}}),
                )
                .await?
            }
            Some("thread/start") => {
                ensure!(
                    request["params"]["dynamicTools"][0]["type"] == "namespace",
                    "typed namespace declaration"
                );
                ensure!(
                    request["params"]["dynamicTools"][0]["tools"][0]["type"] == "function",
                    "typed function declaration"
                );
                ensure!(
                    request["params"]["allowProviderModelFallback"] == false,
                    "no fallback"
                );
                emit(&mut write,json!({"id":3,"result":{"thread":{"id":"ordinary-thread"},"model":request["params"]["model"],"modelProvider":"openai",
                    "approvalPolicy":"never","sandbox":{"type":"readOnly","networkAccess":false},"reasoningEffort":"max","serviceTier":"priority"}})).await?;
            }
            Some("turn/start") => {
                let state: Value = serde_json::from_str(
                    request["params"]["input"][0]["text"]
                        .as_str()
                        .context("state")?,
                )?;
                ensure!(
                    state["citation_status"] == "model_exposure_only_read_artifact_required",
                    "bootstrap exposure"
                );
                if matches!(mode, NativeMode::BootstrapCitation) {
                    let id = state["initial_retrieval"][0]["candidate_id"]
                        .as_str()
                        .context("candidate")?;
                    let answer=json!({"answer":"claim","insufficient_evidence":false,"citations":[{"issued_id":id,"offset":0,"length":5,"quote":"alpha"}]}).to_string();
                    emit(&mut write,json!({"method":"turn/completed","params":{"threadId":"ordinary-thread","turn":{"id":"ordinary-turn","status":"completed","error":null,"items":[{"type":"agentMessage","text":answer}]}}})).await?;
                    return Ok(sent);
                }
                if let NativeMode::Arguments(case) = mode {
                    // Bind the genuine turn before trying any malformed scope.
                    emit(
                        &mut write,
                        json!({"id":4,"result":{"turn":{"id":"ordinary-turn"}}}),
                    )
                    .await?;
                    let mut call = json!({"id":"argument-request","method":"item/tool/call","params":{
                        "threadId":"ordinary-thread","turnId":"ordinary-turn","callId":"argument-call",
                        "namespace":"gptgrep","tool":"gptgrep_inspect_artifact",
                        "arguments":{"document":"source.pdf","page_size":9}}});
                    match case {
                        ArgumentMode::EmptyDocument => {
                            call["params"]["arguments"] = json!({"document":"","page_size":1});
                        }
                        ArgumentMode::UnknownField => {
                            call["params"]["arguments"] =
                                json!({"document":"source.pdf","extra":true});
                        }
                        ArgumentMode::ForeignThread => {
                            call["params"]["threadId"] = json!("other-thread")
                        }
                        ArgumentMode::ForeignTurn => call["params"]["turnId"] = json!("other-turn"),
                        ArgumentMode::Namespace => call["params"]["namespace"] = json!("other"),
                        ArgumentMode::Document => {
                            call["params"]["arguments"]["document"] = json!("other.pdf")
                        }
                        ArgumentMode::ScopedCursor => {
                            call["params"]["tool"] = json!("gptgrep_catalog");
                            call["params"]["arguments"] = json!({"page_size":0,"cursor":"opaque"});
                        }
                        ArgumentMode::Artifact => {
                            call["params"]["tool"] = json!("gptgrep_read_artifact");
                            call["params"]["arguments"] =
                                json!({"artifact_id":"unknown-artifact","max_bytes":0});
                        }
                        ArgumentMode::Oversized => {
                            call["params"]["arguments"]["cursor"] = json!("x".repeat(4096))
                        }
                        _ => {}
                    }
                    emit(&mut write, call).await?;
                    let rejected = line(&mut read).await?;
                    ensure!(
                        rejected["id"] == "argument-request",
                        "argument reply identity"
                    );
                    ensure!(
                        rejected["result"]["success"] == false,
                        "argument rejection envelope"
                    );
                    let body: Value = serde_json::from_str(
                        rejected["result"]["contentItems"][0]["text"]
                            .as_str()
                            .context("argument error text")?,
                    )?;
                    ensure!(
                        body["error"]["code"] == "invalid_arguments",
                        "closed argument error code"
                    );
                    ensure!(
                        body.get("issued_id").is_none(),
                        "argument errors grant no citation"
                    );
                    ensure!(
                        body["error"]["message"]
                            .as_str()
                            .is_some_and(|message| message.contains("nonempty document")
                                && message.contains("1 through 8")),
                        "schema correction hint"
                    );
                    sent.push(rejected);
                    let corrected = json!({"id":if matches!(case,ArgumentMode::ReplayRpc){"argument-request"}else{"corrected-request"},
                        "method":"item/tool/call","params":{"threadId":"ordinary-thread","turnId":"ordinary-turn",
                        "callId":if matches!(case,ArgumentMode::ReplayCall){"argument-call"}else{"corrected-call"},
                        "namespace":"gptgrep","tool":"gptgrep_inspect_artifact","arguments":{"document":"source.pdf","page_size":1}}});
                    emit(&mut write, corrected).await?;
                    if matches!(
                        case,
                        ArgumentMode::ReplayRpc | ArgumentMode::ReplayCall | ArgumentMode::Budget
                    ) {
                        return Ok(sent);
                    }
                    let corrected = line(&mut read).await?;
                    ensure!(
                        corrected["id"] == "corrected-request"
                            && corrected["result"]["success"] == true,
                        "same-turn corrected tool succeeds"
                    );
                    sent.push(corrected);
                }
                let tool = if matches!(mode, NativeMode::Render) {
                    "gptgrep_render"
                } else {
                    "gptgrep_read_artifact"
                };
                let arguments = if matches!(mode, NativeMode::Render) {
                    json!({"resource_id":"opaque-image"})
                } else {
                    json!({"artifact_id":artifact,"offset":0,"max_bytes":2048})
                };
                emit(&mut write,json!({"id":"ordinary-request","method":"item/tool/call","params":{"threadId":"ordinary-thread","turnId":"ordinary-turn",
                    "callId":"ordinary-call","namespace":if matches!(mode,NativeMode::ForbiddenNamespace){"other"}else{"gptgrep"},"tool":tool,"arguments":arguments}})).await?;
                // The actual tool request precedes turn/start's id4 acknowledgement.
                let reply = line(&mut read).await?;
                sent.push(reply.clone());
                let content: Value = serde_json::from_str(
                    reply["result"]["contentItems"][0]["text"]
                        .as_str()
                        .context("content")?,
                )?;
                if matches!(mode, NativeMode::ReplayedId) {
                    emit(&mut write,json!({"id":"ordinary-request","method":"item/tool/call","params":{"threadId":"ordinary-thread","turnId":"ordinary-turn","callId":"another-call",
                        "namespace":"gptgrep","tool":"gptgrep_read_artifact","arguments":{"artifact_id":artifact,"offset":0,"max_bytes":2048}}})).await?;
                    return Ok(sent);
                }
                if !matches!(mode, NativeMode::Arguments(_)) {
                    emit(&mut write,json!({"id":4,"result":{"turn":{"id":if matches!(mode,NativeMode::ReselectTurn){"other-turn"}else{"ordinary-turn"}}}})).await?;
                }
                let citations = if matches!(mode, NativeMode::Render) {
                    vec![]
                } else {
                    vec![
                        json!({"issued_id":content["issued_id"],"offset":if matches!(mode,NativeMode::Ocr){6}else{7},"length":if matches!(mode,NativeMode::Ocr){6}else{3},"quote":if matches!(mode,NativeMode::Ocr){"图像"}else{"中"}}),
                    ]
                };
                if matches!(mode, NativeMode::Render) {
                    ensure!(
                        reply["result"]["success"] == false
                            && content["error"]["code"] == "capability_unavailable",
                        "typed image denial"
                    );
                } else {
                    emit(&mut write,json!({"id":-9,"method":"item/tool/call","params":{"threadId":"ordinary-thread","turnId":"ordinary-turn","callId":"second-call","namespace":"gptgrep",
                        "tool":"gptgrep_read_artifact","arguments":{"artifact_id":artifact,"offset":0,"max_bytes":2048}}})).await?;
                    let second = line(&mut read).await?;
                    ensure!(second["id"] == -9, "native int64 id retained");
                    sent.push(second);
                }
                let answer=json!({"answer":if matches!(mode,NativeMode::Render){"unavailable"}else{"claim"},"insufficient_evidence":matches!(mode,NativeMode::Render),"citations":citations}).to_string();
                emit(&mut write,json!({"method":"turn/completed","params":{"threadId":"ordinary-thread","turn":{"id":"ordinary-turn","status":"completed","error":null,"items":[{"type":"agentMessage","text":answer}]}}})).await?;
                return Ok(sent);
            }
            _ => return Err(anyhow!("unexpected ordinary mock request")),
        }
    }
}
async fn drive_fixture(
    evidence: &mut RichEvidence,
    mode: NativeMode,
) -> Result<(protocol::Outcome, Vec<Value>)> {
    let artifact = evidence.initial.as_ref().unwrap().report().hits[0]
        .window
        .evidence
        .clone();
    let artifact = match artifact {
        gptgrep_core::EvidenceRef::NativeText { binding }
        | gptgrep_core::EvidenceRef::OcrText { binding, .. } => binding.artifact_id,
        _ => unreachable!(),
    };
    let (host, server) = tokio::io::duplex(262144);
    let (read, write) = tokio::io::split(host);
    let task = tokio::spawn(native_mock(server, artifact, mode));
    let deadline = evidence.deadline;
    let config = HostConfig {
        max_tool_calls: if matches!(mode, NativeMode::Arguments(ArgumentMode::Budget)) {
            1
        } else {
            HostConfig::default().max_tool_calls
        },
        document: evidence.document.clone(),
        ..Default::default()
    };
    let result = protocol::run_observed_budget(
        read,
        write,
        Path::new("."),
        &config,
        protocol::Workflow::RichText {
            question: "source claim",
            evidence,
        },
        None,
        protocol::RunOptions::default(),
        crate::transport::TransportBudget::new(protocol::MAX_TOTAL_BYTES),
        deadline,
    )
    .await;
    match result {
        Ok(outcome) => Ok((outcome, task.await??)),
        Err(error) => {
            task.abort();
            Err(error)
        }
    }
}

pub(crate) fn assert_document_discovery_thread_start(
    frames: &[Value],
    document: Option<&str>,
) -> Result<()> {
    let start = frames
        .iter()
        .find(|frame| frame["method"] == "thread/start")
        .context("thread/start was emitted")?;
    let params = &start["params"];
    let instructions = params["baseInstructions"]
        .as_str()
        .context("base instructions")?;
    assert!(instructions.contains(DOCUMENT_DISCOVERY_GUIDANCE));
    let tools = params["dynamicTools"][0]["tools"]
        .as_array()
        .context("rich tools")?;
    for name in ["gptgrep_inspect_artifact", "gptgrep_search_text"] {
        let tool = tools
            .iter()
            .find(|tool| tool["name"] == name)
            .context("document tool")?;
        let schema = &tool["inputSchema"];
        let property = &schema["properties"]["document"];
        assert_eq!(property["type"], "string");
        let description = property["description"]
            .as_str()
            .context("document description")?;
        assert!(description.contains("documents[].path"));
        assert!(description.contains("not documents[].id"));
        assert_eq!(schema["additionalProperties"], false);
        assert_eq!(
            schema["required"],
            if name == "gptgrep_inspect_artifact" {
                json!(["document"])
            } else {
                json!(["query"])
            }
        );
        match document {
            Some(path) => {
                assert_eq!(property["enum"], json!([path]));
                let validator = jsonschema::validator_for(property)?;
                assert!(validator.is_valid(&json!(path)));
                assert!(!validator.is_valid(&json!(format!("different/{path}"))));
            }
            None => assert!(property.get("enum").is_none()),
        }
    }
    Ok(())
}

#[tokio::test]
async fn rich_document_discovery_thread_start_scoped_and_unscoped() -> Result<()> {
    for scoped in [false, true] {
        let (_dir, prepared, _execution, server) = prepared_fixture_with_scope(scoped).await?;
        let mut evidence = genuine(prepared).await?;
        assert_eq!(server.await??.len(), 2);
        let path = evidence.reader.manifest()?.documents[0]
            .document
            .path
            .clone();
        let (outcome, frames) = drive_fixture(&mut evidence, NativeMode::Normal).await?;
        assert_document_discovery_thread_start(&frames, scoped.then_some(path.as_str()))?;
        assert_eq!(evidence.finish(&outcome.answer)?.citations.len(), 1);
    }
    Ok(())
}

#[tokio::test]
async fn genuine_handle_actual_rpc_tool_before_ack_and_exact_unicode_citation() -> Result<()> {
    let (_dir, prepared, execution, server) = prepared_fixture().await?;
    let mut evidence = genuine(prepared).await?;
    assert_eq!(server.await??.len(), 2);
    let (outcome, frames) = drive_fixture(&mut evidence, NativeMode::Normal).await?;
    let final_value = evidence.finish(&outcome.answer)?;
    assert_eq!(outcome.turn_id, "ordinary-turn");
    assert_eq!(final_value.citations.len(), 1);
    let citation = &final_value.citations[0];
    assert_eq!(citation.quote, "中");
    assert_eq!(citation.host_send.observation, "host_write_and_flush");
    let binding = match &citation.evidence {
        gptgrep_core::EvidenceRef::NativeText { binding } => binding,
        _ => unreachable!(),
    };
    assert_eq!(binding.byte_start, 7);
    assert_eq!(binding.byte_end, 10);
    let frame = frames
        .iter()
        .find(|f| f["id"] == "ordinary-request")
        .unwrap();
    let mut bytes = serde_json::to_vec(frame)?;
    bytes.push(b'\n');
    assert_eq!(citation.host_send.frame_sha256, hash(&bytes));
    assert_eq!(execution.snapshot()?.logical_reservations, 2);
    assert!(evidence.delivery.as_ref().unwrap().retained_bytes() >= 262144);
    let report = evidence.into_report(
        final_value,
        vec![],
        crate::ModelUsage::default(),
        MAX_OUTPUT,
        None,
    )?;
    let json = serde_json::to_vec(&report)?;
    assert!(json.len() <= MAX_OUTPUT);
    assert!(!std::str::from_utf8(&json)?.contains("unknown_detail"));
    assert_eq!(report.initial_retrieval.validated_responses, 2);
    assert_eq!(report.citations[0].quote, "中");
    Ok(())
}
#[tokio::test]
async fn bootstrap_cannot_cite_and_render_is_explicitly_unavailable() -> Result<()> {
    for mode in [NativeMode::BootstrapCitation, NativeMode::Render] {
        let (_dir, prepared, _execution, server) = prepared_fixture().await?;
        let mut evidence = genuine(prepared).await?;
        server.await??;
        let (outcome, _) = drive_fixture(&mut evidence, mode).await?;
        if matches!(mode, NativeMode::BootstrapCitation) {
            assert!(evidence.finish(&outcome.answer).is_err());
            assert_eq!(evidence.delivery.as_ref().unwrap().retained_bytes(), 0);
        } else {
            assert!(evidence.finish(&outcome.answer)?.citations.is_empty());
        }
    }
    Ok(())
}
#[tokio::test]
async fn actual_dispatch_rejects_namespace_id_replay_and_turn_reselection() -> Result<()> {
    for mode in [
        NativeMode::ForbiddenNamespace,
        NativeMode::ReplayedId,
        NativeMode::ReselectTurn,
    ] {
        let (_dir, prepared, _execution, server) = prepared_fixture().await?;
        let mut evidence = genuine(prepared).await?;
        server.await??;
        assert!(drive_fixture(&mut evidence, mode).await.is_err());
    }
    Ok(())
}
#[tokio::test]
async fn invalid_tool_arguments_reply_then_correct_same_turn_and_preserve_citation_custody()
-> Result<()> {
    for case in [
        ArgumentMode::PageLimit,
        ArgumentMode::EmptyDocument,
        ArgumentMode::UnknownField,
    ] {
        let (_dir, prepared, execution, server) = prepared_fixture().await?;
        let mut evidence = genuine(prepared).await?;
        server.await??;
        let (outcome, frames) = drive_fixture(&mut evidence, NativeMode::Arguments(case)).await?;
        assert_eq!(outcome.turn_id, "ordinary-turn");
        assert_eq!(
            frames
                .iter()
                .filter(|frame| frame["method"] == "turn/start")
                .count(),
            1
        );
        assert_eq!(evidence.calls, 4); // Invalid inspect, corrected inspect and two reads.
        assert_eq!(
            frames
                .iter()
                .filter(|frame| frame["result"]["success"] == false)
                .count(),
            1
        );
        assert_eq!(execution.snapshot()?.logical_reservations, 2); // Original bootstrap only.
        let final_value = evidence.finish(&outcome.answer)?;
        assert_eq!(final_value.citations.len(), 1);
        assert_eq!(final_value.citations[0].quote, "中");
        let error_citation = protocol::RichCitationRequest {
            issued_id: "argument-request".into(),
            offset: 0,
            length: 5,
            quote: "alpha".into(),
        };
        assert!(
            evidence
                .delivery
                .as_ref()
                .unwrap()
                .resolve(&[error_citation], &evidence.reader, 8)
                .is_err()
        );
        let report = evidence.into_report(
            final_value,
            vec![],
            crate::ModelUsage::default(),
            MAX_OUTPUT,
            None,
        )?;
        assert_eq!(report.tool_calls, 4);
    }
    Ok(())
}

#[tokio::test]
async fn invalid_tool_arguments_consume_budget_and_both_replay_ids() -> Result<()> {
    for (case, code) in [
        (ArgumentMode::ReplayRpc, "host_rich_call_replayed"),
        (ArgumentMode::ReplayCall, "host_rich_call_replayed"),
        (ArgumentMode::Budget, "host_rich_tool_budget"),
    ] {
        let (_dir, prepared, _execution, server) = prepared_fixture().await?;
        let mut evidence = genuine(prepared).await?;
        server.await??;
        let error = drive_fixture(&mut evidence, NativeMode::Arguments(case))
            .await
            .err()
            .context("terminal replay or budget")?;
        assert_eq!(error.to_string(), code);
        assert_eq!(evidence.calls, 1);
        assert_eq!(evidence.delivery.as_ref().unwrap().retained_bytes(), 0);
    }
    Ok(())
}

#[tokio::test]
async fn invalid_tool_arguments_do_not_recover_scope_or_size_failures() -> Result<()> {
    for (case, calls, code) in [
        (
            ArgumentMode::ForeignThread,
            0,
            "Dynamic tool call came from a different thread",
        ),
        (
            ArgumentMode::ForeignTurn,
            0,
            "Codex event changed turn identity",
        ),
        (ArgumentMode::Namespace, 0, "host_rich_tool_allowlist"),
        (ArgumentMode::Document, 1, "host_rich_document_scope"),
        (
            ArgumentMode::ScopedCursor,
            1,
            "host_rich_scoped_catalog_cursor",
        ),
        (ArgumentMode::Artifact, 1, "host_rich_artifact_unavailable"),
        (ArgumentMode::Oversized, 0, "host_rich_output_budget"),
    ] {
        let dir = tempfile::tempdir()?;
        let root = dir.path().canonicalize()?;
        publish(&root, &["alpha\r\n中 tail\n"]).await?;
        let (mut evidence, _execution) =
            native_metadata_evidence(RichGenerationReader::open(&root)?, true).await?;
        let error = drive_fixture(&mut evidence, NativeMode::Arguments(case))
            .await
            .err()
            .context("terminal scope or size")?;
        assert!(error.to_string().contains(code), "{error}");
        assert_eq!(evidence.calls, calls);
        assert_eq!(evidence.delivery.as_ref().unwrap().retained_bytes(), 0);
    }
    Ok(())
}

#[tokio::test]
async fn whole_bounds_and_absent_finite_context_fail_before_admission() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path().canonicalize()?;
    publish(&root, &["alpha\n"]).await?;
    let reader = RichGenerationReader::open(&root)?;
    let execution = context(2)?;
    let client = client("http://127.0.0.1:1/decisions", execution.clone())?;
    let options = RichTextHostOptions {
        max_output_bytes: 1,
        ..Default::default()
    };
    assert!(
        prepare_rich_text_host(
            reader.clone(),
            client,
            "question",
            &HostConfig::default(),
            &options
        )
        .is_err()
    );
    assert_eq!(execution.snapshot()?.logical_reservations, 0);
    let legacy =
        JevClient::with_endpoint("ordinary-fixture", None, "http://127.0.0.1:1/decisions")?;
    assert!(
        prepare_rich_text_host(
            reader,
            legacy,
            "question",
            &HostConfig::default(),
            &RichTextHostOptions::default()
        )
        .is_err()
    );
    Ok(())
}

#[derive(Clone, Copy)]
enum WriterFailure {
    Partial,
    Flush,
    Stale,
}
struct DeliveryWriter<W> {
    inner: W,
    mode: WriterFailure,
    path: std::path::PathBuf,
    delivery: bool,
    failed: bool,
    bytes: Arc<std::sync::Mutex<Vec<u8>>>,
}
impl<W: tokio::io::AsyncWrite + Unpin> tokio::io::AsyncWrite for DeliveryWriter<W> {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        bytes: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        if bytes
            .windows(b"gptgrep.rich_text.delivery.v1".len())
            .any(|b| b == b"gptgrep.rich_text.delivery.v1")
        {
            self.delivery = true;
        }
        if self.failed {
            return std::task::Poll::Ready(Err(std::io::Error::other(
                "ordinary selected partial writer",
            )));
        }
        let limit = if self.delivery && matches!(self.mode, WriterFailure::Partial) {
            bytes.len().min(17)
        } else {
            bytes.len()
        };
        let result = std::pin::Pin::new(&mut self.inner).poll_write(cx, &bytes[..limit]);
        if let std::task::Poll::Ready(Ok(n)) = result {
            self.bytes.lock().unwrap().extend_from_slice(&bytes[..n]);
            if self.delivery && matches!(self.mode, WriterFailure::Partial) {
                self.failed = true;
            }
        }
        result
    }
    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        if self.delivery && matches!(self.mode, WriterFailure::Flush) {
            return std::task::Poll::Ready(Err(std::io::Error::other(
                "ordinary selected flush failure",
            )));
        }
        let result = std::pin::Pin::new(&mut self.inner).poll_flush(cx);
        if self.delivery
            && !self.failed
            && matches!(self.mode, WriterFailure::Stale)
            && matches!(result, std::task::Poll::Ready(Ok(())))
        {
            fs::write(&self.path, b"changed synthetic source").unwrap();
            self.failed = true;
        }
        result
    }
    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}
#[tokio::test]
async fn partial_write_flush_failure_and_post_write_staleness_never_register_citations()
-> Result<()> {
    for mode in [
        WriterFailure::Partial,
        WriterFailure::Flush,
        WriterFailure::Stale,
    ] {
        let (_dir, prepared, _execution, server) = prepared_fixture().await?;
        let mut evidence = genuine(prepared).await?;
        server.await??;
        let artifact = evidence.initial.as_ref().unwrap().report().hits[0]
            .window
            .evidence
            .clone();
        let artifact = match artifact {
            gptgrep_core::EvidenceRef::NativeText { binding } => binding.artifact_id,
            _ => unreachable!(),
        };
        let (host, server) = tokio::io::duplex(262144);
        let (read, inner) = tokio::io::split(host);
        let path = evidence.reader.manifest()?.root.join("source.pdf");
        let captured = Arc::new(std::sync::Mutex::new(Vec::new()));
        let writer = DeliveryWriter {
            inner,
            mode,
            path,
            delivery: false,
            failed: false,
            bytes: captured.clone(),
        };
        let task = tokio::spawn(native_mock(server, artifact, NativeMode::Normal));
        let deadline = evidence.deadline;
        let budget = crate::transport::TransportBudget::new(protocol::MAX_TOTAL_BYTES);
        let result = protocol::run_observed_budget(
            read,
            writer,
            Path::new("."),
            &HostConfig::default(),
            protocol::Workflow::RichText {
                question: "source claim",
                evidence: &mut evidence,
            },
            None,
            protocol::RunOptions::default(),
            budget.clone(),
            deadline,
        )
        .await;
        assert!(result.is_err());
        task.abort();
        assert!(budget.check_host().is_err());
        let handle = evidence.delivery.as_ref().unwrap();
        assert_eq!(handle.unknown_entries(), 1);
        assert!(handle.retained_bytes() >= 131072);
        if !matches!(mode, WriterFailure::Partial) {
            let bytes = captured.lock().unwrap();
            let frame = bytes
                .split(|b| *b == b'\n')
                .filter_map(|line| serde_json::from_slice::<Value>(line).ok())
                .find(|f| f["id"] == "ordinary-request")
                .unwrap();
            let content: Value =
                serde_json::from_str(frame["result"]["contentItems"][0]["text"].as_str().unwrap())?;
            let request = protocol::RichCitationRequest {
                issued_id: content["issued_id"].as_str().unwrap().into(),
                offset: 0,
                length: 5,
                quote: "alpha".into(),
            };
            assert!(handle.resolve(&[request], &evidence.reader, 8).is_err());
        }
    }
    Ok(())
}
#[tokio::test]
async fn source_scope_closed_arguments_and_final_freshness_are_enforced() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path().canonicalize()?;
    publish(&root, &["alpha\r\n中 tail\n"]).await?;
    let (endpoint, server) = mock(2, Arc::new(|_| Ok(())), false).await?;
    let config = HostConfig {
        document: Some("source.pdf".into()),
        ..Default::default()
    };
    let mut options = RichTextHostOptions::default();
    options.retrieval.document = config.document.clone();
    let prepared = prepare_rich_text_host(
        RichGenerationReader::open(&root)?,
        client(&endpoint, context(2)?)?,
        "source claim",
        &config,
        &options,
    )?;
    let mut evidence = genuine(prepared).await?;
    server.await??;
    assert!(
        validate_arguments(
            "gptgrep_read_artifact",
            &json!({"artifact_id":"x","offset":0,"max_bytes":1024,"extra":true})
        )
        .is_err()
    );
    assert!(
        evidence
            .metadata_tool("gptgrep_inspect_artifact", &json!({"document":"other.pdf"}))
            .is_err()
    );
    assert!(
        evidence
            .metadata_tool(
                "gptgrep_search_text",
                &json!({"query":"claim","document":"other.pdf"})
            )
            .is_err()
    );
    let catalog = evidence.metadata_tool("gptgrep_catalog", &json!({}))?;
    let catalog: Value =
        serde_json::from_str(catalog["contentItems"][0]["text"].as_str().unwrap())?;
    assert_eq!(catalog["result"]["documents"][0]["path"], "source.pdf");
    let (outcome, _) = drive_fixture(&mut evidence, NativeMode::Normal).await?;
    assert!(evidence.finish(&outcome.answer).is_ok());
    fs::write(
        evidence.reader.manifest()?.root.join("source.pdf"),
        b"new source bytes",
    )?;
    assert!(evidence.finish(&outcome.answer).is_err());
    Ok(())
}
#[tokio::test]
async fn final_cpu_gap_source_check_preserves_partial_usage_and_unknown_registry() -> Result<()> {
    let (_dir, prepared, _execution, server) = prepared_fixture().await?;
    let mut evidence = genuine(prepared).await?;
    server.await??;
    let (outcome, _) = drive_fixture(&mut evidence, NativeMode::Normal).await?;
    let final_value = evidence.finish(&outcome.answer)?;
    fs::write(
        evidence.reader.manifest()?.root.join("source.pdf"),
        b"changed after final answer",
    )?;
    let error = evidence
        .into_report(
            final_value,
            vec![],
            crate::ModelUsage::default(),
            MAX_OUTPUT,
            None,
        )
        .err()
        .unwrap();
    assert_eq!(error.code, "host_rich_completion_rejected");
    assert_eq!(error.initial_retrieval.unwrap().validated_responses, 2);
    assert!(error.retained_text_registry_bytes >= 262144);
    Ok(())
}

fn fixture_artifact(
    parsed: &mut RichParsedDocument,
    staging: &Path,
    kind: ArtifactKind,
    page: Option<u32>,
    bytes: &[u8],
    name: &str,
    mime: &str,
) -> Result<ArtifactRef> {
    let artifact = ArtifactRef {
        id: artifact_id(
            &parsed.summary.source_sha256,
            &parsed.native_profile_sha256,
            &kind,
            page,
            None,
            &hash(bytes),
        )?,
        kind,
        relative_path: name.into(),
        sha256: hash(bytes),
        profile_sha256: parsed.native_profile_sha256.clone(),
        bytes: bytes.len() as u64,
        mime: mime.into(),
        page,
        region_id: None,
    };
    fs::write(staging.join(name), bytes)?;
    parsed.artifacts.push(artifact.clone());
    Ok(artifact)
}
async fn publish_saved_ocr(root: &Path) -> Result<GenerationPin> {
    let (staging, mut parsed) = candidate(root, &["Native description\n"]).await?;
    let staging = staging.path().canonicalize()?;
    // Synthetic source/receipt DTO only: no OCR execution or image qualification.
    let mut png = Vec::new();
    image::write_buffer_with_format(
        &mut std::io::Cursor::new(&mut png),
        &[255u8; 24],
        2,
        4,
        image::ColorType::Rgb8,
        image::ImageFormat::Png,
    )?;
    let rendition = fixture_artifact(
        &mut parsed,
        &staging,
        ArtifactKind::Rendition,
        Some(1),
        &png,
        "page.png",
        "image/png",
    )?;
    let image = OcrImage {
        artifact: rendition,
        path: staging.join("page.png"),
        width: 2,
        height: 4,
        page_to_raster: AffineTransform {
            a: 0.02,
            b: 0.0,
            c: 0.0,
            d: 0.02,
            e: 0.0,
            f: 0.0,
        },
    };
    parsed.pages[0].renditions = vec![image.clone()];
    parsed.pages[0].coverage = CoverageState::ValidatedOcr;
    parsed.coverage = vec![(1, CoverageState::ValidatedOcr)];
    parsed.pages[0].complexity.needs_ocr = true;
    parsed.pages[0]
        .capabilities
        .insert("annotation_count".into(), FeatureState::Present("1".into()));
    let old = parsed.pages[0].metadata_artifact.id.clone();
    parsed.artifacts.retain(|a| a.id != old);
    let metadata = serde_json::to_vec(
        &json!({"page":{"page_number":1,"page_width":100.0,"page_height":200.0,"text_items":[]},"objects":[],
        "capabilities":parsed.pages[0].capabilities,"complexity":parsed.pages[0].complexity,"geometry":parsed.pages[0].geometry,"image_rects":[]}),
    )?;
    parsed.pages[0].metadata_artifact = fixture_artifact(
        &mut parsed,
        &staging,
        ArtifactKind::NativeMetadata,
        Some(1),
        &metadata,
        "ocr-native.json",
        "application/json",
    )?;
    let mut task = OcrTask {
        schema_version: "gptgrep.ocr.task.v1".into(),
        id: String::new(),
        task_sha256: String::new(),
        source_sha256: parsed.summary.source_sha256.clone(),
        prepared_sha256: parsed.summary.prepared_sha256.clone(),
        source_path: root.join("source.pdf"),
        page: 1,
        page_width_points: 100.0,
        page_height_points: 200.0,
        regions: vec![OcrRegion {
            id: "p1-required".into(),
            bbox: BoundingBox {
                x: 0.0,
                y: 0.0,
                width: 100.0,
                height: 200.0,
            },
            required: true,
            reason: format!(
                "required visible coverage; native complexity={:?}; annotation count=1",
                parsed.pages[0].complexity.reasons
            ),
        }],
        images: vec![image.clone()],
        native_text: "Native description\n".into(),
        layout_context: String::new(),
        profile: OcrProfile::default(),
        budget: OcrTaskBudget::default(),
    };
    task.task_sha256 = ocr_task_digest(&task)?;
    task.id = format!("ocr-{}", task.task_sha256);
    let raw = "Raw\r\n\t图像 label  ";
    let outcome = OcrPageResult {
        schema_version: "gptgrep.ocr.result.v1".into(),
        task_id: task.id.clone(),
        task_sha256: task.task_sha256.clone(),
        source_sha256: task.source_sha256.clone(),
        prepared_sha256: task.prepared_sha256.clone(),
        page: 1,
        status: OcrResultStatus::Complete,
        regions: vec![OcrRegionResult {
            id: "p1-required".into(),
            bbox: task.regions[0].bbox,
            text: raw.into(),
            reading_order: 0,
            status: OcrRegionStatus::Complete,
        }],
        receipt: OcrReceipt {
            model: task.profile.model.clone(),
            profile_sha256: ocr_profile_digest(&task.profile)?,
            request_sha256: "1".repeat(64),
            response_sha256: "2".repeat(64),
            runtime_profile_sha256: "3".repeat(64),
            image_deliveries: vec![OcrImageDelivery {
                artifact_id: image.artifact.id.clone(),
                artifact_sha256: image.artifact.sha256.clone(),
                tool_ordinal: 1,
                mime: "image/png".into(),
                width: 2,
                height: 4,
                tool_response_sha256: "4".repeat(64),
                native: None,
            }],
            usage: None,
            latency_ms: None,
        },
    };
    fixture_artifact(
        &mut parsed,
        &staging,
        ArtifactKind::OcrRegions,
        Some(1),
        &serde_json::to_vec(&outcome)?,
        "ocr-regions.json",
        "application/json",
    )?;
    fixture_artifact(
        &mut parsed,
        &staging,
        ArtifactKind::OcrText,
        Some(1),
        format!("{raw}\n").as_bytes(),
        "ocr.txt",
        "text/plain; charset=utf-8",
    )?;
    parsed.ocr_tasks = vec![task];
    parsed.ocr_results = vec![outcome];
    let mut writer = RichGenerationWriter::begin(root, ResourceBudget::default())?;
    writer.add_document("source.pdf", &staging, parsed)?;
    writer.publish()
}

#[tokio::test]
async fn saved_ocr_text_delivery_retains_raw_crlf_regions_and_receipt_binding() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path().canonicalize()?;
    publish_saved_ocr(&root).await?;
    let reader = RichGenerationReader::open(&root)?;
    let artifact = reader.manifest()?.documents[0]
        .artifacts
        .iter()
        .find(|a| a.artifact.kind == ArtifactKind::OcrText)
        .unwrap()
        .artifact
        .id
        .clone();
    let (endpoint, server) = mock(2, Arc::new(|_| Ok(())), false).await?;
    let prepared = prepare_rich_text_host(
        reader,
        client(&endpoint, context(2)?)?,
        "source claim",
        &HostConfig::default(),
        &RichTextHostOptions::default(),
    )?;
    let mut evidence = genuine(prepared).await?;
    server.await??;
    let (host, stream) = tokio::io::duplex(262144);
    let (read, write) = tokio::io::split(host);
    // Synthetic OCR DTO through the actual RPC read and final answer; no OCR execution.
    let task = tokio::spawn(native_mock(stream, artifact, NativeMode::Ocr));
    let deadline = evidence.deadline;
    let outcome = protocol::run_observed_budget(
        read,
        write,
        Path::new("."),
        &HostConfig::default(),
        protocol::Workflow::RichText {
            question: "source claim",
            evidence: &mut evidence,
        },
        None,
        protocol::RunOptions::default(),
        crate::transport::TransportBudget::new(protocol::MAX_TOTAL_BYTES),
        deadline,
    )
    .await?;
    let frames = task.await??;
    let reply = frames
        .iter()
        .find(|f| f["id"] == "ordinary-request")
        .unwrap();
    let content: Value =
        serde_json::from_str(reply["result"]["contentItems"][0]["text"].as_str().unwrap())?;
    assert_eq!(content["window"]["text"], "Raw\r\n\t图像 label  \n");
    let final_value = evidence.finish(&outcome.answer)?;
    assert_eq!(outcome.turn_id, "ordinary-turn");
    match &final_value.citations[0].evidence {
        gptgrep_core::EvidenceRef::OcrText {
            binding,
            region_ids,
            ocr_profile_sha256,
            ocr_receipt_sha256,
        } => {
            assert_eq!(binding.byte_start, 6);
            assert_eq!(binding.byte_end, 12);
            assert_eq!(region_ids, &vec!["p1-required".to_owned()]);
            assert_eq!(ocr_profile_sha256.len(), 64);
            assert_eq!(ocr_receipt_sha256.len(), 64);
        }
        _ => panic!("saved OCR ref must remain OCR"),
    }
    Ok(())
}
#[tokio::test]
async fn scoped_read_never_widens_to_another_actual_document() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path().canonicalize()?;
    let (other_stage, other) = candidate(&root, &["other text\n"]).await?;
    fs::rename(root.join("source.pdf"), root.join("other.pdf"))?;
    let (source_stage, source) = candidate(&root, &["alpha\r\n中 tail\n"]).await?;
    let mut writer = RichGenerationWriter::begin(&root, ResourceBudget::default())?;
    writer.add_document("source.pdf", &source_stage.path().canonicalize()?, source)?;
    writer.add_document("other.pdf", &other_stage.path().canonicalize()?, other)?;
    writer.publish()?;
    let reader = RichGenerationReader::open(&root)?;
    let other_id = reader
        .manifest()?
        .documents
        .iter()
        .find(|d| d.document.path == "other.pdf")
        .unwrap()
        .pages[0]
        .native_text
        .id
        .clone();
    let (endpoint, server) = mock(2, Arc::new(|_| Ok(())), false).await?;
    let config = HostConfig {
        document: Some("source.pdf".into()),
        ..Default::default()
    };
    let mut options = RichTextHostOptions::default();
    options.retrieval.document = config.document.clone();
    let mut evidence = genuine(prepare_rich_text_host(
        reader,
        client(&endpoint, context(2)?)?,
        "source claim",
        &config,
        &options,
    )?)
    .await?;
    server.await??;
    assert!(evidence.read(&json!({"artifact_id":other_id})).is_err());
    let source_id = evidence.initial.as_ref().unwrap().report().hits[0]
        .window
        .evidence
        .clone();
    let source_id = match source_id {
        gptgrep_core::EvidenceRef::NativeText { binding } => binding.artifact_id,
        _ => unreachable!(),
    };
    assert!(evidence.read(&json!({"artifact_id":source_id})).is_ok());
    let (_outcome, _frames) = drive_fixture(&mut evidence, NativeMode::Normal).await?;
    Ok(())
}

#[test]
#[ignore = "requires an explicitly selected generated app-server schema fixture"]
fn selected_generated_schema_validates_text_tools_and_native_request_ids() -> Result<()> {
    let path =
        std::env::var_os("GPTGREP_RICH_TEXT_SCHEMA_FIXTURE").context("selected schema fixture")?;
    let path = std::path::PathBuf::from(path);
    ensure!(
        path.metadata()?.len() <= 8 * 1024 * 1024,
        "schema fixture byte cap"
    );
    let mut schema: Value = serde_json::from_slice(&fs::read(path)?)?;
    for tool in tools(None).as_array().unwrap() {
        schema["$ref"] = json!("#/definitions/v2/DynamicToolSpec");
        jsonschema::validator_for(&schema)?
            .validate(tool)
            .map_err(|_| anyhow!("generated dynamic-tool schema mismatch"))?;
    }
    schema["$ref"] = json!("#/definitions/DynamicToolCallResponse");
    jsonschema::validator_for(&schema)?
        .validate(&tool_error("capability_unavailable"))
        .map_err(|_| anyhow!("generated content-items schema mismatch"))?;
    schema["$ref"] = json!("#/definitions/RequestId");
    let validator = jsonschema::validator_for(&schema)?;
    for id in [
        json!("opaque-string"),
        json!(-9),
        json!(i64::MIN),
        json!(i64::MAX),
    ] {
        assert!(validator.is_valid(&id));
    }
    assert!(!validator.is_valid(&json!(1.5)));
    assert!(!validator.is_valid(&Value::Null));
    Ok(())
}

// Same ordinary source/mock/Rpc entrypoint as the fixtures above. The binary
// parameter deliberately names an absent owned path; expiry must stop before
// any native launch, and these tests never create a child runtime.
async fn deadline_fixture(
    response_delay: Duration,
) -> Result<(
    tempfile::TempDir,
    PreparedRichTextHost,
    ExecutionContext,
    Arc<std::sync::atomic::AtomicUsize>,
    tokio::task::JoinHandle<Result<Vec<Value>>>,
)> {
    let dir = tempfile::tempdir()?;
    let root = dir.path().canonicalize()?;
    publish(&root, &["alpha\r\n中 tail\n"]).await?;
    let execution = context(2)?;
    let observed = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let calls = observed.clone();
    let (endpoint, server) = mock_with_score_and_delay(
        2,
        Arc::new(move |_| {
            calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        }),
        false,
        3.0,
        response_delay,
    )
    .await?;
    let config = HostConfig {
        timeout_secs: 1,
        codex_bin: root
            .join("not-a-native-runtime")
            .to_string_lossy()
            .into_owned(),
        codex_home: root.join("unused-owned-home"),
        ..Default::default()
    };
    let prepared = prepare_rich_text_host(
        RichGenerationReader::open(&root)?,
        client(&endpoint, execution.clone())?,
        "source claim",
        &config,
        &RichTextHostOptions::default(),
    )?;
    Ok((dir, prepared, execution, observed, server))
}
#[tokio::test]
async fn captured_host_deadline_blocks_delayed_execution_before_any_admission() -> Result<()> {
    let (_dir, prepared, execution, observed, server) = deadline_fixture(Duration::ZERO).await?;
    tokio::time::sleep(Duration::from_millis(1100)).await;
    let error = prepared
        .execute(&|_| Ok(()))
        .await
        .err()
        .expect("expired host cannot succeed");
    server.abort();
    let _ = server.await;
    let snapshot = execution.snapshot()?;
    assert_eq!(snapshot.submission_observations, 0);
    assert_eq!(snapshot.logical_reservations, 0);
    assert_eq!(snapshot.send_start_observations, 0);
    assert_eq!(observed.load(std::sync::atomic::Ordering::SeqCst), 0);
    assert!(error.model_attempts.is_empty());
    assert_eq!(error.unknown_text_deliveries, 0);
    assert!(execution.original_run_remaining()? > Duration::from_secs(50));
    Ok(())
}

// The existing graphics fixture supplies native objects, paths, an image and
// text. IDs and row counts come from the published generation, never test gold
// injected into a product selector. These tests need admitted PDFium execution;
// the protocol peer below is in-memory and is not a native model observation.
// Offline coverage makes the real graphics fixture publishable; it is not a model receipt.
fn native_metadata_offline_ocr(task: &OcrTask) -> Result<OcrPageResult> {
    Ok(OcrPageResult {
        schema_version: "gptgrep.ocr.result.v1".into(),
        task_id: task.id.clone(),
        task_sha256: task.task_sha256.clone(),
        source_sha256: task.source_sha256.clone(),
        prepared_sha256: task.prepared_sha256.clone(),
        page: task.page,
        status: OcrResultStatus::Complete,
        regions: task
            .regions
            .iter()
            .enumerate()
            .map(|(i, r)| OcrRegionResult {
                id: r.id.clone(),
                bbox: r.bbox,
                text: "Authored offline host fixture transcription.".into(),
                reading_order: i as u32,
                status: OcrRegionStatus::Complete,
            })
            .collect(),
        receipt: OcrReceipt {
            model: task.profile.model.clone(),
            profile_sha256: ocr_profile_digest(&task.profile)?,
            request_sha256: "1".repeat(64),
            response_sha256: "2".repeat(64),
            runtime_profile_sha256: "3".repeat(64),
            image_deliveries: task
                .images
                .iter()
                .enumerate()
                .map(|(i, image)| OcrImageDelivery {
                    artifact_id: image.artifact.id.clone(),
                    artifact_sha256: image.artifact.sha256.clone(),
                    tool_ordinal: i as u32 + 1,
                    mime: image.artifact.mime.clone(),
                    width: image.width,
                    height: image.height,
                    tool_response_sha256: "4".repeat(64),
                    native: None,
                })
                .collect(),
            usage: None,
            latency_ms: None,
        },
    })
}
pub(crate) async fn native_metadata_generation(
    include_other: bool,
) -> Result<(tempfile::TempDir, RichGenerationReader)> {
    let temp = tempfile::tempdir()?;
    let root = temp.path().canonicalize()?;
    let budget = ResourceBudget::default();
    let mut writer = RichGenerationWriter::begin(&root, budget.clone())?;
    for name in if include_other {
        vec!["source.pdf", "other.pdf"]
    } else {
        vec!["source.pdf"]
    } {
        let mut bytes = include_bytes!("../../../../tests/fixtures/native-graphics.pdf").to_vec();
        if name == "other.pdf" {
            bytes.extend_from_slice(b"\n% independent source identity\n");
        }
        fs::write(root.join(name), bytes)?;
        let prepared = prepare_document(
            &root.join(name),
            &root.join(format!("stage-{name}")),
            &PreparationOptions,
            &budget,
        )
        .await?;
        let candidate = extract_native(
            &prepared,
            &NativeOptions {
                dpi: 72.,
                render_pages: true,
                page_window: 1,
            },
            &budget,
        )
        .await?;
        let outcomes = candidate
            .plan()
            .tasks
            .iter()
            .map(native_metadata_offline_ocr)
            .collect::<Result<Vec<_>>>()?;
        let parsed = finalize_document(candidate, outcomes, OcrMode::Auto, &budget)?;
        writer.add_document(name, prepared.staging_root(), parsed)?;
    }
    writer.publish()?;
    Ok((temp, RichGenerationReader::open(&root)?))
}

fn native_metadata_config(scoped: bool) -> HostConfig {
    HostConfig {
        max_tool_calls: 64,
        document: scoped.then(|| "source.pdf".into()),
        ..Default::default()
    }
}
async fn native_metadata_evidence(
    reader: RichGenerationReader,
    scoped: bool,
) -> Result<(RichEvidence, ExecutionContext)> {
    let execution = context(2)?;
    let (endpoint, server) = mock(2, Arc::new(|_| Ok(())), false).await?;
    let config = native_metadata_config(scoped);
    let mut options = RichTextHostOptions::default();
    options.retrieval.document = config.document.clone();
    let evidence = genuine(prepare_rich_text_host(
        reader,
        client(&endpoint, execution.clone())?,
        "source claim",
        &config,
        &options,
    )?)
    .await?;
    assert_eq!(server.await??.len(), 2);
    Ok((evidence, execution))
}

async fn metadata_call(
    read: &mut (impl tokio::io::AsyncBufRead + Unpin),
    write: &mut (impl tokio::io::AsyncWrite + Unpin),
    frames: &mut Vec<Value>,
    ordinal: &mut usize,
    tool: &str,
    arguments: Value,
) -> Result<Value> {
    ensure!(*ordinal < 64, "metadata fixture call bound");
    let id = format!("metadata-call-{ordinal}");
    *ordinal += 1;
    emit(
        write,
        json!({"id":id,"method":"item/tool/call","params":{
        "threadId":"metadata-thread","turnId":"metadata-turn","callId":id,
        "namespace":"gptgrep","tool":tool,"arguments":arguments}}),
    )
    .await?;
    let frame = line(read).await?;
    ensure!(frame["id"] == id, "metadata RPC identity");
    let body = serde_json::from_str(
        frame["result"]["contentItems"][0]["text"]
            .as_str()
            .context("metadata tool inputText")?,
    )?;
    frames.push(frame);
    Ok(body)
}

pub(crate) async fn native_metadata_rpc_peer(
    stream: tokio::io::DuplexStream,
    override_call: Option<(&'static str, Value)>,
    all_collections: bool,
) -> Result<Vec<Value>> {
    let (read, mut write) = tokio::io::split(stream);
    let mut read = BufReader::new(read);
    let mut frames = Vec::new();
    loop {
        let request = line(&mut read).await?;
        match request["method"].as_str() {
            Some("initialize") => emit(&mut write, json!({"id":1,"result":{}})).await?,
            Some("initialized") => {}
            Some("account/read") => {
                emit(
                    &mut write,
                    json!({"id":100,"result":{"account":{"type":"chatgpt"}}}),
                )
                .await?;
            }
            Some("config/read") => {
                emit(
                    &mut write,
                    json!({"id":2,"result":{"config":{"mcp_servers":{}}}}),
                )
                .await?;
            }
            Some("thread/start") => {
                assert!(
                    request["params"]["dynamicTools"][0]["tools"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|tool| tool["name"] == NATIVE_METADATA_TOOL)
                );
                emit(&mut write, json!({"id":3,"result":{"thread":{"id":"metadata-thread"},
                    "model":request["params"]["model"],"modelProvider":"openai","approvalPolicy":"never",
                    "sandbox":{"type":"readOnly","networkAccess":false},"reasoningEffort":"max","serviceTier":"priority"}})).await?;
            }
            Some("turn/start") => {
                let mut ordinal = 0;
                // Discovery and the first metadata call arrive before turn ACK.
                let discovery = metadata_call(
                    &mut read,
                    &mut write,
                    &mut frames,
                    &mut ordinal,
                    "gptgrep_inspect_artifact",
                    json!({"document":"source.pdf"}),
                )
                .await?;
                let artifact = discovery["result"]["items"][0]["result"]["objects"][0]
                    ["metadata_artifact"]["id"].as_str().context("discovered page metadata")?.to_owned();
                if let Some((tool, arguments)) = override_call {
                    metadata_call(
                        &mut read,
                        &mut write,
                        &mut frames,
                        &mut ordinal,
                        tool,
                        arguments,
                    )
                    .await?;
                } else if all_collections {
                    let mut path_id = None;
                    let mut binding = None;
                    for collection in [
                        "object_summaries",
                        "native_characters",
                        "native_words",
                        "image_geometry",
                        "path_segments",
                    ] {
                        let mut cursor = Value::Null;
                        let mut end = 0;
                        loop {
                            let mut arguments = json!({"artifact_id":artifact,"collection":collection,
                                "fields":["counts"],"page_size":2,"cursor":cursor});
                            if collection == "object_summaries" {
                                arguments["item_fields"] = json!(["id", "kind", "ordinal"]);
                            }
                            if collection == "path_segments" {
                                arguments["object_id"] =
                                    json!(path_id.as_ref().context("discovered path")?);
                            }
                            let body = metadata_call(
                                &mut read,
                                &mut write,
                                &mut frames,
                                &mut ordinal,
                                NATIVE_METADATA_TOOL,
                                arguments,
                            )
                            .await?;
                            assert_eq!(frames.last().unwrap()["result"]["success"], true);
                            assert_eq!(
                                body["citation_status"],
                                "not_registered_read_artifact_required"
                            );
                            let item = &body["result"]["items"][0];
                            let value = &item["result"];
                            assert_eq!(value["metadata_only"], true);
                            assert_eq!(value["citation_authority"], false);
                            assert!(value.get("issued_id").is_none());
                            if let Some(expected) = &binding {
                                assert_eq!(&value["binding"], expected);
                            } else {
                                binding = Some(value["binding"].clone());
                            }
                            let rows = value["rows"].as_array().context("whole rows")?;
                            assert_eq!(item["coverage"]["start"], end);
                            end += rows.len();
                            assert_eq!(item["coverage"]["end"], end);
                            if collection == "object_summaries" {
                                for row in rows {
                                    if row["kind"] == "path" {
                                        path_id =
                                            Some(row["id"].as_str().context("path ID")?.to_owned());
                                    }
                                }
                            }
                            if item["coverage"]["exhausted"] == true {
                                assert_eq!(item["coverage"]["available"], end);
                                assert!(end > 0, "fixture supplies each collection");
                                assert!(item["coverage"].get("cursor").is_none());
                                break;
                            }
                            assert!(!rows.is_empty());
                            cursor = item["coverage"]["cursor"].clone();
                            assert!(cursor.as_str().is_some());
                        }
                    }
                } else {
                    metadata_call(
                        &mut read,
                        &mut write,
                        &mut frames,
                        &mut ordinal,
                        NATIVE_METADATA_TOOL,
                        json!({"artifact_id":artifact,"collection":"native_characters",
                            "fields":["counts"],"page_size":1}),
                    )
                    .await?;
                }
                emit(
                    &mut write,
                    json!({"id":4,"result":{"turn":{"id":"metadata-turn"}}}),
                )
                .await?;
                let answer = json!({"answer":"Metadata inspected; no source window was delivered.",
                    "insufficient_evidence":true,"citations":[]})
                .to_string();
                emit(
                    &mut write,
                    json!({"method":"turn/completed","params":{"threadId":"metadata-thread",
                    "turn":{"id":"metadata-turn","status":"completed","error":null,
                    "items":[{"type":"agentMessage","text":answer}]}}}),
                )
                .await?;
                return Ok(frames);
            }
            _ => return Err(anyhow!("unexpected metadata fixture request")),
        }
    }
}

async fn drive_native_metadata(
    evidence: &mut RichEvidence,
    override_call: Option<(&'static str, Value)>,
    all_collections: bool,
) -> Result<(protocol::Outcome, Vec<Value>)> {
    let (host, server) = tokio::io::duplex(262144);
    let (read, write) = tokio::io::split(host);
    let peer = tokio::spawn(native_metadata_rpc_peer(
        server,
        override_call,
        all_collections,
    ));
    let config = native_metadata_config(evidence.document.is_some());
    let deadline = evidence.deadline;
    let result = protocol::run_observed_budget(
        read,
        write,
        Path::new("."),
        &config,
        protocol::Workflow::RichText {
            question: "source claim",
            evidence,
        },
        None,
        protocol::RunOptions::default(),
        crate::transport::TransportBudget::new(protocol::MAX_TOTAL_BYTES),
        deadline,
    )
    .await;
    match result {
        Ok(outcome) => Ok((outcome, peer.await??)),
        Err(error) => {
            peer.abort();
            let _ = peer.await;
            Err(error)
        }
    }
}

#[test]
fn native_metadata_closed_arguments_and_tool_specific_limits() -> Result<()> {
    let valid = json!({"artifact_id":"a".repeat(64),"collection":"native_characters"});
    let parsed = NativeMetadataArgs::parse(&valid)?;
    assert_eq!((parsed.page_size, parsed.max_output_bytes), (100, 16384));
    for patch in [
        json!({"unknown":0}),
        json!({"document":"other.pdf"}),
        json!({"collection":"unknown"}),
        json!({"page_size":0}),
        json!({"page_size":1001}),
        json!({"max_output_bytes":0}),
        json!({"max_output_bytes":16385}),
        json!({"fields":["unknown"]}),
        json!({"fields":["counts","counts"]}),
        json!({"item_fields":["page_point"]}),
        json!({"object_id":"a".repeat(64)}),
        json!({"collection":"path_segments"}),
        json!({"cursor":"A0"}),
        json!({"cursor":"a"}),
        json!({"cursor":"aa".repeat(8193)}),
    ] {
        let mut invalid = valid.clone();
        invalid
            .as_object_mut()
            .unwrap()
            .extend(patch.as_object().unwrap().clone());
        assert!(validate_arguments(NATIVE_METADATA_TOOL, &invalid).is_err());
    }
    let maximum = json!({"artifact_id":"\0".repeat(512),"fields":gptgrep_core::NATIVE_METADATA_SCALAR_FIELDS,
        "collection":"image_geometry","item_fields":gptgrep_core::NATIVE_IMAGE_FIELDS,"object_id":null,
        "page_size":1000,"max_output_bytes":16384,"cursor":"a".repeat(16384)});
    assert_eq!(measure(&maximum, MAX_NATIVE_ARGUMENTS)?, 19842);
    validate_arguments(NATIVE_METADATA_TOOL, &maximum)?;
    assert_eq!(argument_limit(NATIVE_METADATA_TOOL), 20480);
    for tool in [
        "gptgrep_catalog",
        "gptgrep_inspect_artifact",
        "gptgrep_search_text",
        "gptgrep_read_artifact",
        "gptgrep_render",
    ] {
        assert_eq!(argument_limit(tool), 4096);
        assert!(measure(&maximum, argument_limit(tool)).is_err());
    }
    let toolset = tools(None);
    let tool = toolset[0]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|tool| tool["name"] == NATIVE_METADATA_TOOL)
        .unwrap();
    assert_eq!(tool["inputSchema"]["additionalProperties"], false);
    assert_eq!(
        tool["inputSchema"]["properties"]["page_size"]["maximum"],
        1000
    );
    assert_eq!(
        tool["inputSchema"]["properties"]["page_size"]["default"],
        100
    );
    assert_eq!(
        tool["inputSchema"]["properties"]["max_output_bytes"]["maximum"],
        16384
    );
    Ok(())
}

#[tokio::test]
async fn native_metadata_actual_rpc_discovers_all_collections_and_pages_without_citations()
-> Result<()> {
    let (_temp, reader) = native_metadata_generation(false).await?;
    let (mut evidence, execution) = native_metadata_evidence(reader, false).await?;
    let (outcome, frames) = drive_native_metadata(&mut evidence, None, true).await?;
    assert!(evidence.finish(&outcome.answer)?.citations.is_empty());
    assert_eq!(evidence.calls, frames.len());
    assert!(frames.len() > 6, "continuations used actual dynamic calls");
    let discovery: Value = serde_json::from_str(
        frames[0]["result"]["contentItems"][0]["text"]
            .as_str()
            .unwrap(),
    )?;
    let id = &discovery["result"]["items"][0]["result"]["objects"][0]["metadata_artifact"]["id"];
    assert!(
        evidence
            .finish(
                &json!({"answer":"claim","insufficient_evidence":false,
        "citations":[{"issued_id":id,"offset":0,"length":1,"quote":"x"}]})
                .to_string()
            )
            .is_err()
    );
    assert_eq!(evidence.reader.native_metadata_retained_bytes(), 0);
    assert_eq!(evidence.delivery.as_ref().unwrap().retained_bytes(), 0);
    assert_eq!(execution.snapshot()?.submission_observations, 2);
    Ok(())
}

#[tokio::test]
async fn native_metadata_actual_rpc_long_cursor_preserves_integrity_and_other_tool_cap()
-> Result<()> {
    let (_temp, reader) = native_metadata_generation(false).await?;
    let id = reader.manifest()?.documents[0].pages[0]
        .metadata_artifact
        .id
        .clone();
    let (mut evidence, _execution) = native_metadata_evidence(reader.clone(), false).await?;
    let (outcome, frames) = drive_native_metadata(
        &mut evidence,
        Some((
            NATIVE_METADATA_TOOL,
            json!({"artifact_id":id,"collection":"native_characters","cursor":"a".repeat(16384)}),
        )),
        false,
    )
    .await?;
    assert!(evidence.finish(&outcome.answer)?.citations.is_empty());
    assert_eq!(frames[1]["result"]["success"], false);
    let body: Value = serde_json::from_str(
        frames[1]["result"]["contentItems"][0]["text"]
            .as_str()
            .unwrap(),
    )?;
    assert_eq!(
        body["result"]["items"][0]["error"]["code"],
        "invalid_cursor"
    );
    assert_eq!(evidence.reader.native_metadata_retained_bytes(), 0);
    let (mut evidence, _) = native_metadata_evidence(reader, false).await?;
    assert!(
        drive_native_metadata(
            &mut evidence,
            Some(("gptgrep_catalog", json!({"cursor":"a".repeat(4096)}))),
            false
        )
        .await
        .is_err()
    );
    assert_eq!(
        evidence.calls, 1,
        "oversized old-tool call was not admitted"
    );
    Ok(())
}

#[tokio::test]
async fn native_metadata_scope_and_artifact_kind_fail_before_native_content_read() -> Result<()> {
    let (_temp, reader) = native_metadata_generation(true).await?;
    let manifest = reader.manifest()?;
    let own = manifest
        .documents
        .iter()
        .find(|d| d.document.path == "source.pdf")
        .unwrap();
    let other = manifest
        .documents
        .iter()
        .find(|d| d.document.path == "other.pdf")
        .unwrap();
    let own_id = own.pages[0].metadata_artifact.id.clone();
    let other_id = other.pages[0].metadata_artifact.id.clone();
    let text_id = own.pages[0].native_text.id.clone();
    let document_id = own
        .artifacts
        .iter()
        .find(|a| a.artifact.kind == ArtifactKind::NativeMetadata && a.artifact.page.is_none())
        .unwrap()
        .artifact
        .id
        .clone();
    let other_path = manifest
        .root
        .join(".gptgrep/generations")
        .join(reader.pin().generation)
        .join(&other.pages[0].metadata_artifact.relative_path);
    let (mut evidence, _) = native_metadata_evidence(reader, true).await?;
    // A content read would see corruption; scope must reject first by identity.
    let saved = fs::read(&other_path)?;
    fs::write(&other_path, b"corrupt out-of-scope metadata")?;
    for id in [
        other_id.clone(),
        text_id,
        document_id,
        "missing-resource".into(),
    ] {
        let error = evidence
            .native_metadata_tool(&json!({"artifact_id":id,"collection":"native_characters"}))
            .err()
            .context("unavailable resource must fail")?;
        assert_eq!(error.to_string(), "host_rich_native_metadata_unavailable");
        assert_eq!(evidence.reader.native_metadata_retained_bytes(), 0);
    }
    fs::write(other_path, saved)?;
    let reply = evidence
        .native_metadata_tool(&json!({"artifact_id":own_id,"collection":"native_characters"}))?;
    reply.validate(&evidence.reader)?;
    drop(reply);
    assert_eq!(evidence.reader.native_metadata_retained_bytes(), 0);
    assert!(
        drive_native_metadata(
            &mut evidence,
            Some((
                NATIVE_METADATA_TOOL,
                json!({"artifact_id":other_id,"collection":"native_characters"})
            )),
            false
        )
        .await
        .is_err()
    );
    assert_eq!(
        evidence.calls, 2,
        "discovery then scoped rejection used actual RPC"
    );
    Ok(())
}

#[tokio::test]
async fn native_metadata_cursor_options_and_first_unfit_remain_core_errors() -> Result<()> {
    let (_temp, reader) = native_metadata_generation(false).await?;
    let id = reader.manifest()?.documents[0].pages[0]
        .metadata_artifact
        .id
        .clone();
    let (evidence, _) = native_metadata_evidence(reader, false).await?;
    let mut arguments = json!({"artifact_id":id,"collection":"native_characters",
        "fields":["counts"],"page_size":1});
    let first = evidence.native_metadata_tool(&arguments)?;
    arguments["cursor"] = json!(
        first._response.items[0]
            .coverage
            .cursor
            .as_ref()
            .context("continuation")?
    );
    drop(first);
    for patch in [
        json!({"fields":["geometry"]}),
        json!({"page_size":2}),
        json!({"max_output_bytes":16383}),
        json!({"collection":"native_words"}),
    ] {
        let mut changed = arguments.clone();
        changed
            .as_object_mut()
            .unwrap()
            .extend(patch.as_object().unwrap().clone());
        let reply = evidence.native_metadata_tool(&changed)?;
        assert_eq!(
            reply._response.items[0].error.as_ref().unwrap().code,
            gptgrep_core::rich_batch::RichBatchErrorCode::InvalidCursor
        );
        drop(reply);
        assert_eq!(evidence.reader.native_metadata_retained_bytes(), 0);
    }
    arguments.as_object_mut().unwrap().remove("cursor");
    arguments["max_output_bytes"] = json!(1);
    let reply = evidence.native_metadata_tool(&arguments)?;
    assert_eq!(
        reply._response.items[0].error.as_ref().unwrap().code,
        gptgrep_core::rich_batch::RichBatchErrorCode::OutputBudgetInsufficient
    );
    assert!(reply._response.items[0].result.is_none());
    assert_eq!(
        evidence.reader.native_metadata_retained_bytes(),
        NATIVE_TRANSPORT_RETAINED
    );
    drop(reply);
    assert_eq!(evidence.reader.native_metadata_retained_bytes(), 0);
    Ok(())
}

#[derive(Clone, Copy)]
enum MetadataWriteMode {
    Observe,
    Partial,
    Flush,
    ChangeSource,
    ChangePrepared,
    ChangeMetadata,
    HoldWrite,
    HoldFlush,
}
#[derive(Default)]
struct MetadataSendObservation {
    samples: std::sync::Mutex<Vec<u64>>,
    entered: tokio::sync::Notify,
    saw_flush: std::sync::atomic::AtomicBool,
}
struct MetadataWriter<W> {
    inner: W,
    reader: RichGenerationReader,
    mode: MetadataWriteMode,
    path: std::path::PathBuf,
    native_frame: bool,
    changed: bool,
    partial: bool,
    observation: Arc<MetadataSendObservation>,
}
impl<W> MetadataWriter<W> {
    fn observe(&self) {
        let held = self.reader.native_metadata_retained_bytes();
        assert!(held >= NATIVE_TRANSPORT_RETAINED + 64 * MAX_NATIVE_ITEM_OUTPUT as u64);
        self.observation.samples.lock().unwrap().push(held);
    }
}
impl<W: tokio::io::AsyncWrite + Unpin> tokio::io::AsyncWrite for MetadataWriter<W> {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        bytes: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        if bytes
            .windows(b"gptgrep.native_metadata.inspection.v1".len())
            .any(|part| part == b"gptgrep.native_metadata.inspection.v1")
        {
            self.native_frame = true;
        }
        if self.native_frame {
            self.observe();
            if matches!(self.mode, MetadataWriteMode::HoldWrite) {
                self.observation.entered.notify_one();
                return std::task::Poll::Pending;
            }
            if matches!(self.mode, MetadataWriteMode::Partial) {
                if self.partial {
                    return std::task::Poll::Ready(Err(std::io::Error::other(
                        "metadata partial write",
                    )));
                }
                let result = std::pin::Pin::new(&mut self.inner)
                    .poll_write(cx, &bytes[..bytes.len().min(17)]);
                if matches!(&result, std::task::Poll::Ready(Ok(n)) if *n > 0) {
                    self.partial = true;
                }
                return result;
            }
        }
        std::pin::Pin::new(&mut self.inner).poll_write(cx, bytes)
    }
    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        if self.native_frame {
            self.observe();
            self.observation
                .saw_flush
                .store(true, std::sync::atomic::Ordering::SeqCst);
            if matches!(self.mode, MetadataWriteMode::HoldFlush) {
                self.observation.entered.notify_one();
                return std::task::Poll::Pending;
            }
            if matches!(self.mode, MetadataWriteMode::Flush) {
                return std::task::Poll::Ready(Err(std::io::Error::other(
                    "metadata flush failure",
                )));
            }
        }
        let result = std::pin::Pin::new(&mut self.inner).poll_flush(cx);
        if self.native_frame
            && !self.changed
            && matches!(result, std::task::Poll::Ready(Ok(())))
            && matches!(
                self.mode,
                MetadataWriteMode::ChangeSource
                    | MetadataWriteMode::ChangePrepared
                    | MetadataWriteMode::ChangeMetadata
            )
        {
            fs::write(&self.path, b"changed during native metadata flush").unwrap();
            self.changed = true;
        }
        result
    }
    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

#[tokio::test]
async fn native_metadata_rpc_retains_both_leases_through_write_flush_and_freshness() -> Result<()> {
    for mode in [
        MetadataWriteMode::Observe,
        MetadataWriteMode::Partial,
        MetadataWriteMode::Flush,
        MetadataWriteMode::ChangeSource,
        MetadataWriteMode::ChangePrepared,
        MetadataWriteMode::ChangeMetadata,
    ] {
        let (_temp, reader) = native_metadata_generation(false).await?;
        let (mut evidence, _) = native_metadata_evidence(reader.clone(), false).await?;
        let manifest = reader.manifest()?;
        let document = &manifest.documents[0];
        let generation = manifest
            .root
            .join(".gptgrep/generations")
            .join(reader.pin().generation);
        let path = match mode {
            MetadataWriteMode::ChangePrepared => generation.join(&document.prepared_path),
            MetadataWriteMode::ChangeMetadata => {
                generation.join(&document.pages[0].metadata_artifact.relative_path)
            }
            _ => manifest.root.join(&document.document.path),
        };
        let held = reader.reserve_native_metadata_transport(4096)?;
        let observation = Arc::new(MetadataSendObservation::default());
        let (host, server) = tokio::io::duplex(262144);
        let (read, inner) = tokio::io::split(host);
        let writer = MetadataWriter {
            inner,
            reader: reader.clone(),
            mode,
            path,
            native_frame: false,
            changed: false,
            partial: false,
            observation: observation.clone(),
        };
        let peer = tokio::spawn(native_metadata_rpc_peer(server, None, false));
        let deadline = evidence.deadline;
        let config = native_metadata_config(false);
        let budget = crate::transport::TransportBudget::new(protocol::MAX_TOTAL_BYTES);
        let result = protocol::run_observed_budget(
            read,
            writer,
            Path::new("."),
            &config,
            protocol::Workflow::RichText {
                question: "source claim",
                evidence: &mut evidence,
            },
            None,
            protocol::RunOptions::default(),
            budget.clone(),
            deadline,
        )
        .await;
        if matches!(mode, MetadataWriteMode::Observe) {
            let outcome = result?;
            assert!(evidence.finish(&outcome.answer)?.citations.is_empty());
            assert_eq!(peer.await??.len(), 2);
            budget.check_host()?;
        } else {
            assert!(result.is_err());
            peer.abort();
            let _ = peer.await;
            assert!(budget.check_host().is_err());
        }
        let samples = observation.samples.lock().unwrap();
        assert!(!samples.is_empty());
        assert!(
            samples.iter().all(|bytes| *bytes
                == 4096 + NATIVE_TRANSPORT_RETAINED + 64 * MAX_NATIVE_ITEM_OUTPUT as u64)
        );
        if !matches!(mode, MetadataWriteMode::Partial) {
            assert!(
                observation
                    .saw_flush
                    .load(std::sync::atomic::Ordering::SeqCst)
            );
        } else {
            assert!(
                samples.len() >= 2,
                "partial write followed by a failing retry"
            );
        }
        assert_eq!(reader.native_metadata_retained_bytes(), 4096);
        let handle = evidence.delivery.as_ref().unwrap();
        assert_eq!(handle.unknown_entries(), 0);
        assert_eq!(handle.retained_bytes(), 0);
        drop(held);
        assert_eq!(reader.native_metadata_retained_bytes(), 0);
    }
    Ok(())
}

#[tokio::test]
async fn native_metadata_rpc_cancellation_drops_shared_reservations_after_pending_io() -> Result<()>
{
    for mode in [MetadataWriteMode::HoldWrite, MetadataWriteMode::HoldFlush] {
        let (_temp, reader) = native_metadata_generation(false).await?;
        let (mut evidence, _) = native_metadata_evidence(reader.clone(), false).await?;
        let held = reader.reserve_native_metadata_transport(4096)?;
        let ceiling = reader
            .manifest()?
            .resource_budget
            .owned_working_bytes
            .min(gptgrep_core::MAX_NATIVE_METADATA_WORKING_BYTES);
        let observation = Arc::new(MetadataSendObservation::default());
        let (host, server) = tokio::io::duplex(262144);
        let (read, inner) = tokio::io::split(host);
        let writer = MetadataWriter {
            inner,
            reader: reader.clone(),
            mode,
            path: std::path::PathBuf::new(),
            native_frame: false,
            changed: false,
            partial: false,
            observation: observation.clone(),
        };
        let peer = tokio::spawn(native_metadata_rpc_peer(server, None, false));
        let deadline = evidence.deadline;
        let config = native_metadata_config(false);
        let budget = crate::transport::TransportBudget::new(protocol::MAX_TOTAL_BYTES);
        let mut run = Box::pin(protocol::run_observed_budget(
            read,
            writer,
            Path::new("."),
            &config,
            protocol::Workflow::RichText {
                question: "source claim",
                evidence: &mut evidence,
            },
            None,
            protocol::RunOptions::default(),
            budget.clone(),
            deadline,
        ));
        tokio::select! {
            result = &mut run => panic!("send did not remain pending: {:?}", result.err()),
            _ = observation.entered.notified() => {}
        }
        let before = reader.native_metadata_retained_bytes();
        assert_eq!(
            before,
            4096 + NATIVE_TRANSPORT_RETAINED + 64 * MAX_NATIVE_ITEM_OUTPUT as u64
        );
        assert!(
            reader
                .reserve_native_metadata_transport(ceiling - before + 1)
                .is_err()
        );
        assert_eq!(reader.native_metadata_retained_bytes(), before);
        drop(run);
        assert!(budget.check_host().is_err());
        assert_eq!(reader.native_metadata_retained_bytes(), 4096);
        assert_eq!(evidence.delivery.as_ref().unwrap().unknown_entries(), 0);
        assert_eq!(evidence.delivery.as_ref().unwrap().retained_bytes(), 0);
        peer.abort();
        let _ = peer.await;
        drop(held);
        assert_eq!(reader.native_metadata_retained_bytes(), 0);
    }
    Ok(())
}

#[test]
fn native_metadata_bounded_serialization_covers_escaping_and_rejects_growth() -> Result<()> {
    let inner = json!({"text":"\"\\\u{0}中".repeat(128)});
    let length = measure(&inner, MAX_NATIVE_ENVELOPE)?;
    assert!(bounded_json_string(&inner, length - 1).is_err());
    let text = bounded_json_string(&inner, length)?;
    assert_eq!(serde_json::from_str::<Value>(&text)?, inner);
    let frame = json!({"id":"\0".repeat(256),"result":{"success":false,
        "contentItems":[{"type":"inputText","text":text}]}});
    assert!(measure(&frame, MAX_NATIVE_METADATA_FRAME - 1)? < 2 * length + 1621);
    assert_eq!(MAX_NATIVE_METADATA_FRAME, 67393);
    struct ChangesAfterMeasure(std::cell::Cell<bool>);
    impl Serialize for ChangesAfterMeasure {
        fn serialize<S: serde::Serializer>(
            &self,
            serializer: S,
        ) -> std::result::Result<S::Ok, S::Error> {
            serializer.serialize_str(if self.0.replace(true) {
                "does not fit"
            } else {
                "x"
            })
        }
    }
    assert!(bounded_json_string(&ChangesAfterMeasure(std::cell::Cell::new(false)), 64).is_err());
    Ok(())
}
#[tokio::test]
async fn captured_host_deadline_quarantines_slow_route_without_second_admission() -> Result<()> {
    let (_dir, prepared, execution, observed, server) =
        deadline_fixture(Duration::from_millis(1500)).await?;
    let error = prepared
        .execute(&|_| Ok(()))
        .await
        .err()
        .expect("expired host cannot succeed");
    server.abort();
    let _ = server.await;
    let snapshot = execution.snapshot()?;
    assert_eq!(snapshot.submission_observations, 1);
    assert_eq!(snapshot.logical_reservations, 1);
    assert_eq!(snapshot.send_start_observations, 1);
    assert_eq!(snapshot.quarantined_slots, 1);
    assert_eq!(snapshot.active_or_quarantined_slots, 1);
    assert_eq!(snapshot.terminal_submissions, 0);
    assert_eq!(observed.load(std::sync::atomic::Ordering::SeqCst), 1);
    let partial = error.initial_retrieval.as_ref().unwrap();
    assert_eq!(partial.logical_calls_attempted, 1);
    assert_eq!(partial.validated_responses, 0);
    assert_eq!(partial.attempts[0].response_status, "failed_or_unreported");
    assert!(!partial.attempts[0].usage.usage_observed);
    assert_eq!(partial.attempts[0].actual_provider_requests, None);
    assert!(!error.accounting_complete);
    assert!(error.model_attempts.is_empty());
    assert_eq!(error.unknown_text_deliveries, 0);
    assert_eq!(error.retained_text_registry_bytes, 0);
    assert!(execution.original_run_remaining()? > Duration::from_secs(50));
    Ok(())
}
#[tokio::test]
async fn captured_host_deadline_preserves_first_receipt_and_blocks_dependent_admission()
-> Result<()> {
    let (_dir, prepared, execution, observed, server) = deadline_fixture(Duration::ZERO).await?;
    let receipts = std::sync::Mutex::new(Vec::new());
    let error = prepared
        .execute(&|attempt| {
            receipts.lock().unwrap().push(attempt.full_usage().clone());
            std::thread::sleep(Duration::from_millis(1100));
            Ok(())
        })
        .await
        .err()
        .expect("expired host cannot succeed");
    server.abort();
    let _ = server.await;
    let snapshot = execution.snapshot()?;
    assert_eq!(snapshot.logical_reservations, 1);
    assert_eq!(snapshot.send_start_observations, 1);
    assert_eq!(snapshot.submission_observations, 1);
    assert_eq!(snapshot.terminal_submissions, 1);
    assert_eq!(snapshot.quarantined_slots, 0);
    assert_eq!(observed.load(std::sync::atomic::Ordering::SeqCst), 1);
    let partial = error.initial_retrieval.as_ref().unwrap();
    assert_eq!(partial.logical_calls_attempted, 1);
    assert_eq!(partial.validated_responses, 1);
    assert_eq!(partial.attempts.len(), 1);
    assert_eq!(partial.attempts[0].response_status, "validated");
    assert_eq!(partial.attempts[0].usage.cost, Some(json!(0.001)));
    assert_eq!(
        partial.attempts[0].full_usage(),
        &receipts.lock().unwrap()[0]
    );
    assert_eq!(
        partial.attempts[0].full_usage()["unknown_detail"]
            .as_str()
            .unwrap()
            .len(),
        131072
    );
    assert!(error.model_attempts.is_empty());
    assert_eq!(error.unknown_text_deliveries, 0);
    assert_eq!(error.retained_text_registry_bytes, 0);
    assert!(!error.accounting_complete);
    assert!(execution.original_run_remaining()? > Duration::from_secs(50));
    Ok(())
}
