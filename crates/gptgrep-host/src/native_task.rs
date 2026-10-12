//! Structured work performed by the selected native Codex runtime.
use crate::{HostConfig, NativePtcReport, protocol, retrieval::hash};
use anyhow::{Result, anyhow, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{path::Path, sync::atomic::AtomicBool};
use tokio::time::Instant;

pub struct NativeTaskRequest<'a> {
    pub instructions: &'a str,
    pub state: &'a Value,
    pub schema: &'a Value,
    pub working_dir: &'a Path,
    pub deadline: Instant,
    pub max_output_bytes: usize,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct NativeTaskReport {
    pub schema_version: String,
    pub value: Value,
    pub thread_id: String,
    pub turn_id: String,
    pub model: String,
    pub model_provider: String,
    pub reasoning_effort: Option<String>,
    pub service_tier: Option<String>,
    pub usage: Option<Value>,
    pub server_retry_notifications: usize,
    pub elapsed_ms: u128,
    pub request_sha256: String,
    pub response_sha256: String,
    pub native_ptc: NativePtcReport,
    pub transport_bytes: usize,
    /// Private native permission observation; never project this object into public artifacts.
    pub permission_observation: Option<Value>,
}

/// Execute one native task under the original job deadline and validate its JSON output.
/// This uses the caller's selected native capabilities and named permission profile.
pub async fn run_native_task(
    request: NativeTaskRequest<'_>,
    config: &HostConfig,
) -> Result<NativeTaskReport> {
    run_native_task_cancellable(request, config, None, None).await
}

pub(crate) async fn run_native_task_cancellable(
    request: NativeTaskRequest<'_>,
    config: &HostConfig,
    cancel: Option<&crate::OcrCancellation>,
    cleanup_proof: Option<&AtomicBool>,
) -> Result<NativeTaskReport> {
    let mut config = config.clone();
    config.native_ptc = true;
    config.working_dir = Some(request.working_dir.to_path_buf());
    crate::validate_config(&config, "native_task")?;
    ensure!(
        request.max_output_bytes > 0,
        "host_native_task_output_limit"
    );
    ensure!(
        !request.instructions.trim().is_empty(),
        "host_native_task_instructions"
    );
    ensure!(
        request.schema.is_object() && !crate::completion::external_reference(request.schema),
        "host_native_task_schema"
    );
    let validator = jsonschema::validator_for(request.schema)
        .map_err(|_| anyhow!("host_native_task_schema"))?;
    let input = serde_json::to_vec(&(request.instructions, request.state, request.schema))?;
    ensure!(
        input.len() <= config.max_input_bytes,
        "host_native_task_input_limit"
    );
    let request_sha256 = hash(&input);
    let completed = crate::run_process_until(
        &config,
        protocol::Workflow::NativeTask {
            instructions: request.instructions,
            state: request.state,
            schema: request.schema,
        },
        Some(request.deadline),
        protocol::RunOptions {
            max_output_bytes: Some(request.max_output_bytes),
            cancel,
            cleanup_proof,
            ..Default::default()
        },
    )
    .await?;
    let result = completed.result;
    ensure!(
        result.answer.len() <= request.max_output_bytes,
        "host_native_task_output_limit"
    );
    let response_sha256 = hash(result.answer.as_bytes());
    let value: Value = serde_json::from_str(&result.answer)
        .map_err(|_| anyhow!("host_native_task_invalid_output"))?;
    ensure!(
        validator.is_valid(&value),
        "host_native_task_invalid_output"
    );
    let native_ptc = result
        .native_ptc
        .ok_or_else(|| anyhow!("host_native_task_observation_missing"))?;
    ensure!(
        native_ptc.successful_turn_completed_observed && !native_ptc.budget_exceeded,
        "host_native_task_incomplete"
    );
    Ok(NativeTaskReport {
        schema_version: "gptgrep.native-task.v1".into(),
        value,
        thread_id: result.thread_id,
        turn_id: result.turn_id,
        model: result.model,
        model_provider: result.provider,
        reasoning_effort: result.effort,
        service_tier: result.service_tier,
        usage: result.usage,
        server_retry_notifications: result.server_retry_notifications,
        elapsed_ms: completed.elapsed_ms,
        request_sha256,
        response_sha256,
        native_ptc,
        transport_bytes: completed.transport_bytes,
        permission_observation: result.permission_observation,
    })
}
