//! Trusted host bootstrap for source-bound text and rich-batch retrieval.
//! This binary does not ingest, launch a reader, initialize a budget, or own money admission.
use anyhow::{Result, ensure};
use clap::Parser;
use gptgrep_core::rich_batch::{
    PreparedRichBatch, RichBatchGeneration, RichBatchOperation, RichBatchRequest,
    RichBatchResponse, RichBatchStatus, prepare_rich_batch_with_reader,
};
use gptgrep_core::{
    EvidenceRef, GenerationPin, RichGenerationReader, RichTextAttempt, RichTextSearchOptions,
    RichTextSearchReport, prepare_rich_text_search,
};
use gptgrep_jev::{
    ExecutionContext, ExecutionSnapshot, JevClient, PreparedDecision, RunClockAnchor,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::{Component, Path, PathBuf},
    sync::Mutex,
    time::{Duration, Instant, UNIX_EPOCH},
};

const MODEL: &str = "openai/gpt-6-luna-decisions";
const CONFIG_CAP: usize = 65536;
const EVIDENCE_CAP: usize = 1048576;
const ACCOUNT_CAP: usize = 262144;

#[derive(Parser)]
#[command(about = "Trusted v2 Decisions bootstrap; default is nonexecuting")]
struct Cli {
    #[arg(long, requires = "config_sha256")]
    config: Option<PathBuf>,
    #[arg(long, requires = "config")]
    config_sha256: Option<String>,
    #[arg(long, requires = "config", conflicts_with = "preflight")]
    execute: bool,
    #[arg(long)]
    preflight: bool,
    /// Select the closed rich-batch host bootstrap contract.
    #[arg(long)]
    rich: bool,
    /// Prepare source-bound envelopes without credentials, reservations or sends.
    #[arg(long, requires_all = ["rich", "config"], conflicts_with = "execute")]
    prepare_rich: bool,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct InputFile {
    path: PathBuf,
    bytes: u64,
    sha256: String,
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Options {
    document: Option<String>,
    window_bytes: usize,
    max_output_bytes: usize,
    max_metadata_bytes: usize,
    min_score: f64,
}
impl Options {
    fn product(&self) -> RichTextSearchOptions {
        RichTextSearchOptions {
            document: self.document.clone(),
            window_bytes: self.window_bytes,
            max_output_bytes: self.max_output_bytes,
            max_metadata_bytes: self.max_metadata_bytes,
            min_score: self.min_score,
            require_shared_ledger: true,
        }
    }
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ContextInput {
    ledger: PathBuf,
    initialization_sha256: String,
    original_clock_anchor_sha256: String,
    run_id: String,
    plan_digest: String,
    absolute_deadline_unix_ms: u64,
    max_submissions: u32,
    #[serde(deserialize_with = "closed_snapshot")]
    expected_before: ExecutionSnapshot,
}
fn closed_snapshot<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<ExecutionSnapshot, D::Error> {
    let value = Value::deserialize(deserializer)?;
    let keys = [
        "scope",
        "run_id",
        "plan_digest",
        "max_submissions",
        "logical_reservations",
        "send_start_observations",
        "submission_observations",
        "active_or_quarantined_slots",
        "quarantined_slots",
        "terminal_submissions",
        "deadline_unix_ms",
    ];
    if !value
        .as_object()
        .is_some_and(|map| map.len() == keys.len() && keys.iter().all(|key| map.contains_key(*key)))
    {
        return Err(serde::de::Error::custom("rich_helper_snapshot_fields"));
    }
    serde_json::from_value(value).map_err(serde::de::Error::custom)
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Config {
    schema_version: String,
    corpus_root: PathBuf,
    generation: GenerationPin,
    query: InputFile,
    options: Options,
    expected_actual_model: String,
    context: ContextInput,
    output_dir: PathBuf,
}
struct Bound {
    config: Config,
    config_sha256: String,
    query: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Header {
    schema: String,
    run_id: String,
    plan_digest: String,
    deadline_unix_ms: Option<u64>,
    max_submissions: u32,
    max_concurrent_requests: usize,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Initialization {
    header: Header,
    clock_anchor: RunClockAnchor,
}
fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn valid_sha(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn clean_path(path: &Path, leaf_exists: bool) -> Result<()> {
    ensure!(path.is_absolute(), "rich_helper_absolute_path_required");
    let mut current = PathBuf::new();
    for component in path.components() {
        ensure!(
            matches!(component, Component::RootDir | Component::Normal(_)),
            "rich_helper_path_components"
        );
        current.push(component.as_os_str());
        if !leaf_exists && current == path {
            break;
        }
        ensure!(
            !fs::symlink_metadata(&current)?.file_type().is_symlink(),
            "rich_helper_symlink"
        );
    }
    Ok(())
}
fn bounded_read(path: &Path, cap: usize) -> Result<Vec<u8>> {
    clean_path(path, true)?;
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file() && metadata.len() <= cap as u64,
        "rich_helper_file_bound"
    );
    let mut bytes = Vec::new();
    Read::by_ref(&mut file)
        .take(cap as u64 + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= cap && bytes.len() as u64 == metadata.len(),
        "rich_helper_file_changed"
    );
    Ok(bytes)
}
fn decode<T: for<'de> Deserialize<'de>>(bytes: &[u8]) -> Result<T> {
    Ok(serde_json::from_value(gptgrep_jev::parse_unique_json(
        bytes,
    )?)?)
}
fn bind(config_path: &Path, expected_sha: &str) -> Result<Bound> {
    ensure!(valid_sha(expected_sha), "rich_helper_config_digest");
    let raw = bounded_read(config_path, CONFIG_CAP)?;
    ensure!(digest(&raw) == expected_sha, "rich_helper_config_binding");
    let config: Config = decode(&raw)?;
    ensure!(
        config.schema_version == "gptgrep.harbor.rich_helper.v1"
            && valid_sha(&config.query.sha256)
            && (1..=8192).contains(&config.query.bytes)
            && valid_sha(&config.context.initialization_sha256)
            && valid_sha(&config.context.original_clock_anchor_sha256)
            && valid_sha(&config.context.plan_digest)
            && (1..=200).contains(&config.context.max_submissions)
            && !config.context.run_id.is_empty()
            && config.context.run_id.len() <= 256
            && !config.context.run_id.chars().any(char::is_control)
            && config
                .expected_actual_model
                .starts_with("openai/gpt-6-luna-decisions-")
            && config.expected_actual_model.len() <= 128
            && config
                .expected_actual_model
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"/.-_".contains(&b))
            && (4..=2048).contains(&config.options.window_bytes)
            && (1..=EVIDENCE_CAP).contains(&config.options.max_output_bytes)
            && (1..=524288).contains(&config.options.max_metadata_bytes)
            && config
                .options
                .document
                .as_ref()
                .is_none_or(|d| !d.trim().is_empty() && d.len() <= 8192)
            && config.options.min_score.is_finite()
            && (0.0..=1.0).contains(&config.options.min_score),
        "rich_helper_config_fields"
    );
    for path in [&config.corpus_root, &config.context.ledger] {
        clean_path(path, true)?;
    }
    clean_path(&config.output_dir, false)?;
    ensure!(
        !config.output_dir.starts_with(&config.corpus_root)
            && !config.corpus_root.starts_with(&config.output_dir)
            && !config.context.ledger.starts_with(&config.output_dir)
            && !config.query.path.starts_with(&config.output_dir)
            && !config_path.starts_with(&config.output_dir),
        "rich_helper_output_overlap"
    );
    let query = bounded_read(&config.query.path, 8192)?;
    ensure!(
        query.len() as u64 == config.query.bytes && digest(&query) == config.query.sha256,
        "rich_helper_query_binding"
    );
    let query = String::from_utf8(query)?;
    ensure!(!query.trim().is_empty(), "rich_helper_query_empty");
    verify_initialization(&config.context)?;
    Ok(Bound {
        config,
        config_sha256: expected_sha.into(),
        query,
    })
}
fn lock_path(ledger: &Path) -> Result<PathBuf> {
    let name = ledger
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("rich_helper_ledger_path"))?;
    let mut name = name.to_os_string();
    name.push(".lock");
    Ok(ledger.with_file_name(name))
}
// Check an existing immutable initialization without adopting/sampling its clock.
// No missing journal/lock is created by this front door.
fn verify_initialization(input: &ContextInput) -> Result<()> {
    verify_initialization_calls(input, 2)
}
fn verify_initialization_calls(input: &ContextInput, calls: u32) -> Result<()> {
    bounded_read(&input.ledger, 131072)?;
    let lock = bounded_read(&lock_path(&input.ledger)?, 4096)?;
    ensure!(
        digest(&lock) == input.initialization_sha256,
        "rich_helper_initialization_binding"
    );
    let init: Initialization = decode(&lock)?;
    let h = &init.header;
    ensure!(
        h.schema == "gptgrep.decisions-budget.v1"
            && h.run_id == input.run_id
            && h.plan_digest == input.plan_digest
            && h.deadline_unix_ms == Some(input.absolute_deadline_unix_ms)
            && h.max_submissions == input.max_submissions
            && h.max_concurrent_requests == 4
            && init.clock_anchor.deadline_unix_ms() == input.absolute_deadline_unix_ms
            && digest(&serde_json::to_vec(&init.clock_anchor)?)
                == input.original_clock_anchor_sha256,
        "rich_helper_original_context_binding"
    );
    let before = &input.expected_before;
    ensure!(
        before.scope == "same_host_ledger"
            && before.run_id.as_deref() == Some(input.run_id.as_str())
            && before.plan_digest.as_deref() == Some(input.plan_digest.as_str())
            && before.deadline_unix_ms == Some(input.absolute_deadline_unix_ms)
            && before.max_submissions == input.max_submissions
            && before
                .logical_reservations
                .checked_add(calls as usize)
                .is_some_and(|n| n <= input.max_submissions as usize),
        "rich_helper_expected_context"
    );
    Ok(())
}
fn join_existing(input: &ContextInput) -> Result<ExecutionContext> {
    join_existing_calls(input, 2)
}
fn join_existing_calls(input: &ContextInput, calls: u32) -> Result<ExecutionContext> {
    verify_initialization_calls(input, calls)?;
    let context = ExecutionContext::new(
        &input.run_id,
        &input.plan_digest,
        UNIX_EPOCH + Duration::from_millis(input.absolute_deadline_unix_ms),
        input.max_submissions,
    )?
    .with_existing_ledger(&input.ledger)?;
    let anchor = context
        .ledger_clock_anchor()?
        .ok_or_else(|| anyhow::anyhow!("rich_helper_anchor_missing"))?;
    ensure!(
        digest(&serde_json::to_vec(&anchor)?) == input.original_clock_anchor_sha256
            && context.snapshot()? == input.expected_before,
        "rich_helper_context_changed"
    );
    context.check_deadline()?;
    Ok(context)
}
struct Limited<'a> {
    bytes: &'a mut Vec<u8>,
    cap: usize,
}
impl Write for Limited<'_> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if self
            .bytes
            .len()
            .checked_add(bytes.len())
            .is_none_or(|n| n > self.cap)
        {
            return Err(std::io::Error::other("rich_helper_output_cap"));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
struct ReservedOutput {
    accounting: File,
    evidence: File,
    dir: PathBuf,
}
impl ReservedOutput {
    fn create(bound: &Bound) -> Result<Self> {
        Self::create_at(
            &bound.config.output_dir,
            &bound.config_sha256,
            &bound.config.query.sha256,
            &bound.config.generation,
        )
    }
    fn create_at(
        dir: &Path,
        config_sha256: &str,
        query_sha256: &str,
        generation: &GenerationPin,
    ) -> Result<Self> {
        clean_path(dir, false)?;
        fs::DirBuilder::new().mode(0o700).create(dir)?;
        let mut claim = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(dir.join("consumed.json"))?;
        claim.write_all(&serde_json::to_vec(&json!({"schema_version":"gptgrep.rich_helper.once.v1","config_sha256":config_sha256,"query_sha256":query_sha256,"generation":generation,"retry":"forbidden"}))?)?;
        claim.sync_all()?;
        let create = |name: &str, cap| -> Result<File> {
            let file = OpenOptions::new()
                .write(true)
                .read(true)
                .create_new(true)
                .mode(0o600)
                .open(dir.join(name))?;
            file.set_len(cap)?;
            file.sync_all()?;
            Ok(file)
        };
        let accounting = create("accounting.json", ACCOUNT_CAP as u64)?;
        let mut evidence = create("reader-evidence.json", EVIDENCE_CAP as u64)?;
        save(
            &mut evidence,
            &json!({"status":"unavailable","reader_must_not_start":true}),
            EVIDENCE_CAP,
        )?;
        File::open(dir)?.sync_all()?;
        File::open(
            dir.parent()
                .ok_or_else(|| anyhow::anyhow!("rich_helper_output_parent"))?,
        )?
        .sync_all()?;
        Ok(Self {
            accounting,
            evidence,
            dir: dir.to_path_buf(),
        })
    }
}
fn save(file: &mut File, value: &Value, cap: usize) -> Result<()> {
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(cap)?;
    serde_json::to_writer(
        Limited {
            bytes: &mut bytes,
            cap,
        },
        value,
    )?;
    file.seek(SeekFrom::Start(0))?;
    file.write_all(&bytes)?;
    file.set_len(bytes.len() as u64)?;
    file.sync_all()?;
    Ok(())
}
struct HashWriter(Sha256);
impl Write for HashWriter {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.update(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
fn attempt_projection(attempt: &RichTextAttempt, expected_model: &str) -> Result<Value> {
    let usage = attempt.full_usage();
    let mut hash = HashWriter(Sha256::new());
    serde_json::to_writer(&mut hash, usage)?;
    let cost = usage
        .get("cost")
        .and_then(Value::as_f64)
        .filter(|n| n.is_finite() && *n >= 0.0);
    let decimal = usage
        .get("cost")
        .and_then(Value::as_number)
        .map(ToString::to_string)
        .filter(|s| s.len() <= 128 && cost.is_some());
    let model_matches = attempt.returned_model.as_deref() == Some(expected_model);
    Ok(json!({
        "stage":attempt.stage,"requested_model":attempt.requested_model,
        "prepared_request_sha256":attempt.prepared_request_sha256,
        "encoded_request_bytes":attempt.encoded_request_bytes,"question_count":attempt.question_count,
        "response_status":attempt.response_status,
        "actual_model":model_matches.then_some(expected_model),"actual_model_matches_expected":model_matches,
        "returned_model_sha256":attempt.returned_model.as_ref().map(|m|digest(m.as_bytes())),
        "input_tokens":usage.get("input_tokens").and_then(Value::as_u64),
        "output_tokens":usage.get("output_tokens").and_then(Value::as_u64),
        "cost":decimal.as_ref().and(cost),"cost_decimal":decimal,"cost_present":usage.get("cost").is_some(),
        "cost_projection_valid":cost.is_some() && decimal.is_some(),
        "complete_validated_usage_sha256":format!("{:x}",hash.0.finalize()),
        "usage_is_null":usage.is_null(),"projection_not_complete_dto":true,
        "actual_provider_requests":null
    }))
}
fn accounting(
    bound: &Bound,
    started: Instant,
    before: Option<&ExecutionSnapshot>,
    after: Option<&ExecutionSnapshot>,
    rows: &[Value],
    report: Option<&RichTextSearchReport>,
    outcome: (&str, Option<&str>),
) -> Value {
    let (status, code) = outcome;
    let delta = |a: usize, b: usize| a.checked_sub(b);
    let logical_reservations = before
        .zip(after)
        .and_then(|(b, a)| delta(a.logical_reservations, b.logical_reservations));
    let send_starts = before
        .zip(after)
        .and_then(|(b, a)| delta(a.send_start_observations, b.send_start_observations));
    let submission_observations = before.zip(after).and_then(|(b, a)| {
        a.submission_observations
            .checked_sub(b.submission_observations)
    });
    let terminal_submissions = before
        .zip(after)
        .and_then(|(b, a)| delta(a.terminal_submissions, b.terminal_submissions));
    let sum_tokens = |field: &str| {
        rows.iter()
            .try_fold(0u64, |sum, row| sum.checked_add(row[field].as_u64()?))
    };
    let input_tokens = sum_tokens("input_tokens");
    let output_tokens = sum_tokens("output_tokens");
    let reported_cost_sum = rows.iter().try_fold(0.0f64, |sum, row| {
        let total = sum + row["cost"].as_f64()?;
        total.is_finite().then_some(total)
    });
    let original_context_unchanged = before.zip(after).is_some_and(|(b, a)| {
        b.scope == a.scope
            && b.run_id == a.run_id
            && b.plan_digest == a.plan_digest
            && b.max_submissions == a.max_submissions
            && b.deadline_unix_ms == a.deadline_unix_ms
            && b.active_or_quarantined_slots == a.active_or_quarantined_slots
            && b.quarantined_slots == a.quarantined_slots
    });
    // This is deliberately distinct from the unmodified product status flag and
    // from physical/provider accounting. Concurrent unrelated ledger activity
    // makes attribution incomplete rather than being silently counted as ours.
    let logical_accounting_complete = status == "completed"
        && rows.len() == 2
        && report.is_some_and(|r| r.logical_calls_attempted == 2 && r.validated_responses == 2)
        && logical_reservations == Some(2)
        && send_starts == Some(2)
        && submission_observations == Some(2)
        && terminal_submissions == Some(2)
        && original_context_unchanged
        && input_tokens.is_some()
        && output_tokens.is_some()
        && reported_cost_sum.is_some()
        && rows.iter().all(|row| {
            row["actual_model_matches_expected"] == true && row["cost_projection_valid"] == true
        });
    json!({
        "schema_version":"gptgrep.harbor.rich_helper_accounting.v1",
        "status":status,"code":code,"config_sha256":bound.config_sha256,
        "generation":bound.config.generation,"query_sha256":bound.config.query.sha256,
        "requested_model":MODEL,"expected_actual_model":bound.config.expected_actual_model,
        "original_clock_anchor_sha256":bound.config.context.original_clock_anchor_sha256,
        "retrieval_wall_ms":started.elapsed().as_millis(),"timing_scope":"all_helper_execution_including_admission_and_preparation_is_retrieval",
        "excluded_as_indexing":false,"max_product_calls":2,
        "execution_before":before,"execution_after":after,"validated_observations":rows,
        "helper_logical_accounting_complete":logical_accounting_complete,
        "helper_accounting_scope":"two_validated_logical_calls_and_exact_shared_context_deltas_not_physical_requests_or_money_settlement",
        "logical_reservations_delta":logical_reservations,"send_start_observations_delta":send_starts,
        "submission_observations_delta":submission_observations,"terminal_submissions_delta":terminal_submissions,
        "observed_validated_input_tokens_sum":input_tokens,"observed_validated_output_tokens_sum":output_tokens,
        "observed_validated_cost_sum":reported_cost_sum,
        "product":report.map(|r|json!({"run_id":r.run_id,"plan_digest":r.plan_digest,"budget_scope":r.budget_scope,"original_clock_anchor_sha256":r.original_clock_anchor_sha256,"query_options_sha256":r.query_options_sha256,"logical_calls_attempted":r.logical_calls_attempted,"validated_responses":r.validated_responses,"accounting_complete":r.accounting_complete,"product_elapsed_ms":r.elapsed_ms,"coverage":r.coverage,"read_intervals":r.read_intervals,"read_meter_scope":r.read_meter_scope})),
        "failed_response_usage":"unavailable_when_core_required_stage_fails",
        "actual_provider_requests":null,"cost_qualification":"reported_numeric_observations_not_money_settlement",
        "qualification":"controlled_pipeline_source_only_not_interactive_tool_or_utility",
        "reader_must_not_start":status!="completed"
    })
}
async fn execute_bound<F>(bound: Bound, started: Instant, client_factory: F) -> Result<Value>
where
    F: FnOnce(ExecutionContext) -> Result<JevClient>,
{
    execute_bound_with_finalization_hook(bound, started, client_factory, |_, _| Ok(())).await
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum FinalizationPoint {
    BeforeEvidence,
    AfterAccounting,
}
// The production front door supplies only a no-op hook. The private seam lets
// synthetic tests expire the original context at either publication barrier.
async fn execute_bound_with_finalization_hook<F, H>(
    bound: Bound,
    started: Instant,
    client_factory: F,
    finalization_hook: H,
) -> Result<Value>
where
    F: FnOnce(ExecutionContext) -> Result<JevClient>,
    H: Fn(&ExecutionContext, FinalizationPoint) -> Result<()>,
{
    let mut output = ReservedOutput::create(&bound)?;
    save(
        &mut output.accounting,
        &accounting(&bound, started, None, None, &[], None, ("admission", None)),
        ACCOUNT_CAP,
    )?;
    let result = async {
        let context = join_existing(&bound.config.context)?;
        let before = context.snapshot()?;
        let reader = RichGenerationReader::open_pinned(&bound.config.corpus_root, &bound.config.generation)?;
        let client = client_factory(context.clone())?;
        let prepared = prepare_rich_text_search(reader, client, &bound.query, &bound.config.options.product())?;
        let full_pool_request_bytes = prepared.full_pool_request_bytes();
        let reserved_product_output_bytes = prepared.reserved_output_bytes();
        let private = Mutex::new((&mut output.accounting, Vec::<Value>::with_capacity(2)));
        let observer = |attempt: &RichTextAttempt| -> Result<()> {
            let row = attempt_projection(attempt, &bound.config.expected_actual_model)?;
            let accepted = row["actual_model_matches_expected"] == true
                && row["cost_projection_valid"] == true;
            let mut state = private.lock().map_err(|_| anyhow::anyhow!("rich_helper_observer_lock"))?;
            ensure!(state.1.len() < 2, "rich_helper_extra_observation");
            state.1.push(row);
            let value = accounting(&bound, started, Some(&before), None, &state.1, None, ("running", None));
            save(state.0, &value, ACCOUNT_CAP)?;
            ensure!(accepted, "rich_helper_model_or_usage_not_admitted");
            Ok(())
        };
        let result = prepared.execute_with_observer(&observer).await;
        let (_, rows) = private.into_inner().map_err(|_| anyhow::anyhow!("rich_helper_observer_lock"))?;
        let after = context.snapshot()?;
        let (report, status, code, stage) = match result {
            Ok(initial) => {
                initial.validate()?;
                (initial.into_report(), "completed", None, None)
            }
            Err(error) => (error.report, "failed", Some(error.code), Some(error.stage)),
        };
        let mut value = accounting(&bound, started, Some(&before), Some(&after), &rows, Some(&report), (status, code));
        value["full_pool_request_bytes"] = json!(full_pool_request_bytes);
        value["reserved_product_output_bytes"] = json!(reserved_product_output_bytes);
        value["failed_stage"] = json!(stage);
        ensure!(report.logical_calls_attempted <= 2 && report.validated_responses <= 2, "rich_helper_product_call_invariant");
        if status == "completed" {
            ensure!(report.logical_calls_attempted == 2 && rows.len() == 2, "rich_helper_required_two_stages");
            // Keep the product's exact hits, order, threshold and coverage; no new citations.
            let evidence = json!({"schema_version":"gptgrep.harbor.rich_helper_evidence.v1","status":"completed","generation":report.generation,"query_sha256":bound.config.query.sha256,"query_options_sha256":report.query_options_sha256,"hits":report.hits,"coverage":report.coverage,"citation_status":report.citation_status,"model_exposure_is_not_reader_delivery":true});
            finalization_hook(&context, FinalizationPoint::BeforeEvidence)?;
            context.check_deadline()?;
            save(&mut output.evidence, &evidence, EVIDENCE_CAP)?;
        }
        save(&mut output.accounting, &value, ACCOUNT_CAP)?;
        finalization_hook(&context, FinalizationPoint::AfterAccounting)?;
        context.check_deadline()?;
        Ok::<_, anyhow::Error>(value)
    }.await;
    match result {
        Ok(value) => Ok(value),
        Err(_) => {
            // Keep any already-written observation rows if a later local operation fails.
            let retained = bounded_read(&output.dir.join("accounting.json"), ACCOUNT_CAP)
                .ok()
                .and_then(|b| decode::<Value>(&b).ok())
                .unwrap_or(Value::Null);
            let mut value = accounting(
                &bound,
                started,
                None,
                None,
                &[],
                None,
                ("failed", Some("rich_helper_local_failure")),
            );
            value["prior_accounting"] = retained;
            save(
                &mut output.evidence,
                &json!({"status":"unavailable","reader_must_not_start":true}),
                EVIDENCE_CAP,
            )?;
            save(&mut output.accounting, &value, ACCOUNT_CAP)?;
            Ok(value)
        }
    }
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RichConfig {
    schema_version: String,
    corpus_root: PathBuf,
    generation: GenerationPin,
    query: InputFile,
    request: InputFile,
    expected_actual_model: String,
    context: ContextInput,
    max_calls: u32,
    /// Integrity binding only. The private coordinator owns dollar admission.
    expected_preparation_sha256: Option<String>,
    output_dir: PathBuf,
}
struct RichBound {
    config: RichConfig,
    config_path: PathBuf,
    config_sha256: String,
    query: String,
    request: RichBatchRequest,
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RichEvidenceEntry {
    candidate_id: String,
    evidence: EvidenceRef,
    score: f64,
    confidence: Option<f64>,
    literal_anchor: Option<bool>,
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RichEvidenceOperation {
    id: String,
    kind: String,
    entries: Vec<RichEvidenceEntry>,
    coverage: Value,
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RichEvidence {
    schema_version: String,
    status: String,
    generation: GenerationPin,
    query_sha256: String,
    config_sha256: String,
    request_sha256: String,
    preparation_sha256: String,
    operations: Vec<RichEvidenceOperation>,
    actual_image_delivery: bool,
    citation_authority: bool,
}
fn rich_operation(op: &RichBatchOperation) -> Result<(&str, &str, &str)> {
    match op {
        RichBatchOperation::SearchMultimodal {
            id,
            query,
            max_concurrent_batches,
            ..
        } => {
            ensure!(*max_concurrent_batches == 1, "rich_helper_serial_required");
            Ok((id, "search_multimodal", query))
        }
        RichBatchOperation::Rerank {
            id,
            query,
            max_concurrent_batches,
            ..
        } => {
            ensure!(*max_concurrent_batches == 1, "rich_helper_serial_required");
            Ok((id, "rerank", query))
        }
        _ => anyhow::bail!("rich_helper_paid_operations_only"),
    }
}
fn rich_file(input: &InputFile, cap: usize) -> Result<Vec<u8>> {
    ensure!(
        valid_sha(&input.sha256) && input.bytes > 0 && input.bytes <= cap as u64,
        "rich_helper_input_bound"
    );
    let bytes = bounded_read(&input.path, cap)?;
    ensure!(
        bytes.len() as u64 == input.bytes && digest(&bytes) == input.sha256,
        "rich_helper_input_binding"
    );
    Ok(bytes)
}
fn bind_rich(path: &Path, sha: &str) -> Result<RichBound> {
    ensure!(valid_sha(sha), "rich_helper_config_digest");
    let bytes = bounded_read(path, CONFIG_CAP)?;
    ensure!(digest(&bytes) == sha, "rich_helper_config_binding");
    let config: RichConfig = decode(&bytes)?;
    ensure!(
        config.schema_version == "gptgrep.harbor.rich_batch_helper.v1"
            && (1..=200).contains(&config.max_calls)
            && (1..=200).contains(&config.context.max_submissions)
            && valid_sha(&config.context.initialization_sha256)
            && valid_sha(&config.context.original_clock_anchor_sha256)
            && valid_sha(&config.context.plan_digest)
            && config.max_calls <= config.context.max_submissions
            && config
                .expected_preparation_sha256
                .as_ref()
                .is_none_or(|s| valid_sha(s))
            && config
                .expected_actual_model
                .starts_with("openai/gpt-6-luna-decisions-")
            && config.expected_actual_model.len() <= 128
            && config
                .expected_actual_model
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"/.-_".contains(&b)),
        "rich_helper_config_fields"
    );
    clean_path(&config.corpus_root, true)?;
    clean_path(&config.context.ledger, true)?;
    clean_path(&config.output_dir, false)?;
    for input in [
        path,
        &config.query.path,
        &config.request.path,
        &config.context.ledger,
    ] {
        ensure!(
            !input.starts_with(&config.output_dir),
            "rich_helper_output_overlap"
        );
    }
    ensure!(
        !config.output_dir.starts_with(&config.corpus_root)
            && !config.corpus_root.starts_with(&config.output_dir),
        "rich_helper_output_overlap"
    );
    let query = String::from_utf8(rich_file(&config.query, 8192)?)?;
    ensure!(!query.trim().is_empty(), "rich_helper_query_empty");
    let request: RichBatchRequest = decode(&rich_file(&config.request, CONFIG_CAP)?)?;
    ensure!(
        matches!(&request.generation, RichBatchGeneration::Exact(pin) if pin == &config.generation)
            && !request.operations.is_empty()
            && request.operations.len() <= 8
            && request.budget.max_decisions_submissions == config.max_calls
            && request.budget.max_output_bytes <= ACCOUNT_CAP - CONFIG_CAP - 32768,
        "rich_helper_request_binding"
    );
    let mut ids = BTreeSet::new();
    for op in &request.operations {
        let (id, _, text) = rich_operation(op)?;
        ensure!(
            text == query
                && !id.is_empty()
                && id.len() <= 64
                && ids.insert(id)
                && id
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'),
            "rich_helper_operation_query_binding"
        );
    }
    verify_initialization_calls(&config.context, config.max_calls)?;
    Ok(RichBound {
        config,
        config_path: path.into(),
        config_sha256: sha.into(),
        query,
        request,
    })
}
fn envelope(operation: &str, stage: &str, index: usize, request: &PreparedDecision) -> Value {
    json!({"operation":operation,"stage":stage,"index":index,
        "requested_model":request.requested_model(),"body_sha256":request.body_sha256(),
        "body_bytes":request.body_bytes(),"question_count":request.question_count()})
}
fn prepared_rich(
    bound: &RichBound,
    client: JevClient,
) -> Result<(RichGenerationReader, PreparedRichBatch, Value, String)> {
    let reader = RichGenerationReader::open_pinned_with_cache_budget(
        &bound.config.corpus_root,
        &bound.config.generation,
        &bound.request.budget.cache,
    )?;
    let prepared = prepare_rich_batch_with_reader(&reader, bound.request.clone(), Some(client))?;
    let searches: BTreeMap<_, _> = prepared.prepared_searches().collect();
    let reranks: BTreeMap<_, _> = prepared.prepared_reranks().collect();
    let mut envelopes = Vec::new();
    for (position, op) in bound.request.operations.iter().enumerate() {
        let (id, kind, _) = rich_operation(op)?;
        let before = envelopes.len();
        if kind == "search_multimodal" {
            let search = searches
                .get(id)
                .ok_or_else(|| anyhow::anyhow!("rich_helper_paid_item_not_prepared"))?;
            for (i, request) in search.routing_requests().enumerate() {
                envelopes.push(envelope(id, "document_routing", i, request));
            }
            for (i, request) in search.rerank_pool_requests().enumerate() {
                envelopes.push(envelope(id, "full_pool_reranking", i, request));
            }
        } else {
            let rerank = reranks
                .get(id)
                .ok_or_else(|| anyhow::anyhow!("rich_helper_paid_item_not_prepared"))?;
            for (i, request) in rerank.requests().enumerate() {
                envelopes.push(envelope(id, "exact_reranking", i, request));
            }
        }
        let calls = envelopes.len() - before;
        ensure!(
            calls > 0 && prepared.item_call_shares().get(position) == Some(&calls),
            "rich_helper_paid_item_share"
        );
    }
    ensure!(
        searches.len() + reranks.len() == bound.request.operations.len()
            && envelopes.len() == prepared.reserved_calls()
            && (1..=bound.config.max_calls as usize).contains(&envelopes.len()),
        "rich_helper_call_bound"
    );
    let plan = json!({
        "schema_version":"gptgrep.harbor.rich_preparation.v1",
        "corpus_root":bound.config.corpus_root,"generation":bound.config.generation,
        "query_sha256":bound.config.query.sha256,"request_sha256":bound.config.request.sha256,
        "requested_model":MODEL,"expected_actual_model":bound.config.expected_actual_model,
        "context":bound.config.context,"max_calls":bound.config.max_calls,
        "batch_budget":bound.request.budget,"generation_resources":reader.manifest()?.resource_budget,
        "reserved_calls":prepared.reserved_calls(),"envelopes":envelopes,
        "rerank_binding":"full_pool_envelopes_are_conservative_actual_subsets_are_bound_by_terminal_receipts",
        "money_authority":"external_owner_only"
    });
    let plan_bytes = serde_json::to_vec(&plan)?;
    ensure!(
        plan_bytes.len() <= CONFIG_CAP,
        "rich_helper_preparation_output_bound"
    );
    let sha = digest(&plan_bytes);
    Ok((reader, prepared, plan, sha))
}
fn rich_prepare_only(bound: &RichBound) -> Result<Value> {
    let context = join_existing_calls(&bound.config.context, bound.config.max_calls)?;
    let before = context.snapshot()?;
    let client = JevClient::new("unused-preparation-only", Some(MODEL))?
        .with_execution_context(context.clone());
    let (_, _, plan, sha) = prepared_rich(bound, client)?;
    ensure!(
        context.snapshot()? == before,
        "rich_helper_preparation_consumed_context"
    );
    context.check_deadline()?;
    Ok(
        json!({"schema_version":"gptgrep.harbor.rich_preparation_result.v1","status":"prepared",
        "preparation":plan,"preparation_sha256":sha,"execution_authorized":false,
        "effects":{"credentials":0,"reservations":0,"submissions":0,"context_consumption":0,"output_writes":0}}),
    )
}
fn validate_rich_coverage(kind: &str, value: &Value) -> Result<()> {
    let (counts, flags, fixed): (&[&str], &[&str], (&str, &str)) = match kind {
        "search_multimodal" => (
            &[
                "scoped_documents",
                "routed_documents",
                "selected_documents",
                "available_text_artifacts",
                "inspected_text_artifacts",
                "omitted_text_artifacts",
                "scanned_text_bytes",
                "potential_text_candidates",
                "potential_visual_candidates",
                "available_visual_candidates",
                "omitted_visual_candidates",
                "visual_pages_in_scope",
                "visual_pages_without_rendition",
                "union_text_candidates",
                "union_visual_candidates",
                "filtered_candidates",
            ],
            &[
                "lexical_truncated",
                "lexical_query_terms_truncated",
                "visual_enumeration_complete",
                "selected_visual_artifact_bytes_verified",
                "semantic_scope_complete",
            ],
            ("delivered_set_sufficiency", "unassessed"),
        ),
        "rerank" => (
            &[
                "supplied_candidates",
                "text_candidates",
                "visual_candidates",
                "scored_candidates",
            ],
            &["complete"],
            ("scope", "supplied_candidate_pool_only"),
        ),
        _ => anyhow::bail!("rich_helper_evidence_operation"),
    };
    let object = value
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("rich_helper_coverage"))?;
    ensure!(
        object.len() == counts.len() + flags.len() + 1
            && counts
                .iter()
                .all(|key| object.get(*key).and_then(Value::as_u64).is_some())
            && flags
                .iter()
                .all(|key| object.get(*key).and_then(Value::as_bool).is_some())
            && object.get(fixed.0).and_then(Value::as_str) == Some(fixed.1),
        "rich_helper_coverage"
    );
    Ok(())
}
fn validate_rich_reference(reader: &RichGenerationReader, evidence: &EvidenceRef) -> Result<()> {
    match evidence {
        EvidenceRef::NativeText { .. } | EvidenceRef::OcrText { .. } => {
            reader.validate_text_evidence(evidence)
        }
        EvidenceRef::VisualRegion { rendition_id, .. } => {
            let visual = reader.visual_reader()?;
            let file = visual.open_rendition(rendition_id, &reader.manifest()?.resource_budget)?;
            ensure!(
                serde_json::to_value(&file.candidate().evidence)?
                    == serde_json::to_value(evidence)?,
                "rich_helper_visual_reference"
            );
            visual.validate()
        }
    }
}
// Recheck only emitted references; Reader::validate alone does not bind raw sources.
fn validate_rich_delivery_sources(
    reader: &RichGenerationReader,
    evidence: &RichEvidence,
) -> Result<()> {
    for operation in &evidence.operations {
        for entry in &operation.entries {
            validate_rich_reference(reader, &entry.evidence)?;
        }
    }
    reader.validate()
}
fn rich_evidence(
    bound: &RichBound,
    reader: &RichGenerationReader,
    response: &RichBatchResponse,
    preparation_sha: &str,
) -> Result<RichEvidence> {
    ensure!(
        response.generation == bound.config.generation
            && response.items.len() == bound.request.operations.len()
            && response.coverage.failed == 0
            && response.coverage.not_executed == 0,
        "rich_helper_batch_incomplete"
    );
    let mut operations = Vec::new();
    for (op, item) in bound.request.operations.iter().zip(&response.items) {
        let (id, kind, _) = rich_operation(op)?;
        ensure!(
            item.id == id && item.status == RichBatchStatus::Ok && item.error.is_none(),
            "rich_helper_batch_item_failed"
        );
        let result = item
            .result
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("rich_helper_result_missing"))?;
        ensure!(
            result["generation"] == serde_json::to_value(&bound.config.generation)?
                && result["query"] == bound.query
                && result["accounting_complete"] == true,
            "rich_helper_result_binding"
        );
        let coverage = result["coverage"].clone();
        validate_rich_coverage(kind, &coverage)?;
        let entries = result[if kind == "rerank" {
            "ranked_candidates"
        } else {
            "hits"
        }]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("rich_helper_hits"))?;
        ensure!(entries.len() <= 128, "rich_helper_evidence_count");
        let mut kept = Vec::new();
        for entry in entries {
            let reference = if kind == "rerank" {
                &entry["evidence"]
            } else if entry["content"]["kind"] == "text" {
                &entry["content"]["window"]["evidence"]
            } else {
                &entry["content"]["candidate"]["evidence"]
            };
            let evidence: EvidenceRef = serde_json::from_value(reference.clone())?;
            validate_rich_reference(reader, &evidence)?;
            let candidate_id = entry["candidate_id"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("rich_helper_candidate"))?;
            ensure!(valid_sha(candidate_id), "rich_helper_candidate");
            let score = entry["score"]
                .as_f64()
                .filter(|v| v.is_finite() && (0.0..=1.0).contains(v))
                .ok_or_else(|| anyhow::anyhow!("rich_helper_score"))?;
            let confidence = if entry["confidence"].is_null() {
                None
            } else {
                Some(
                    entry["confidence"]
                        .as_f64()
                        .filter(|v| v.is_finite() && (0.0..=1.0).contains(v))
                        .ok_or_else(|| anyhow::anyhow!("rich_helper_confidence"))?,
                )
            };
            kept.push(RichEvidenceEntry {
                candidate_id: candidate_id.into(),
                evidence,
                score,
                confidence,
                literal_anchor: entry["content"]["literal_anchor"].as_bool(),
            });
        }
        operations.push(RichEvidenceOperation {
            id: id.into(),
            kind: kind.into(),
            entries: kept,
            coverage,
        });
    }
    reader.validate()?;
    Ok(RichEvidence {
        schema_version: "gptgrep.harbor.rich_helper_evidence.v2".into(),
        status: "completed".into(),
        generation: bound.config.generation.clone(),
        query_sha256: bound.config.query.sha256.clone(),
        config_sha256: bound.config_sha256.clone(),
        request_sha256: bound.config.request.sha256.clone(),
        preparation_sha256: preparation_sha.into(),
        operations,
        actual_image_delivery: false,
        citation_authority: false,
    })
}
fn rich_attempts_valid(response: &RichBatchResponse, expected_model: &str) -> Result<usize> {
    let mut total = 0usize;
    for item in &response.items {
        let result = item
            .result
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("rich_helper_result_missing"))?;
        ensure!(
            matches!(
                result["schema_version"].as_str(),
                Some("gptgrep.rich_multimodal_search.v1" | "gptgrep.rich_rerank.response.v1")
            ),
            "rich_helper_result_schema"
        );
        let attempts = result["attempts"]
            .as_array()
            .ok_or_else(|| anyhow::anyhow!("rich_helper_attempts_missing"))?;
        ensure!(!attempts.is_empty(), "rich_helper_attempts_missing");
        for attempt in attempts {
            ensure!(
                attempt["requested_model"] == MODEL
                    && attempt["returned_model"] == expected_model
                    && attempt["response_status"] == "validated"
                    && attempt["failure"].is_null()
                    && attempt["prepared_request_sha256"]
                        .as_str()
                        .is_some_and(valid_sha)
                    && attempt["encoded_request_bytes"]
                        .as_u64()
                        .is_some_and(|n| n > 0)
                    && attempt["usage"]["usage_observed"] == true
                    && attempt["usage"]["complete_usage_sha256"]
                        .as_str()
                        .is_some_and(valid_sha),
                "rich_helper_model_or_usage_not_admitted"
            );
            for name in ["input_tokens", "output_tokens"] {
                let field = &attempt["usage"][name];
                ensure!(
                    field["presence"] == "reported" && field["value"].as_u64().is_some(),
                    "rich_helper_usage_missing"
                );
            }
            let cost = &attempt["usage"]["cost"];
            ensure!(
                cost["presence"] == "reported"
                    && cost["value"]
                        .as_f64()
                        .is_some_and(|n| n.is_finite() && n >= 0.0),
                "rich_helper_cost_missing"
            );
            total += 1;
        }
    }
    Ok(total)
}
async fn execute_rich<F>(bound: RichBound, started: Instant, client_factory: F) -> Result<Value>
where
    F: FnOnce(ExecutionContext) -> Result<JevClient>,
{
    execute_rich_with_finalization_hook(bound, started, client_factory, |_, _| Ok(())).await
}
async fn execute_rich_with_finalization_hook<F, H>(
    bound: RichBound,
    started: Instant,
    client_factory: F,
    finalization_hook: H,
) -> Result<Value>
where
    F: FnOnce(ExecutionContext) -> Result<JevClient>,
    H: Fn(&ExecutionContext, FinalizationPoint) -> Result<()>,
{
    let expected = bound
        .config
        .expected_preparation_sha256
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("rich_helper_preparation_binding_required"))?
        .clone();
    let mut output = ReservedOutput::create_at(
        &bound.config.output_dir,
        &bound.config_sha256,
        &bound.config.query.sha256,
        &bound.config.generation,
    )?;
    let mut account = json!({"schema_version":"gptgrep.harbor.rich_batch_accounting.v1","status":"admission",
        "config_sha256":bound.config_sha256,"generation":bound.config.generation,
        "query_sha256":bound.config.query.sha256,"request_sha256":bound.config.request.sha256,
        "preparation_sha256":expected,"helper_logical_accounting_complete":false,
        "reader_must_not_start":true,"money_authority":"external_owner_only",
        "money_settlement":false,"actual_provider_requests":null,
        "excluded_as_indexing":false,"timing_scope":"all_query_dependent_helper_work_is_treatment_retrieval"});
    save(&mut output.accounting, &account, ACCOUNT_CAP)?;
    let result = async {
        let context = join_existing_calls(&bound.config.context, bound.config.max_calls)?;
        let before = context.snapshot()?;
        let (reader, prepared, plan, sha) =
            prepared_rich(&bound, client_factory(context.clone())?)?;
        ensure!(sha == expected, "rich_helper_preparation_changed");
        let reserved = prepared.reserved_calls();
        account["preparation"] = plan;
        account["execution_before"] = serde_json::to_value(&before)?;
        account["status"] = json!("running");
        save(&mut output.accounting, &account, ACCOUNT_CAP)?;
        let outcome = prepared.execute().await;
        account["batch"] = match &outcome {
            Ok(value) => serde_json::to_value(value)?,
            Err(value) => serde_json::to_value(value)?,
        };
        account["retrieval_wall_ms"] = json!(started.elapsed().as_millis());
        // Preserve actual subset hashes, byte bounds and usage before deciding delivery.
        save(&mut output.accounting, &account, ACCOUNT_CAP)?;
        let response = outcome?;
        let after = context.snapshot()?;
        account["execution_after"] = serde_json::to_value(&after)?;
        let actual = rich_attempts_valid(&response, &bound.config.expected_actual_model)?;
        let unchanged = before.scope == after.scope
            && before.run_id == after.run_id
            && before.plan_digest == after.plan_digest
            && before.max_submissions == after.max_submissions
            && before.deadline_unix_ms == after.deadline_unix_ms
            && before.active_or_quarantined_slots == after.active_or_quarantined_slots
            && before.quarantined_slots == after.quarantined_slots;
        ensure!(
            actual > 0
                && actual <= reserved
                && unchanged
                && after
                    .logical_reservations
                    .checked_sub(before.logical_reservations)
                    == Some(actual)
                && after
                    .send_start_observations
                    .checked_sub(before.send_start_observations)
                    == Some(actual)
                && after
                    .terminal_submissions
                    .checked_sub(before.terminal_submissions)
                    == Some(actual)
                && after
                    .submission_observations
                    .checked_sub(before.submission_observations)
                    == Some(actual as u64),
            "rich_helper_context_delta"
        );
        let evidence = rich_evidence(&bound, &reader, &response, &sha)?;
        bind_rich(&bound.config_path, &bound.config_sha256)?;
        finalization_hook(&context, FinalizationPoint::BeforeEvidence)?;
        context.check_deadline()?;
        validate_rich_delivery_sources(&reader, &evidence)?;
        context.check_deadline()?;
        let evidence_value = serde_json::to_value(&evidence)?;
        account["reader_evidence_sha256"] = json!(digest(&serde_json::to_vec(&evidence_value)?));
        account["helper_logical_accounting_complete"] = json!(true);
        account["reader_must_not_start"] = json!(false);
        account["status"] = json!("completed");
        account["logical_calls_observed"] = json!(actual);
        save(&mut output.evidence, &evidence_value, EVIDENCE_CAP)?;
        save(&mut output.accounting, &account, ACCOUNT_CAP)?;
        finalization_hook(&context, FinalizationPoint::AfterAccounting)?;
        context.check_deadline()?;
        validate_rich_delivery_sources(&reader, &evidence)?;
        context.check_deadline()?;
        Ok::<(), anyhow::Error>(())
    }
    .await;
    if result.is_err() {
        account["status"] = json!("failed");
        account["helper_logical_accounting_complete"] = json!(false);
        account["reader_must_not_start"] = json!(true);
        account["code"] = json!("rich_helper_failed_no_reader_delivery");
        save(
            &mut output.evidence,
            &json!({"status":"unavailable","reader_must_not_start":true}),
            EVIDENCE_CAP,
        )?;
        save(&mut output.accounting, &account, ACCOUNT_CAP)?;
    }
    Ok(account)
}
fn discovery() -> Value {
    json!({"schema_version":"gptgrep.harbor.rich_helper_frontdoor.v1","requested_model":MODEL,"max_calls":2,"default":"preflight","effects":0,"execution":"explicit --execute --config PATH --config-sha256 SHA","context":"existing_same_host_ledger_and_original_anchor_required","output":"separate_private_accounting_and_source_bound_reader_evidence","qualification":"controlled_bootstrap_pipeline_not_interactive_tool_or_utility"})
}
#[tokio::main(flavor = "current_thread")]
async fn main() {
    let cli = Cli::parse();
    let result = if cli.rich {
        match (&cli.config, &cli.config_sha256) {
            (Some(path), Some(sha)) => match bind_rich(path, sha) {
                Ok(bound) if cli.execute => {
                    execute_rich(bound, Instant::now(), |context| {
                        Ok(JevClient::from_env(Some(MODEL))?.with_execution_context(context))
                    })
                    .await
                }
                Ok(bound) if cli.prepare_rich => rich_prepare_only(&bound),
                Ok(bound) => Ok(
                    json!({"schema_version":"gptgrep.harbor.rich_batch_helper_preflight.v1",
                    "status":"bound","config_sha256":bound.config_sha256,
                    "generation":bound.config.generation,"query_sha256":bound.config.query.sha256,
                    "request_sha256":bound.config.request.sha256,"max_calls":bound.config.max_calls,
                    "prepared":false,"money_authority":"external_owner_only","effects":0}),
                ),
                Err(_) => Err(anyhow::anyhow!("rich_helper_input_not_admitted")),
            },
            _ => Ok(
                json!({"schema_version":"gptgrep.harbor.rich_batch_helper_frontdoor.v1",
                "default":"nonexecuting","preparation":"--rich --prepare-rich --config PATH --config-sha256 SHA",
                "execution":"--rich --execute --config PATH --config-sha256 SHA",
                "money_authority":"external_owner_required_before_execution","effects":0}),
            ),
        }
    } else {
        match (&cli.config, &cli.config_sha256) {
            (Some(path), Some(sha)) => {
                let started = Instant::now();
                match bind(path, sha) {
                    Ok(bound) if cli.execute => {
                        execute_bound(bound, started, |context| {
                            Ok(JevClient::from_env(Some(MODEL))?.with_execution_context(context))
                        })
                        .await
                    }
                    Ok(bound) => Ok(
                        json!({"schema_version":"gptgrep.harbor.rich_helper_preflight.v1","config_sha256":bound.config_sha256,"generation":bound.config.generation,"query_sha256":bound.config.query.sha256,"requested_model":MODEL,"max_calls":2,"effects":{"clock_adoption":0,"ledger_join_or_initialization":0,"credentials":0,"provider":0,"output_writes":0}}),
                    ),
                    Err(_) => Err(anyhow::anyhow!("rich_helper_input_not_admitted")),
                }
            }
            _ => Ok(discovery()),
        }
    };
    match result {
        Ok(value) => {
            println!(
                "{}",
                json!({"schema_version":value["schema_version"],"status":value["status"],"details":(!cli.execute).then_some(&value),"reader_must_not_start":value["reader_must_not_start"]})
            );
            if cli.execute && value["status"] != "completed" {
                std::process::exit(2);
            }
        }
        Err(_) => {
            eprintln!("rich_helper_failed_without_reader_admission");
            std::process::exit(2);
        }
    }
}

#[cfg(test)]
mod rich_helper_tests;
