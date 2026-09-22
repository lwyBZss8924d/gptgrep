use crate::{
    HostConfig, ProcessOutcome, protocol, retrieval::hash, run_process_until, validate_config,
};
use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{fmt, path::PathBuf};

pub const MAX_COMPLETION_INPUT_BYTES: usize = 1024 * 1024;
pub const MAX_COMPLETION_OUTPUT_BYTES: usize = 128 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompletionError {
    InputLimit,
    InvalidSchema,
    OutputLimit,
    InvalidOutput,
}
impl CompletionError {
    pub fn code(self) -> &'static str {
        match self {
            Self::InputLimit => "host_input_limit",
            Self::InvalidSchema => "host_invalid_schema",
            Self::OutputLimit => "host_output_limit",
            Self::InvalidOutput => "host_invalid_output",
        }
    }
}
impl fmt::Display for CompletionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.code())
    }
}
impl std::error::Error for CompletionError {}

#[derive(Debug, Serialize, Deserialize)]
pub struct CompletionReport {
    pub schema_version: String,
    pub status: String,
    pub value: Value,
    pub thread_id: String,
    pub turn_id: String,
    pub requested_model: String,
    pub model: String,
    pub model_provider: String,
    pub requested_reasoning_effort: String,
    pub effective_reasoning_effort: Option<String>,
    pub requested_service_tier: String,
    pub effective_service_tier: Option<String>,
    pub server_retry_notifications: usize,
    pub auth_mode: String,
    pub codex_home: PathBuf,
    pub usage: Option<Value>,
    pub elapsed_ms: u128,
    pub input_bytes: usize,
    pub instructions_sha256: String,
    pub state_sha256: String,
    pub schema_sha256: String,
    pub stderr_bytes: Option<u64>,
    pub stderr_truncated: Option<bool>,
    pub warnings: Vec<String>,
}

/// Perform a bounded JSON completion without an index or dynamic tools.
///
/// The schema is passed through unchanged and validated locally. No string/number coercion,
/// Markdown-fence removal, external schema resolution, or input truncation is performed.
pub async fn complete_json(
    instructions: &str,
    state: Value,
    schema: Value,
    config: &HostConfig,
) -> Result<CompletionReport> {
    if config.query_plan.is_some() {
        return Err(anyhow!("host_query_plan_requires_ask"));
    }
    complete_json_until(
        instructions,
        state,
        schema,
        config,
        None,
        protocol::RunOptions::default(),
    )
    .await
}

pub(crate) async fn complete_json_until(
    instructions: &str,
    state: Value,
    schema: Value,
    config: &HostConfig,
    deadline: Option<tokio::time::Instant>,
    options: protocol::RunOptions<'_>,
) -> Result<CompletionReport> {
    validate_config(config, "complete_json")?;
    let (validator, input_bytes) = prepare(instructions, &state, &schema, config.max_input_bytes)?;
    let completed = run_process_until(
        config,
        protocol::Workflow::Completion {
            instructions,
            state: &state,
            schema: &schema,
        },
        deadline,
        options,
    )
    .await?;
    let ProcessOutcome {
        result,
        home,
        elapsed_ms,
        stderr_bytes,
        stderr_truncated,
    } = completed;
    if result.answer.len()
        > options
            .max_output_bytes
            .unwrap_or(MAX_COMPLETION_OUTPUT_BYTES)
    {
        return Err(CompletionError::OutputLimit.into());
    }
    let value = validate_output(&result.answer, &validator)?;
    Ok(CompletionReport {
        schema_version: "gptgrep.completion.v1".into(),
        status: "completed".into(),
        value,
        thread_id: result.thread_id,
        turn_id: result.turn_id,
        requested_model: config.model.clone(),
        model: result.model,
        model_provider: result.provider,
        requested_reasoning_effort: config.reasoning_effort.clone(),
        effective_reasoning_effort: result.effort,
        requested_service_tier: config.service_tier.clone(),
        effective_service_tier: result.service_tier,
        server_retry_notifications: result.server_retry_notifications,
        auth_mode: "chatgpt".into(),
        codex_home: home,
        usage: result.usage,
        elapsed_ms,
        input_bytes,
        instructions_sha256: hash(instructions.as_bytes()),
        state_sha256: hash(&serde_json::to_vec(&state)?),
        schema_sha256: hash(&serde_json::to_vec(&schema)?),
        stderr_bytes,
        stderr_truncated,
        warnings: result.warnings,
    })
}

