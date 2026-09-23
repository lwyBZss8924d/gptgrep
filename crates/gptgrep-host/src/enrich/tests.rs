use super::*;
use std::{collections::BTreeMap, fs, sync::Mutex};

#[derive(Clone, Copy)]
enum Mode {
    Hint,
    Empty,
    Mixed,
    Foreign,
    Control,
    Failure,
    Pending,
    JevFailure,
    JevTransport,
    BuilderTimeout,
    AlternateHint,
    JevFlaky,
    JevUnauthorized,
    JevValidation,
    JevNoSupport,
    BuilderProtocolFailure,
    EscapedHints,
}

#[derive(Default)]
struct Calls {
    builder: usize,
    jev: usize,
    windows: Vec<String>,
    deadlines: Vec<tokio::time::Instant>,
    prepared_questions: usize,
}

struct Mock {
    mode: Mode,
    calls: Mutex<Calls>,
    client: JevClient,
    ledger: PathBuf,
    mutate_source: Option<PathBuf>,
}

impl Mock {
    fn new(mode: Mode, config: &EnrichConfig) -> Self {
        Self {
            mode,
            calls: Mutex::new(Calls::default()),
            client: JevClient::with_endpoint(
                "synthetic",
                config.host.jev_model.as_deref(),
                "http://127.0.0.1:9/decisions",
            )
            .unwrap(),
            ledger: config.ledger_path.clone(),
            mutate_source: None,
        }
    }
    fn last_reservation(&self, expected_kind: &str) -> Value {
        let bytes = fs::read_to_string(&self.ledger).unwrap();
        let last: Value = serde_json::from_str(bytes.lines().last().unwrap()).unwrap();
        assert!(matches!(
            last["payload"]["event"].as_str(),
            Some("call_reserved" | "support_retry_reserved")
        ));
        assert_eq!(last["payload"]["reservation"]["kind"], expected_kind);
        last["payload"]["reservation"].clone()
    }
}

impl Backend for Mock {
    fn prepare(&self, state: Value, questions: Value) -> Result<PreparedDecision> {
        self.calls.lock().unwrap().prepared_questions = questions.as_object().unwrap().len();
        self.client.prepare_decision(state, questions)
    }
    async fn complete(
        &self,
        state: Value,
        schema: Value,
        config: &HostConfig,
        deadline: tokio::time::Instant,
    ) -> Result<CompletionReport> {
        self.last_reservation("builder");
        let ordinal = {
            let mut calls = self.calls.lock().unwrap();
            calls.builder += 1;
            calls
                .windows
                .push(state["source_window"]["text"].as_str().unwrap().into());
            calls.deadlines.push(deadline);
            calls.builder
        };
        if matches!(self.mode, Mode::Pending) {
            std::future::pending::<()>().await;
        }
        if matches!(self.mode, Mode::Failure) {
            return Err(anyhow!("PRIVATE_PROVIDER_TEXT"));
        }
        if matches!(self.mode, Mode::BuilderTimeout) {
            return Err(anyhow!("host_model_attempt_timeout"));
        }
        if matches!(self.mode, Mode::BuilderProtocolFailure) {
            return Err(crate::HostProtocolError { kind: crate::HostProtocolErrorKind::TerminalError,
                codex_error_info: Some(crate::CodexErrorInfo::ServerOverloaded), will_retry: Some(false), http_status_code: Some(503),
                server_retry_notifications: 1, usage: Some(json!({"total":{"inputTokens":12,"outputTokens":5,"totalTokens":17},"PRIVATE":"not-retained"})),
                accounting_complete: false, tool_budget: None }.into());
        }
        if let Some(path) = &self.mutate_source {
            fs::write(path, "Changed source after planning.")?;
        }
        let anchor = state["anchor_id"].as_str().unwrap();
        let value = match self.mode {
            Mode::Empty => json!({"hints":[]}),
            Mode::Foreign => json!({"hints":[{"anchor_id":"foreign","hint":"A topic."}]}),
            Mode::Control => json!({"hints":[{"anchor_id":anchor,"hint":"First\nsecond"}]}),
            Mode::AlternateHint => {
                json!({"hints":[{"anchor_id":anchor,"hint":"An independently regenerated local navigation description."}]})
            }
            Mode::Mixed => {
                json!({"hints":[{"anchor_id":anchor,"hint":"Copper bead topic."},{"anchor_id":anchor,"hint":"Nearby context required."},{"anchor_id":anchor,"hint":"Unsupported silver claim."}]})
            }
            Mode::EscapedHints => json!({"hints":(0..MAX_HINTS_PER_WINDOW).map(|index|
                json!({"anchor_id":anchor,"hint":format!("{}{index}", "\"".repeat(MAX_HINT_BYTES - 1))})).collect::<Vec<_>>()}),
            _ => json!({"hints":[{"anchor_id":anchor,"hint":"A local navigation topic."}]}),
        };
        Ok(CompletionReport {
            schema_version: "gptgrep.completion.v1".into(),
            status: "completed".into(),
            value,
            thread_id: format!("synthetic-thread-{ordinal}"),
            turn_id: format!("synthetic-turn-{ordinal}"),
            requested_model: config.model.clone(),
            model: config.model.clone(),
            model_provider: "synthetic-provider".into(),
            requested_reasoning_effort: config.reasoning_effort.clone(),
            effective_reasoning_effort: Some(config.reasoning_effort.clone()),
            requested_service_tier: config.service_tier.clone(),
            effective_service_tier: Some(config.service_tier.clone()),
            server_retry_notifications: 0,
            auth_mode: "synthetic".into(),
            codex_home: config.codex_home.clone(),
            usage: Some(
                json!({"total":{"inputTokens":7,"outputTokens":3,"totalTokens":10},"private":"not-retained"}),
            ),
            elapsed_ms: 1,
            input_bytes: serde_json::to_vec(&state)?.len(),
            instructions_sha256: hash(BUILDER_INSTRUCTIONS.as_bytes()),
            state_sha256: hash(&serde_json::to_vec(&state)?),
            schema_sha256: hash(&serde_json::to_vec(&schema)?),
            stderr_bytes: Some(0),
            stderr_truncated: Some(false),
            warnings: vec![],
        })
    }
    async fn submit(&self, prepared: PreparedDecision) -> Result<DecisionResponse> {
        let reservation = self.last_reservation("jev");
        assert_eq!(reservation["request_sha256"], prepared.body_sha256());
        assert_eq!(reservation["request_bytes"], prepared.body_bytes());
        let (ordinal, count) = {
            let mut calls = self.calls.lock().unwrap();
            calls.jev += 1;
            (calls.jev, calls.prepared_questions)
        };
        if matches!(self.mode, Mode::JevFailure) {
            return Err(anyhow!("PRIVATE_JEV_TEXT"));
        }
        if matches!(self.mode, Mode::JevTransport) {
            return Err(anyhow!(
                "Jev transport failed; usage may be unknown and the request was not retried"
            ));
        }
        if matches!(self.mode, Mode::JevFlaky) && ordinal <= 2 {
            return Err(anyhow!(
                "Jev request timed out; usage may be unknown and the request was not retried"
            ));
        }
        if matches!(self.mode, Mode::JevUnauthorized) {
            return Err(anyhow!(
                "Jev provider returned HTTP 401; request was not retried"
            ));
        }
        if matches!(self.mode, Mode::JevValidation) {
            return Err(anyhow!(
                "Jev response is invalid JSON or contains duplicate keys"
            ));
        }
        let answers = (0..count)
            .map(|index| {
                (
                    format!("hint_{index}"),
                    DecisionAnswer::Choice {
                        choice: if matches!(self.mode, Mode::JevNoSupport) {
                            "unsupported".into()
                        } else if matches!(self.mode, Mode::Mixed) {
                            ["supported", "needs_context", "unsupported"][index].into()
                        } else {
                            "supported".into()
                        },
                        confidence: None,
                        probabilities: None,
                    },
                )
            })
            .collect::<BTreeMap<_, _>>();
        Ok(DecisionResponse {
            model: prepared.requested_model().into(),
            answers,
            usage: json!({"prompt_tokens":11,"completion_tokens":2,"total_tokens":13,"private":"not-retained"}),
            id: Some(format!("synthetic-jev-{ordinal}")),
            provider: Some("synthetic-provider".into()),
        })
    }
}

async fn fixture(source: &str) -> Result<(tempfile::TempDir, EnrichConfig)> {
    let directory = tempfile::tempdir()?;
    fs::write(directory.path().join("notes.txt"), source)?;
    gptgrep_core::index(directory.path(), 8).await?;
    let config = EnrichConfig {
        ledger_path: directory.path().join("private-builder/attempt.jsonl"),
        max_windows_per_run: MAX_WINDOWS,
        ..Default::default()
    };
    Ok((directory, config))
}

#[tokio::test]
async fn enrich_plan_covers_long_utf8_source_and_preflights_escaped_envelope_without_calls()
-> Result<()> {
    let source = "quoted \"\\ \u{1b} λ🙂\r\n".repeat(5000);
    let (directory, mut config) = fixture(&source).await?;
    config.window_bytes = 65_536;
    let plan = plan_enrichment(directory.path(), &config)?;
    assert!(!config.ledger_path.exists());
    assert!(plan.unit_count > 2);
    assert_eq!(plan.unit_count, plan.worst_case_builder_calls);
    assert_eq!(plan.unit_count, plan.worst_case_jev_calls);
    assert!(plan.units[0].anchor.byte_end < config.window_bytes);
    let mut cursor = 0;
    for unit in &plan.units {
        assert_eq!(unit.anchor.byte_start, cursor);
        cursor = unit.anchor.byte_end;
        assert!(unit.worst_case_jev_request_bytes <= gptgrep_jev::MAX_REQUEST_BYTES);
    }
    assert_eq!(cursor, plan.units.last().unwrap().document.text_bytes);
    config.expected_plan_sha256 = Some(plan.plan_sha256.clone());
    let backend = Mock::new(Mode::Hint, &config);
    let report = enrich_with(directory.path(), &config, &backend).await?;
    assert_eq!(report.status, "complete");
    assert_eq!(report.plan_sha256, plan.plan_sha256);
    assert_eq!(report.windows_completed, plan.unit_count);
    assert_eq!(backend.calls.lock().unwrap().builder, plan.unit_count);
    assert_eq!(backend.calls.lock().unwrap().jev, plan.unit_count);
    let mut canonical = String::new();
    let mut raw = gptgrep_core::open_navigation_document(directory.path(), "notes.txt")?;
    while let Some(window) = raw.next_window(65_536)? {
        canonical.push_str(&window.text);
    }
    assert_eq!(backend.calls.lock().unwrap().windows.concat(), canonical);
    let overlay = gptgrep_core::read_navigation_overlay(directory.path())?.unwrap();
    assert!(!overlay.coverage().partial_source_coverage);
    assert_eq!(overlay.coverage().covered_text_bytes, canonical.len());
    assert!(overlay.coverage().partial_hint_coverage); // chunk hints are not node hints
    assert_eq!(
        report.builder.known_total_tokens,
        Some(10 * plan.unit_count as u64)
    );
    assert_eq!(
        report.jev.known_total_tokens,
        Some(13 * plan.unit_count as u64)
    );
    Ok(())
}

#[tokio::test]
async fn enrich_resume_skips_complete_units_and_complete_resume_is_idempotent() -> Result<()> {
    let (directory, mut config) = fixture(&"Copper beads and blue shelves.\r\n".repeat(20)).await?;
    config.window_bytes = 83;
    config.max_windows_per_run = 1;
    let plan = plan_enrichment(directory.path(), &config)?;
    let backend = Mock::new(Mode::Hint, &config);
    let first = enrich_with(directory.path(), &config, &backend).await?;
    assert_eq!(first.status, "incomplete");
    assert!(first.resume_safe);
    assert_eq!(
        first.next_cursor.as_ref().unwrap().offset_bytes,
        plan.units[0].anchor.byte_end
    );
    assert!(gptgrep_core::read_navigation_overlay(directory.path())?.is_none());
    let prefix = fs::read(&config.ledger_path)?;
    config.resume = true;
    config.max_windows_per_run = MAX_WINDOWS;
    config.expected_plan_sha256 = Some(plan.plan_sha256);
    let final_report = enrich_with(directory.path(), &config, &backend).await?;
    assert_eq!(final_report.status, "complete");
    assert_eq!(final_report.windows_reused, 1);
    assert_eq!(
        backend.calls.lock().unwrap().builder,
        final_report.windows_completed
    );
    assert!(fs::read(&config.ledger_path)?.starts_with(&prefix));
    let complete_bytes = fs::read(&config.ledger_path)?;
    let again = enrich_with(directory.path(), &config, &backend).await?;
    assert_eq!(again.status, "complete");
    assert_eq!(again.windows_reused, final_report.windows_completed);
    assert_eq!(fs::read(&config.ledger_path)?, complete_bytes);
    Ok(())
}

#[tokio::test]
async fn enrich_maximum_escaped_hints_fit_the_pre_model_reserved_envelope() -> Result<()> {
    let (directory, mut config) = fixture(&"\u{1b}\"\\λ🙂\r\n".repeat(1800)).await?;
    config.window_bytes = 65_536;
    let plan = plan_enrichment(directory.path(), &config)?;
    let backend = Mock::new(Mode::EscapedHints, &config);
    let report = enrich_with(directory.path(), &config, &backend).await?;
    assert_eq!(report.status, "complete");
    assert_eq!(report.jev.completed_calls, plan.unit_count);
    assert_eq!(
        report.coverage.unwrap().hints,
        plan.unit_count * MAX_HINTS_PER_WINDOW
    );
    Ok(())
}