pub(crate) fn prepare(
    instructions: &str,
    state: &Value,
    schema: &Value,
    max_input_bytes: usize,
) -> Result<(jsonschema::Validator, usize)> {
    if max_input_bytes == 0
        || max_input_bytes > MAX_COMPLETION_INPUT_BYTES
        || instructions.trim().is_empty()
    {
        return Err(CompletionError::InputLimit.into());
    }
    #[derive(Serialize)]
    struct Input<'a> {
        instructions: &'a str,
        state: &'a Value,
        schema: &'a Value,
    }
    let mut counter = InputCounter {
        bytes: 0,
        limit: max_input_bytes,
    };
    serde_json::to_writer(
        &mut counter,
        &Input {
            instructions,
            state,
            schema,
        },
    )
    .map_err(|_| anyhow!(CompletionError::InputLimit))?;
    let input_bytes = counter.bytes;
    if !schema.is_object() || external_reference(schema) {
        return Err(CompletionError::InvalidSchema.into());
    }
    let validator =
        jsonschema::validator_for(schema).map_err(|_| anyhow!(CompletionError::InvalidSchema))?;
    Ok((validator, input_bytes))
}

struct InputCounter {
    bytes: usize,
    limit: usize,
}
impl std::io::Write for InputCounter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if self.bytes.saturating_add(bytes.len()) > self.limit {
            return Err(std::io::Error::other("input limit"));
        }
        self.bytes += bytes.len();
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn external_reference(value: &Value) -> bool {
    match value {
        Value::Object(object) => object.iter().any(|(key, value)| {
            (matches!(key.as_str(), "$ref" | "$dynamicRef" | "$recursiveRef")
                && value
                    .as_str()
                    .is_some_and(|reference| !reference.starts_with('#')))
                || external_reference(value)
        }),
        Value::Array(values) => values.iter().any(external_reference),
        _ => false,
    }
}

pub(crate) fn validate_output(raw: &str, validator: &jsonschema::Validator) -> Result<Value> {
    if raw.len() > MAX_COMPLETION_OUTPUT_BYTES {
        return Err(CompletionError::OutputLimit.into());
    }
    let value: Value =
        serde_json::from_str(raw).map_err(|_| anyhow!(CompletionError::InvalidOutput))?;
    if !validator.is_valid(&value) {
        return Err(CompletionError::InvalidOutput.into());
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn prepare(
        instructions: &str,
        state: &Value,
        schema: &Value,
    ) -> Result<(jsonschema::Validator, usize)> {
        super::prepare(instructions, state, schema, 256 * 1024)
    }
    #[test]
    fn validates_without_coercion_or_truncation() {
        let schema = json!({"type":"object","properties":{"count":{"type":"integer"}},"required":["count"],"additionalProperties":false});
        let (validator, _) = prepare("Count.", &json!({}), &schema).unwrap();
        assert_eq!(
            validate_output(r#"{"count":2}"#, &validator).unwrap(),
            json!({"count":2})
        );
        assert!(validate_output(r#"{"count":"2"}"#, &validator).is_err());
        assert!(validate_output(r#"{"count":2,"extra":true}"#, &validator).is_err());
        assert!(validate_output("not-json", &validator).is_err());
        assert!(
            prepare(
                "Answer.",
                &json!("x".repeat(MAX_COMPLETION_INPUT_BYTES)),
                &schema
            )
            .is_err()
        );
        assert!(prepare("Answer.", &json!({}), &json!({"$ref":"file:///forbidden"})).is_err());
        assert!(
            prepare(
                "Answer.",
                &json!({}),
                &json!({"$ref":"https://example.invalid/schema"})
            )
            .is_err()
        );
        let (blank, _) = prepare("Return an empty object.", &json!({}), &json!({})).unwrap();
        assert_eq!(validate_output("{}", &blank).unwrap(), json!({}));
    }
}