#[tokio::test]
async fn enrich_zero_hints_and_finite_support_filter_keep_full_raw_coverage() -> Result<()> {
    for mode in [Mode::Empty, Mode::Mixed] {
        let (directory, config) = fixture("Copper bead notes. Blue shelves remain nearby.").await?;
        let backend = Mock::new(mode, &config);
        let report = enrich_with(directory.path(), &config, &backend).await?;
        assert_eq!(report.status, "complete");
        let overlay = gptgrep_core::read_navigation_overlay(directory.path())?.unwrap();
        let hints = overlay
            .selected_document_hints(directory.path(), "notes.txt")?
            .unwrap();
        assert_eq!(hints.windows.len(), 1);
        assert_eq!(hints.hints.len(), usize::from(matches!(mode, Mode::Mixed)));
        assert_eq!(
            backend.calls.lock().unwrap().jev,
            usize::from(matches!(mode, Mode::Mixed))
        );
        assert!(!overlay.coverage().partial_source_coverage);
        assert!(!serde_json::to_string(&hints)?.contains("Unsupported silver"));
        for hint in &hints.hints {
            assert!(matches!(hint.target, NavigationHintTarget::Chunk { .. }));
        }
    }
    Ok(())
}

#[tokio::test]
async fn enrich_invalid_outputs_preserve_observed_usage_and_never_retry() -> Result<()> {
    for mode in [
        Mode::Foreign,
        Mode::Control,
        Mode::Failure,
        Mode::JevFailure,
    ] {
        let (directory, mut config) = fixture("Copper bead notes.").await?;
        let backend = Mock::new(mode, &config);
        let report = enrich_with(directory.path(), &config, &backend).await?;
        assert_eq!(report.status, "failed");
        assert!(!report.resume_safe);
        assert_eq!(report.builder.attempted_calls, 1);
        if matches!(mode, Mode::Failure) {
            assert_eq!(report.builder.known_total_tokens, None);
            assert_eq!(report.builder.unobserved_calls, 1);
        } else {
            assert_eq!(report.builder.known_total_tokens, Some(10));
        }
        if matches!(mode, Mode::JevFailure) {
            assert_eq!(report.jev.missing_total_tokens, 1);
        }
        assert!(gptgrep_core::read_navigation_overlay(directory.path())?.is_none());
        let bytes = fs::read(&config.ledger_path)?;
        assert!(!String::from_utf8_lossy(&bytes).contains("PRIVATE_"));
        assert!(!String::from_utf8_lossy(&bytes).contains("not-retained"));
        config.resume = true;
        assert!(
            enrich_with(directory.path(), &config, &backend)
                .await
                .is_err()
        );
        assert_eq!(fs::read(&config.ledger_path)?, bytes);
        assert_eq!(backend.calls.lock().unwrap().builder, 1);
    }
    Ok(())
}

#[tokio::test]
async fn enrich_pending_call_cancellation_blocks_resume_without_invented_usage() -> Result<()> {
    let (directory, mut config) = fixture("Copper bead notes.").await?;
    let backend = Mock::new(Mode::Pending, &config);
    assert!(
        tokio::time::timeout(
            Duration::from_millis(50),
            enrich_with(directory.path(), &config, &backend)
        )
        .await
        .is_err()
    );
    let bytes = fs::read_to_string(&config.ledger_path)?;
    assert!(bytes.contains("call_reserved"));
    assert!(!bytes.contains("call_finished"));
    config.resume = true;
    let error = enrich_with(directory.path(), &config, &backend)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("pending_call"));
    assert_eq!(backend.calls.lock().unwrap().builder, 1);
    assert_eq!(fs::read_to_string(&config.ledger_path)?, bytes);
    assert!(gptgrep_core::read_navigation_overlay(directory.path())?.is_none());
    Ok(())
}

#[tokio::test]
async fn enrich_cumulative_cap_resume_and_binding_changes_cannot_admit_calls() -> Result<()> {
    let (directory, mut config) = fixture(&"Copper bead notes.\n".repeat(10)).await?;
    config.window_bytes = 32;
    config.max_builder_calls = 1;
    config.max_jev_calls = 1;
    let backend = Mock::new(Mode::Hint, &config);
    let first = enrich_with(directory.path(), &config, &backend).await?;
    assert_eq!(first.reason.as_deref(), Some("cumulative_call_limit"));
    config.resume = true;
    let second = enrich_with(directory.path(), &config, &backend).await?;
    assert_eq!(second.reason, first.reason);
    assert_eq!(backend.calls.lock().unwrap().builder, 1);
    config.max_builder_calls = 2;
    assert!(
        enrich_with(directory.path(), &config, &backend)
            .await
            .is_err()
    );
    assert_eq!(backend.calls.lock().unwrap().builder, 1);
    Ok(())
}

#[tokio::test]
async fn enrich_plan_profile_and_source_changes_reject_before_calls() -> Result<()> {
    let (directory, mut config) = fixture("Copper bead notes.").await?;
    let plan = plan_enrichment(directory.path(), &config)?;
    config.expected_plan_sha256 = Some(plan.plan_sha256);
    config.host.model = "gpt-5.6-luna".into();
    let backend = Mock::new(Mode::Hint, &config);
    assert!(
        enrich_with(directory.path(), &config, &backend)
            .await
            .is_err()
    );
    assert_eq!(backend.calls.lock().unwrap().builder, 0);
    assert!(!config.ledger_path.exists());
    config.host.model = DEFAULT_ENRICH_MODEL.into();
    fs::write(
        directory.path().join("notes.txt"),
        "Changed canonical source.",
    )?;
    assert!(plan_enrichment(directory.path(), &config).is_err());
    assert!(!config.ledger_path.exists());
    Ok(())
}

#[tokio::test]
async fn enrich_source_change_after_call_never_publishes_overlay() -> Result<()> {
    let (directory, config) = fixture("Copper bead notes.").await?;
    let mut backend = Mock::new(Mode::Hint, &config);
    backend.mutate_source = Some(directory.path().join("notes.txt"));
    let report = enrich_with(directory.path(), &config, &backend).await?;
    assert_eq!(report.status, "failed");
    assert_eq!(report.builder.known_total_tokens, Some(10));
    assert!(report.publication.is_none());
    assert!(gptgrep_core::read_navigation_overlay(directory.path())?.is_none());
    Ok(())
}

#[tokio::test]
async fn enrich_publication_crash_boundary_recovers_only_exact_artifact_without_calls() -> Result<()>
{
    let (directory, mut config) = fixture("Copper bead notes.").await?;
    let backend = Mock::new(Mode::Hint, &config);
    assert_eq!(
        enrich_with(directory.path(), &config, &backend)
            .await?
            .status,
        "complete"
    );
    let content = fs::read_to_string(&config.ledger_path)?;
    let mut lines: Vec<_> = content.lines().collect();
    let final_event: Value = serde_json::from_str(lines.pop().unwrap())?;
    assert_eq!(final_event["payload"]["event"], "published");
    fs::write(&config.ledger_path, format!("{}\n", lines.join("\n")))?;
    config.resume = true;
    assert_eq!(
        enrich_with(directory.path(), &config, &backend)
            .await?
            .status,
        "complete"
    );
    assert_eq!(backend.calls.lock().unwrap().builder, 1);
    assert_eq!(backend.calls.lock().unwrap().jev, 1);
    Ok(())
}

#[tokio::test]
async fn enrich_orphan_partial_and_tampered_ledgers_fail_closed() -> Result<()> {
    for malformed in ["", "{", "{}\n"] {
        let (directory, mut config) = fixture("Copper bead notes.").await?;
        fs::create_dir_all(config.ledger_path.parent().unwrap())?;
        fs::write(&config.ledger_path, malformed)?;
        config.resume = true;
        let backend = Mock::new(Mode::Hint, &config);
        assert!(
            enrich_with(directory.path(), &config, &backend)
                .await
                .is_err()
        );
        assert_eq!(backend.calls.lock().unwrap().builder, 0);
        assert_eq!(fs::read_to_string(&config.ledger_path)?, malformed);
    }
    let (directory, mut config) = fixture(&"Copper bead notes.\n".repeat(4)).await?;
    config.window_bytes = 32;
    config.max_windows_per_run = 1;
    let backend = Mock::new(Mode::Hint, &config);
    enrich_with(directory.path(), &config, &backend).await?;
    let old = fs::read_to_string(&config.ledger_path)?;
    fs::write(
        &config.ledger_path,
        old.replace("A local navigation topic.", "An altered navigation topic."),
    )?;
    config.resume = true;
    assert!(
        enrich_with(directory.path(), &config, &backend)
            .await
            .is_err()
    );
    assert_eq!(backend.calls.lock().unwrap().builder, 1);
    Ok(())
}

#[test]
fn enrich_closed_schema_controls_utf8_bounds_and_unknown_targets_reject_without_normalization()
-> Result<()> {
    let schema = hint_schema("anchor", 4);
    let validator = jsonschema::validator_for(&schema)?;
    for control in ['\0', '\n', '\r', '\t', '\u{1b}', '\u{7f}', '\u{85}'] {
        let output = json!({"hints":[{"anchor_id":"anchor","hint":format!("left{control}right")}]});
        assert!(!validator.is_valid(&output));
        assert!(validate_drafts(&output, "anchor", 4).is_err());
    }
    for output in [
        json!({"hints":[{"anchor_id":"anchor","hint":"topic","node_id":"invented"}]}),
        json!({"hints":[{"anchor_id":"foreign","hint":"topic"}]}),
        json!({"hints":[{"anchor_id":"anchor","hint":"🙂".repeat(257)}]}),
        json!({"hints":[{"anchor_id":"anchor","hint":"topic"},{"anchor_id":"anchor","hint":"topic"}]}),
    ] {
        assert!(validate_drafts(&output, "anchor", 4).is_err());
    }
    let unchanged = "  λ topic  ";
    assert_eq!(
        validate_drafts(
            &json!({"hints":[{"anchor_id":"anchor","hint":unchanged}]}),
            "anchor",
            4
        )?[0]
            .hint,
        unchanged
    );
    Ok(())
}

#[tokio::test]
async fn enrich_planning_backend_has_no_transport_or_model_path() -> Result<()> {
    let backend = PlanningBackend {
        client: JevClient::with_endpoint("synthetic", None, "http://127.0.0.1:9/unused")?,
    };
    let prepared = backend.prepare(json!({}), json!({"hint_0":support_question("hint_0")}))?;
    assert_eq!(
        backend.submit(prepared).await.unwrap_err().to_string(),
        "enrich_plan_cannot_invoke_transport"
    );
    assert_eq!(
        backend
            .complete(
                json!({}),
                json!({}),
                &HostConfig::default(),
                tokio::time::Instant::now()
            )
            .await
            .unwrap_err()
            .to_string(),
        "enrich_plan_cannot_invoke_model"
    );
    Ok(())
}

#[tokio::test]
async fn enrich_successful_call_without_window_commit_is_not_automatically_replayed() -> Result<()>
{
    let (directory, mut config) = fixture("Copper bead notes.").await?;
    let backend = Mock::new(Mode::Hint, &config);
    enrich_with(directory.path(), &config, &backend).await?;
    let content = fs::read_to_string(&config.ledger_path)?;
    let first_call: Vec<_> = content.lines().take(3).collect();
    let terminal: Value = serde_json::from_str(first_call.last().unwrap())?;
    assert_eq!(terminal["payload"]["event"], "call_finished");
    fs::write(&config.ledger_path, format!("{}\n", first_call.join("\n")))?;
    config.resume = true;
    let error = enrich_with(directory.path(), &config, &backend)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("uncommitted_window"));
    assert_eq!(backend.calls.lock().unwrap().builder, 1);
    Ok(())
}

#[tokio::test]
async fn enrich_all_documents_share_one_invocation_deadline_and_source_bound_plan() -> Result<()> {
    let (directory, mut config) = fixture("Copper bead notes.\n").await?;
    fs::write(
        directory.path().join("other.txt"),
        "Blue shelves.\r\nNearby topics.",
    )?;
    gptgrep_core::index(directory.path(), 8).await?;
    config.window_bytes = 8;
    config.timeout_secs = 1;
    let plan = plan_enrichment(directory.path(), &config)?;
    assert_eq!(plan.documents.len(), 2);
    assert_eq!(plan.binding.source.documents_total, 2);
    let backend = Mock::new(Mode::Empty, &config);
    let report = enrich_with(directory.path(), &config, &backend).await?;
    assert_eq!(report.status, "complete");
    assert_eq!(report.documents_completed, 2);
    assert_eq!(report.windows_completed, plan.unit_count);
    let calls = backend.calls.lock().unwrap();
    assert!(calls.deadlines.len() > 2);
    assert!(
        calls
            .deadlines
            .iter()
            .all(|deadline| *deadline == calls.deadlines[0])
    );
    assert_eq!(calls.jev, 0);
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn enrich_symlink_and_concurrent_ledger_owners_fail_before_calls() -> Result<()> {
    use std::os::unix::fs::symlink;
    let (directory, mut config) = fixture("Copper bead notes.").await?;
    let plan = plan_enrichment(directory.path(), &config)?;
    let owner = Ledger::open(
        &config.ledger_path,
        plan.binding.clone(),
        &plan.plan_sha256,
        false,
    )?;
    config.resume = true;
    let backend = Mock::new(Mode::Hint, &config);
    assert!(
        enrich_with(directory.path(), &config, &backend)
            .await
            .is_err()
    );
    assert_eq!(backend.calls.lock().unwrap().builder, 0);
    drop(owner);
    let real = config.ledger_path.clone();
    config.ledger_path = directory.path().join("symlink.jsonl");
    symlink(real, &config.ledger_path)?;
    assert!(
        enrich_with(directory.path(), &config, &backend)
            .await
            .is_err()
    );
    Ok(())
}

// Convert only an invented temporary fixture to the historical receipt schema.
// Real frozen ledgers are never rewritten by recovery code.
fn legacy_receipt_fixture(path: &Path) -> Result<()> {
    let mut previous: Option<String> = None;
    let mut output = String::new();
    for line in fs::read_to_string(path)?.lines() {
        let mut line: Value = serde_json::from_str(line)?;
        if let Some(receipt) = line
            .get_mut("payload")
            .and_then(|payload| payload.get_mut("receipt"))
            .and_then(Value::as_object_mut)
        {
            receipt.remove("failure");
            receipt.remove("partial_usage");
        }
        line["previous_sha256"] = json!(previous);
        let digest = hash(&serde_json::to_vec(
            &json!({"sequence":line["sequence"],"previous_sha256":line["previous_sha256"],"payload":line["payload"]}),
        )?);
        line["sha256"] = json!(digest);
        previous = Some(digest);
        output.push_str(&serde_json::to_string(&line)?);
        output.push('\n');
    }
    fs::write(path, output)?;
    Ok(())
}

#[tokio::test]
async fn enrich_explicit_legacy_rebuild_preserves_windows_prefix_and_all_attempt_costs()
-> Result<()> {
    for failure in [Mode::Failure, Mode::JevFailure] {
        let (directory, mut config) =
            fixture(&"Invented copper and blue shelf notes.\n".repeat(10)).await?;
        config.window_bytes = 83;
        let count = plan_enrichment(directory.path(), &config)?.unit_count;
        config.max_builder_calls = count;
        config.max_jev_calls = count;
        config.expected_plan_sha256 = Some(plan_enrichment(directory.path(), &config)?.plan_sha256);
        config.max_windows_per_run = 1;
        let mut backend = Mock::new(Mode::Hint, &config);
        assert_eq!(
            enrich_with(directory.path(), &config, &backend)
                .await?
                .status,
            "incomplete"
        );
        config.resume = true;
        config.max_windows_per_run = MAX_WINDOWS;
        backend.mode = failure;
        let failed = enrich_with(directory.path(), &config, &backend).await?;
        assert_eq!(failed.status, "failed");
        assert_eq!(failed.windows_completed, 1);
        let failed_call = failed.last_failed_call.as_ref().unwrap()["call_id"]
            .as_u64()
            .unwrap() as usize;
        legacy_receipt_fixture(&config.ledger_path)?;
        let origin = fs::read(&config.ledger_path)?;
        assert!(
            enrich_with(directory.path(), &config, &backend)
                .await
                .is_err()
        );
        assert_eq!(fs::read(&config.ledger_path)?, origin);
        config.rebuild_failed_window = Some(failed_call);
        backend.mode = Mode::AlternateHint;
        let complete = enrich_with(directory.path(), &config, &backend).await?;
        assert_eq!(complete.status, "complete");
        assert_eq!(complete.windows_completed, count);
        assert_eq!(complete.windows_reused, 1);
        assert!(fs::read(&config.ledger_path)?.starts_with(&origin));
        assert_eq!(
            complete.recovery["admissions"][0]["failed_call_id"],
            failed_call
        );
        assert_eq!(
            complete.recovery["admissions"][0]["failure_classification"],
            "legacy_unclassified"
        );
        assert_eq!(
            complete.recovery["admissions"][0]["mode"],
            "rebuild_failed_window_new_builder_sample"
        );
        assert_eq!(complete.recovery["original_max_builder_calls"], count);
        assert_eq!(complete.recovery["effective_max_builder_calls"], count + 1);
        assert_eq!(complete.builder.attempted_calls, count + 1);
        assert_eq!(
            complete.jev.attempted_calls,
            count + usize::from(matches!(failure, Mode::JevFailure))
        );
        if matches!(failure, Mode::Failure) {
            assert_eq!(complete.builder.missing_usage_calls, 1);
            assert_eq!(complete.builder.known_total_tokens, Some(count as u64 * 10));
        } else {
            assert_eq!(complete.jev.missing_usage_calls, 1);
            assert_eq!(
                complete.builder.known_total_tokens,
                Some((count + 1) as u64 * 10)
            );
        }
        assert_eq!(complete.jev.known_total_tokens, Some(count as u64 * 13));
        assert!(
            !complete.last_failed_call.unwrap()["classification_available"]
                .as_bool()
                .unwrap()
        );
        let before_repeat = fs::read(&config.ledger_path)?;
        assert!(
            enrich_with(directory.path(), &config, &backend)
                .await
                .is_err()
        );
        assert_eq!(fs::read(&config.ledger_path)?, before_repeat);
    }
    Ok(())
}

#[tokio::test]
async fn enrich_rebuild_caps_at_two_and_never_replays_validation_or_pending_calls() -> Result<()> {
    let (directory, mut config) = fixture("Invented small source.").await?;
    config.max_builder_calls = 1;
    config.max_jev_calls = 1;
    let backend = Mock::new(Mode::JevTransport, &config);
    let mut failed = enrich_with(directory.path(), &config, &backend).await?;
    config.resume = true;
    for ordinal in 1..=2 {
        config.rebuild_failed_window = Some(
            failed.last_failed_call.as_ref().unwrap()["call_id"]
                .as_u64()
                .unwrap() as usize,
        );
        failed = enrich_with(directory.path(), &config, &backend).await?;
        assert_eq!(failed.status, "failed");
        assert_eq!(
            failed.recovery["admissions"].as_array().unwrap().len(),
            ordinal
        );
    }
    config.rebuild_failed_window = Some(
        failed.last_failed_call.as_ref().unwrap()["call_id"]
            .as_u64()
            .unwrap() as usize,
    );
    let prior = fs::read(&config.ledger_path)?;
    assert!(
        enrich_with(directory.path(), &config, &backend)
            .await
            .unwrap_err()
            .to_string()
            .contains("rebuild_limit")
    );
    assert_eq!(fs::read(&config.ledger_path)?, prior);
    assert_eq!(backend.calls.lock().unwrap().builder, 3);
    assert_eq!(backend.calls.lock().unwrap().jev, 3);
    for mode in [Mode::Foreign, Mode::Pending] {
        let (root, mut config) = fixture("Invented small source.").await?;
        let backend = Mock::new(mode, &config);
        let _ = tokio::time::timeout(
            Duration::from_millis(50),
            enrich_with(root.path(), &config, &backend),
        )
        .await;
        let prior = fs::read(&config.ledger_path)?;
        config.resume = true;
        config.rebuild_failed_window = Some(0);
        assert!(enrich_with(root.path(), &config, &backend).await.is_err());
        assert_eq!(fs::read(&config.ledger_path)?, prior);
        assert_eq!(backend.calls.lock().unwrap().builder, 1);
    }
    Ok(())
}

#[test]
fn enrich_failure_taxonomy_uses_safe_owner_metadata_without_provider_text() {
    for (message, expected, status) in [
        (
            "Jev request timed out; usage may be unknown and the request was not retried",
            FailureCategory::Timeout,
            None,
        ),
        (
            "Jev transport failed; usage may be unknown and the request was not retried",
            FailureCategory::Transport,
            None,
        ),
        (
            "Jev provider returned HTTP 503; request was not retried",
            FailureCategory::Http,
            Some(503),
        ),
        (
            "Jev provider returned HTTP 401; request was not retried",
            FailureCategory::Http,
            Some(401),
        ),
        (
            "Jev response is invalid JSON or contains duplicate keys",
            FailureCategory::Validation,
            None,
        ),
        (
            "PRIVATE arbitrary provider message",
            FailureCategory::Unknown,
            None,
        ),
    ] {
        let failure = failure_info(&anyhow!(message), CallKind::Jev);
        assert_eq!(failure.category, expected);
        assert_eq!(failure.http_status_code, status);
        assert!(!serde_json::to_string(&failure).unwrap().contains("PRIVATE"));
    }
    assert_eq!(
        failure_info(
            &crate::CompletionError::InvalidOutput.into(),
            CallKind::Builder
        )
        .category,
        FailureCategory::Validation
    );
    assert_eq!(
        failure_info(&anyhow!("host_model_attempt_timeout"), CallKind::Builder).category,
        FailureCategory::Timeout
    );
    for kind in [
        crate::HostProtocolErrorKind::IdentityMismatch,
        crate::HostProtocolErrorKind::MalformedError,
    ] {
        let error = crate::HostProtocolError {
            kind,
            codex_error_info: Some(crate::CodexErrorInfo::InternalServerError),
            will_retry: Some(false),
            http_status_code: Some(503),
            server_retry_notifications: 2,
            usage: None,
            accounting_complete: false,
            tool_budget: None,
        };
        let failure = failure_info(&error.into(), CallKind::Builder);
        assert_eq!(failure.category, FailureCategory::Protocol);
        assert_eq!(failure.protocol_error_kind, Some(kind));
        assert_eq!(failure.server_retry_notifications, Some(2));
    }
}

#[tokio::test]
async fn enrich_typed_builder_timeout_can_be_explicitly_rebuilt() -> Result<()> {
    let (root, mut config) = fixture("Invented small source.").await?;
    config.max_builder_calls = 1;
    config.max_jev_calls = 1;
    let mut backend = Mock::new(Mode::BuilderTimeout, &config);
    let failed = enrich_with(root.path(), &config, &backend).await?;
    assert_eq!(
        failed.last_failed_call.as_ref().unwrap()["failure"]["category"],
        "timeout"
    );
    let prefix = fs::read(&config.ledger_path)?;
    config.resume = true;
    config.rebuild_failed_window = Some(0);
    backend.mode = Mode::AlternateHint;
    let complete = enrich_with(root.path(), &config, &backend).await?;
    assert_eq!(complete.status, "complete");
    assert_eq!(complete.builder.attempted_calls, 2);
    assert_eq!(complete.builder.missing_usage_calls, 1);
    assert_eq!(
        complete.recovery["admissions"][0]["failure_classification"],
        "timeout"
    );
    assert!(fs::read(&config.ledger_path)?.starts_with(&prefix));
    Ok(())
}

#[tokio::test]
async fn enrich_support_retries_keep_identical_requests_and_one_builder_sample_across_resume()
-> Result<()> {
    let (root, mut config) =
        fixture(&"Invented local orchard and shelf notes.\n".repeat(9)).await?;
    config.window_bytes = 89;
    config.support_retries = 2;
    config.max_windows_per_run = 1;
    let count = plan_enrichment(root.path(), &config)?.unit_count;
    config.max_builder_calls = count;
    config.max_jev_calls = count;
    let plan = plan_enrichment(root.path(), &config)?;
    assert_eq!(plan.worst_case_jev_calls, count + 2);
    config.expected_plan_sha256 = Some(plan.plan_sha256);
    let backend = Mock::new(Mode::JevFlaky, &config);
    let first = enrich_with(root.path(), &config, &backend).await?;
    assert_eq!(first.status, "incomplete");
    assert_eq!(first.windows_completed, 1);
    assert_eq!(first.builder.attempted_calls, 1);
    assert_eq!(first.jev.attempted_calls, 3);
    let admissions = first.recovery["support_retry_admissions"]
        .as_array()
        .unwrap();
    assert_eq!(admissions.len(), 2);
    assert_eq!(admissions[0]["backoff_ms"], 250);
    assert_eq!(admissions[1]["backoff_ms"], 500);
    let rows: Vec<Value> = fs::read_to_string(&config.ledger_path)?
        .lines()
        .map(serde_json::from_str)
        .collect::<std::result::Result<_, _>>()?;
    let reservations: Vec<_> = rows
        .iter()
        .filter_map(|row| row["payload"].get("reservation"))
        .filter(|row| row["kind"] == "jev")
        .collect();
    assert_eq!(reservations.len(), 3);
    assert!(reservations.iter().all(|row| row["request_sha256"]
        == reservations[0]["request_sha256"]
        && row["request_bytes"] == reservations[0]["request_bytes"]));
    let prefix = fs::read(&config.ledger_path)?;
    config.resume = true;
    config.max_windows_per_run = MAX_WINDOWS;
    let complete = enrich_with(root.path(), &config, &backend).await?;
    assert_eq!(complete.status, "complete");
    assert_eq!(complete.windows_reused, 1);
    assert_eq!(complete.builder.attempted_calls, count);
    assert_eq!(complete.jev.attempted_calls, count + 2);
    assert_eq!(complete.jev.failed_calls, 2);
    assert_eq!(complete.jev.missing_usage_calls, 2);
    assert_eq!(complete.jev.known_total_tokens, Some(count as u64 * 13));
    assert_eq!(
        complete.recovery["support_retry_admissions"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert!(fs::read(&config.ledger_path)?.starts_with(&prefix));
    Ok(())
}

#[tokio::test]
async fn enrich_support_retry_exhaustion_and_exclusions_are_bounded_and_explicit() -> Result<()> {
    for mode in [
        Mode::JevTransport,
        Mode::JevUnauthorized,
        Mode::JevValidation,
        Mode::JevFailure,
        Mode::JevNoSupport,
    ] {
        let (root, mut config) = fixture("Invented local notes.").await?;
        config.support_retries = 2;
        config.max_jev_calls = 1;
        config.max_builder_calls = 1;
        let backend = Mock::new(mode, &config);
        let result = enrich_with(root.path(), &config, &backend).await?;
        assert_eq!(result.builder.attempted_calls, 1);
        let expected = if matches!(mode, Mode::JevTransport) {
            3
        } else {
            1
        };
        assert_eq!(result.jev.attempted_calls, expected);
        assert_eq!(
            result.recovery["support_retry_admissions"]
                .as_array()
                .unwrap()
                .len(),
            expected - 1
        );
        assert_eq!(
            result.status,
            if matches!(mode, Mode::JevNoSupport) {
                "complete"
            } else {
                "failed"
            }
        );
        if matches!(mode, Mode::JevNoSupport) {
            assert_eq!(result.coverage.unwrap().hints, 0);
        }
    }
    Ok(())
}

#[tokio::test]
async fn enrich_window_rebuild_after_retry_exhaustion_preserves_the_global_retry_budget()
-> Result<()> {
    let (root, mut config) = fixture("Invented local notes.").await?;
    config.support_retries = 2;
    config.max_builder_calls = 1;
    config.max_jev_calls = 1;
    let mut backend = Mock::new(Mode::JevTransport, &config);
    let failed = enrich_with(root.path(), &config, &backend).await?;
    assert_eq!(failed.status, "failed");
    assert_eq!(failed.jev.attempted_calls, 3);
    let prefix = fs::read(&config.ledger_path)?;
    config.resume = true;
    config.rebuild_failed_window = Some(3);
    backend.mode = Mode::AlternateHint;
    let complete = enrich_with(root.path(), &config, &backend).await?;
    assert_eq!(complete.status, "complete");
    assert_eq!(complete.builder.attempted_calls, 2);
    assert_eq!(complete.jev.attempted_calls, 4);
    assert_eq!(
        complete.recovery["support_retry_admissions"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        complete.recovery["admissions"][0]["effective_max_jev_calls"],
        4
    );
    assert_eq!(complete.recovery["effective_max_jev_calls"], 4);
    assert!(fs::read(&config.ledger_path)?.starts_with(&prefix));
    config.rebuild_failed_window = None;
    assert_eq!(
        enrich_with(root.path(), &config, &backend).await?.status,
        "complete"
    );
    assert_eq!(backend.calls.lock().unwrap().jev, 4);
    Ok(())
}

#[tokio::test]
async fn enrich_retry_backoff_cannot_extend_deadline_and_policy_cannot_change_on_resume()
-> Result<()> {
    let (root, mut config) = fixture("Invented local notes.").await?;
    config.support_retries = 2;
    let plan = plan_enrichment(root.path(), &config)?;
    let mut ledger = Ledger::open(&config.ledger_path, plan.binding, &plan.plan_sha256, false)?;
    let backend = Mock::new(Mode::JevTransport, &config);
    let cursor = gptgrep_core::open_navigation_document(root.path(), "notes.txt")?;
    let (window, bytes, _) = admitted_window(&cursor, 0, &config, &backend)?;
    {
        // The fake completes synchronously. A budget below the 250ms minimum
        // backoff must therefore return in one poll, never await a retry timer.
        // Preparation/fsync may already exhaust that budget under parallel load;
        // either zero or one initial send is valid, but no retry may be reserved.
        let future = process_window(
            &window,
            bytes,
            &config,
            &backend,
            &mut ledger,
            tokio::time::Instant::now() + Duration::from_millis(30),
        );
        tokio::pin!(future);
        let mut context = std::task::Context::from_waker(std::task::Waker::noop());
        assert!(matches!(
            std::future::Future::poll(future.as_mut(), &mut context),
            std::task::Poll::Ready(Err(_))
        ));
    }
    assert!(backend.calls.lock().unwrap().jev <= 1);
    assert!(
        ledger.recovery_summary()["support_retry_admissions"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    drop(ledger);
    config.resume = true;
    config.support_retries = 1;
    let prefix = fs::read(&config.ledger_path)?;
    assert!(enrich_with(root.path(), &config, &backend).await.is_err());
    assert_eq!(fs::read(&config.ledger_path)?, prefix);
    Ok(())
}

#[tokio::test]
async fn enrich_typed_failure_preserves_partial_native_usage_without_invented_identity()
-> Result<()> {
    let (root, mut config) = fixture("Invented local notes.").await?;
    config.max_builder_calls = 1;
    config.max_jev_calls = 1;
    let mut backend = Mock::new(Mode::BuilderProtocolFailure, &config);
    let failed = enrich_with(root.path(), &config, &backend).await?;
    assert_eq!(failed.builder.partial_usage_calls, 1);
    assert_eq!(failed.builder.known_total_tokens, Some(17));
    assert_eq!(failed.builder.unobserved_calls, 1);
    assert!(failed.last_failed_call.as_ref().unwrap()["observed"].is_null());
    assert_eq!(
        failed.last_failed_call.as_ref().unwrap()["failure"]["http_status_code"],
        503
    );
    assert!(!fs::read_to_string(&config.ledger_path)?.contains("PRIVATE"));
    config.resume = true;
    config.rebuild_failed_window = Some(0);
    backend.mode = Mode::Hint;
    let completed = enrich_with(root.path(), &config, &backend).await?;
    assert_eq!(completed.status, "complete");
    assert_eq!(completed.builder.known_total_tokens, Some(27));
    assert_eq!(completed.builder.partial_usage_calls, 1);
    assert_eq!(completed.builder.attempted_calls, 2);
    Ok(())
}

#[tokio::test]
async fn enrich_fresh_build_after_reindex_replaces_only_the_inactive_pointer() -> Result<()> {
    let (directory, mut config) = fixture("Original copper bead notes.\n").await?;
    let ledgers = tempfile::tempdir()?;
    config.ledger_path = ledgers.path().join("original.jsonl");
    let first = enrich_with(directory.path(), &config, &Mock::new(Mode::Hint, &config)).await?;
    assert_eq!(first.status, "complete");
    let prior_publication = first.publication.unwrap();
    let pointer = directory.path().join(".gptgrep/NAVIGATION.json");
    let old_pointer_bytes = fs::read(&pointer)?;
    let old_artifact = directory
        .path()
        .join(".gptgrep/navigation-overlays")
        .join(format!("{}.json", prior_publication.artifact_sha256));
    let old_artifact_bytes = fs::read(&old_artifact)?;

    fs::write(
        directory.path().join("notes.txt"),
        "Revised blue shelf notes.\r\n",
    )?;
    gptgrep_core::index(directory.path(), 8).await?;
    let current = fs::read(directory.path().join(".gptgrep/CURRENT.json"))?;
    assert!(gptgrep_core::read_navigation_overlay(directory.path()).is_err());

    // A failed new build must leave the stale pointer untouched.
    config.ledger_path = ledgers.path().join("failed-new.jsonl");
    let failure = enrich_with(
        directory.path(),
        &config,
        &Mock::new(Mode::Foreign, &config),
    )
    .await?;
    assert_eq!(failure.status, "failed");
    assert_eq!(fs::read(&pointer)?, old_pointer_bytes);

    config.ledger_path = ledgers.path().join("complete-new.jsonl");
    let second_backend = Mock::new(Mode::Hint, &config);
    let second = enrich_with(directory.path(), &config, &second_backend).await?;
    assert_eq!(second.status, "complete");
    let publication = second.publication.unwrap();
    assert_ne!(publication.generation, prior_publication.generation);
    assert_ne!(
        publication.manifest_sha256,
        prior_publication.manifest_sha256
    );
    assert_eq!(second_backend.calls.lock().unwrap().builder, 1);
    assert_eq!(second_backend.calls.lock().unwrap().jev, 1);
    let active = gptgrep_core::read_navigation_overlay(directory.path())?.unwrap();
    assert_eq!(active.publication(), &publication);
    assert!(
        active
            .selected_document_hints(directory.path(), "notes.txt")?
            .is_some()
    );
    assert_eq!(fs::read(&old_artifact)?, old_artifact_bytes);
    assert_eq!(
        fs::read(directory.path().join(".gptgrep/CURRENT.json"))?,
        current
    );
    Ok(())
}

#[tokio::test]
async fn enrich_current_artifact_corruption_is_fatal_and_never_replaced() -> Result<()> {
    let (directory, mut config) = fixture("Copper bead notes.").await?;
    let ledgers = tempfile::tempdir()?;
    config.ledger_path = ledgers.path().join("original.jsonl");
    let first = enrich_with(directory.path(), &config, &Mock::new(Mode::Hint, &config)).await?;
    let publication = first.publication.unwrap();
    let pointer = directory.path().join(".gptgrep/NAVIGATION.json");
    let pointer_bytes = fs::read(&pointer)?;
    let artifact = directory
        .path()
        .join(".gptgrep/navigation-overlays")
        .join(format!("{}.json", publication.artifact_sha256));
    fs::write(&artifact, "corrupted artifact")?;
    config.ledger_path = ledgers.path().join("new.jsonl");
    let result = enrich_with(directory.path(), &config, &Mock::new(Mode::Hint, &config)).await?;
    assert_eq!(result.status, "failed");
    assert_eq!(
        result.reason.as_deref(),
        Some("enrich_active_overlay_invalid")
    );
    assert_eq!(fs::read(&pointer)?, pointer_bytes);
    assert_eq!(fs::read_to_string(&artifact)?, "corrupted artifact");
    Ok(())
}

#[tokio::test]
async fn enrich_confirmed_resume_requires_the_exact_current_artifact() -> Result<()> {
    let (directory, mut config) = fixture("Copper bead notes.").await?;
    let backend = Mock::new(Mode::Hint, &config);
    let first = enrich_with(directory.path(), &config, &backend).await?;
    let active = gptgrep_core::read_navigation_overlay(directory.path())?.unwrap();
    let mut document = active
        .selected_document_hints(directory.path(), "notes.txt")?
        .unwrap();
    document.hints[0].hint = "Another source-local navigation description.".into();
    let other =
        NavigationOverlay::new(active.binding(), active.producer().clone(), vec![document])?;
    let different = gptgrep_core::publish_navigation_overlay(directory.path(), &other)?;
    assert_ne!(
        Some(different.artifact_sha256),
        first.publication.map(|value| value.artifact_sha256)
    );
    let pointer = directory.path().join(".gptgrep/NAVIGATION.json");
    let bytes = fs::read(&pointer)?;
    config.resume = true;
    let resumed = enrich_with(directory.path(), &config, &backend).await?;
    assert_eq!(resumed.status, "failed");
    assert_eq!(
        resumed.reason.as_deref(),
        Some("enrich_active_overlay_changed")
    );
    assert_eq!(fs::read(&pointer)?, bytes);
    assert_eq!(backend.calls.lock().unwrap().builder, 1);
    assert_eq!(backend.calls.lock().unwrap().jev, 1);
    Ok(())
}

#[tokio::test]
async fn enrich_pointer_classification_rejects_malformed_and_hybrid_bindings() -> Result<()> {
    let (directory, config) = fixture("Copper bead notes.").await?;
    let first = enrich_with(directory.path(), &config, &Mock::new(Mode::Hint, &config)).await?;
    let publication = serde_json::to_value(first.publication.unwrap())?;
    let source = gptgrep_core::navigation_overlay_binding(directory.path())?;
    let pointer = directory.path().join(".gptgrep/NAVIGATION.json");
    for (field, value) in [
        ("schema_version", json!("unsupported")),
        ("manifest_sha256", json!("a".repeat(64))),
        ("artifact_sha256", json!("not-a-digest")),
        ("generation", json!("../outside")),
        ("generation", json!("another-generation")),
        ("unknown", json!(true)),
    ] {
        let mut changed = publication.clone();
        changed[field] = value;
        fs::write(&pointer, serde_json::to_vec(&changed)?)?;
        assert!(active_overlay_for_publication(directory.path(), &source, true).is_err());
    }
    let valid = serde_json::to_string(&publication)?;
    let duplicate = format!("{{\"generation\":\"duplicate\",{}", &valid[1..]);
    let sequence = serde_json::to_string(&json!([
        publication["schema_version"],
        publication["generation"],
        publication["manifest_sha256"],
        publication["artifact_sha256"]
    ]))?;
    for malformed in ["{".to_owned(), " ".repeat(4097), duplicate, sequence] {
        fs::write(&pointer, malformed)?;
        assert!(active_overlay_for_publication(directory.path(), &source, true).is_err());
    }
    let mut prior = publication;
    prior["generation"] = json!("another-generation");
    prior["manifest_sha256"] = json!("a".repeat(64));
    fs::write(&pointer, serde_json::to_vec(&prior)?)?;
    assert!(active_overlay_for_publication(directory.path(), &source, false).is_err());
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn enrich_prior_binding_pointer_symlinks_are_never_treated_as_inactive() -> Result<()> {
    use std::os::unix::fs::symlink;
    let (directory, config) = fixture("Copper bead notes.").await?;
    let first = enrich_with(directory.path(), &config, &Mock::new(Mode::Hint, &config)).await?;
    let mut publication = first.publication.unwrap();
    publication.generation = "another-generation".into();
    publication.manifest_sha256 = "a".repeat(64);
    let source = gptgrep_core::navigation_overlay_binding(directory.path())?;
    let pointer = directory.path().join(".gptgrep/NAVIGATION.json");
    let target = directory.path().join("private-pointer.json");
    fs::write(&target, serde_json::to_vec(&publication)?)?;
    fs::remove_file(&pointer)?;
    symlink(&target, &pointer)?;
    assert!(active_overlay_for_publication(directory.path(), &source, true).is_err());
    assert!(fs::symlink_metadata(&pointer)?.file_type().is_symlink());
    Ok(())
}
