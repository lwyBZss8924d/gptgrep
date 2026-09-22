use crate::{HostConfig, HostProtocolError, jev_accounting::Accounting};
use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::Instant,
};

/// One durably reserved model turn, including attempts with no observed turn identity.
/// Server retry notifications are events, not physical or billed request counts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelAttempt {
    pub attempt_id: String,
    pub role: String,
    pub status: String,
    pub requested_model: String,
    pub requested_reasoning_effort: String,
    pub requested_service_tier: String,
    pub model: Option<String>,
    pub model_provider: Option<String>,
    pub effective_reasoning_effort: Option<String>,
    pub effective_service_tier: Option<String>,
    pub thread_id: Option<String>,
    pub turn_id: Option<String>,
    /// Only allowlisted numeric fields from validated usage events are retained.
    pub usage: Option<Value>,
    pub elapsed_ms: u128,
    pub server_retry_notifications: usize,
    pub accounting_complete: bool,
    /// Bounded host codes or sanitized structured protocol errors; never provider text.
    pub error: Option<Value>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ModelTokenTotals {
    pub total_tokens: Option<u64>,
    pub input_tokens: Option<u64>,
    pub cached_input_tokens: Option<u64>,
    pub cache_write_input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub reasoning_output_tokens: Option<u64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ModelTokenMissingCounts {
    pub total_tokens: usize,
    pub input_tokens: usize,
    pub cached_input_tokens: usize,
    pub cache_write_input_tokens: usize,
    pub output_tokens: usize,
    pub reasoning_output_tokens: usize,
}

/// Totals use observed `total` fields once per ephemeral (thread, turn) identity.
/// `last` and `total` are never added together, and absent fields remain unknown.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ModelUsage {
    pub attempted_calls: usize,
    pub observed_turns: usize,
    pub usage_observed_turns: usize,
    pub missing_usage_attempts: usize,
    pub duplicate_turn_observations: usize,
    pub known_totals: ModelTokenTotals,
    pub missing_totals: ModelTokenMissingCounts,
    pub accounting_complete: bool,
}

pub(crate) fn summarize(attempts: &[ModelAttempt]) -> ModelUsage {
    let mut unique: BTreeMap<_, Vec<&ModelAttempt>> = BTreeMap::new();
    let mut unknown = 0;
    let mut duplicates = 0;
    for attempt in attempts {
        match (&attempt.thread_id, &attempt.turn_id) {
            (Some(thread), Some(turn)) => {
                let observed = unique.entry((thread, turn)).or_default();
                if !observed.is_empty() {
                    duplicates += 1;
                }
                observed.push(attempt);
            }
            _ => unknown += 1,
        }
    }
    let observed: Vec<_> = unique.values().collect();
    let aggregate = |field| {
        let mut total: Option<u64> = None;
        let mut missing = unknown;
        for turn in &observed {
            match turn
                .iter()
                .filter_map(|attempt| {
                    attempt
                        .usage
                        .as_ref()
                        .and_then(|usage| usage["total"][field].as_u64())
                })
                .max()
            {
                Some(value) => match total.unwrap_or(0).checked_add(value) {
                    Some(sum) => total = Some(sum),
                    // A count that cannot be represented is unavailable, not saturated.
                    None => return (None, unknown + observed.len()),
                },
                None => missing += 1,
            }
        }
        (total, missing)
    };
    let (total_tokens, total_missing) = aggregate("totalTokens");
    let (input_tokens, input_missing) = aggregate("inputTokens");
    let (cached_input_tokens, cached_missing) = aggregate("cachedInputTokens");
    let (cache_write_input_tokens, cache_write_missing) = aggregate("cacheWriteInputTokens");
    let (output_tokens, output_missing) = aggregate("outputTokens");
    let (reasoning_output_tokens, reasoning_missing) = aggregate("reasoningOutputTokens");
    let usage_observed_turns = observed
        .iter()
        .filter(|turn| {
            turn.iter().any(|attempt| {
                attempt
                    .usage
                    .as_ref()
                    .and_then(|usage| usage["total"].as_object())
                    .is_some_and(|total| total.values().any(|value| value.as_u64().is_some()))
            })
        })
        .count();
    ModelUsage {
        attempted_calls: attempts.len(),
        observed_turns: observed.len(),
        usage_observed_turns,
        missing_usage_attempts: unknown + observed.len() - usage_observed_turns,
        duplicate_turn_observations: duplicates,
        known_totals: ModelTokenTotals {
            total_tokens,
            input_tokens,
            cached_input_tokens,
            cache_write_input_tokens,
            output_tokens,
            reasoning_output_tokens,
        },
        missing_totals: ModelTokenMissingCounts {
            total_tokens: total_missing,
            input_tokens: input_missing,
            cached_input_tokens: cached_missing,
            cache_write_input_tokens: cache_write_missing,
            output_tokens: output_missing,
            reasoning_output_tokens: reasoning_missing,
        },
        accounting_complete: unknown == 0
            && duplicates == 0
            && !attempts.is_empty()
            && total_missing == 0
            && input_missing == 0
            && output_missing == 0
            && attempts.iter().all(|attempt| attempt.accounting_complete),
    }
}

pub(crate) struct ModelAttemptTracker {
    observer: ModelAttemptObserver,
    finished: bool,
}

#[derive(Clone)]
pub(crate) struct ModelAttemptObserver(Arc<Mutex<Tracking>>);

struct Tracking {
    attempt: ModelAttempt,
    started: Instant,
    accounting: Accounting,
}

impl ModelAttemptTracker {
    pub fn reserve(accounting: &Accounting, role: &str, config: &HostConfig) -> Result<Self> {
        let attempt = ModelAttempt {
            attempt_id: format!("{role}-1"),
            role: role.into(),
            status: "reserved".into(),
            requested_model: config.model.clone(),
            requested_reasoning_effort: config.reasoning_effort.clone(),
            requested_service_tier: config.service_tier.clone(),
            model: None,
            model_provider: None,
            effective_reasoning_effort: None,
            effective_service_tier: None,
            thread_id: None,
            turn_id: None,
            usage: None,
            elapsed_ms: 0,
            server_retry_notifications: 0,
            accounting_complete: false,
            error: None,
        };
        accounting.reserve_model_attempt(attempt.clone())?;
        Ok(Self {
            observer: ModelAttemptObserver(Arc::new(Mutex::new(Tracking {
                attempt,
                started: Instant::now(),
                accounting: accounting.clone(),
            }))),
            finished: false,
        })
    }

    pub fn observer(&self) -> &ModelAttemptObserver {
        &self.observer
    }

    pub fn finish(&mut self, error: Option<&anyhow::Error>) -> Result<()> {
        self.observer.update(|attempt| {
            attempt.status = if error.is_some() {
                "failed"
            } else {
                "completed"
            }
            .into();
            attempt.error = error.map(safe_error);
            attempt.accounting_complete = error.is_none()
                && attempt.thread_id.is_some()
                && attempt.turn_id.is_some()
                && attempt.usage.as_ref().is_some_and(|usage| {
                    ["totalTokens", "inputTokens", "outputTokens"]
                        .iter()
                        .all(|field| usage["total"][*field].as_u64().is_some())
                });
        })?;
        self.finished = true;
        Ok(())
    }
}

impl Drop for ModelAttemptTracker {
    fn drop(&mut self) {
        if !self.finished {
            let _ = self.observer.update(|attempt| {
                attempt.status = "interrupted".into();
                attempt.error = Some(json!({"code":"host_model_attempt_interrupted"}));
                attempt.accounting_complete = false;
            });
        }
    }
}

impl ModelAttemptObserver {
    pub fn running(&self) -> Result<()> {
        self.update(|attempt| attempt.status = "running".into())
    }

    pub fn thread(
        &self,
        thread: &str,
        model: &str,
        provider: &str,
        effort: Option<&str>,
        service_tier: Option<&str>,
    ) -> Result<()> {
        self.update(|attempt| {
            attempt.thread_id = Some(thread.into());
            attempt.model = Some(model.into());
            attempt.model_provider = Some(provider.into());
            attempt.effective_reasoning_effort = effort.map(str::to_owned);
            attempt.effective_service_tier = service_tier.map(str::to_owned);
        })
    }

    pub fn turn(&self, turn: &str) -> Result<()> {
        if self
            .0
            .lock()
            .map_err(|_| anyhow!("host_model_attempt_unavailable"))?
            .attempt
            .turn_id
            .as_deref()
            == Some(turn)
        {
            return Ok(());
        }
        self.update(|attempt| attempt.turn_id = Some(turn.into()))
    }

    pub fn usage(&self, usage: Option<&Value>) -> Result<()> {
        let usage = usage.and_then(observed_usage);
        if let Some(usage) = usage {
            self.update(|attempt| {
                let previous = attempt.usage.get_or_insert_with(|| json!({}));
                for (scope, value) in usage.as_object().expect("projected usage") {
                    if let Some(fields) = value.as_object() {
                        for (key, value) in fields {
                            previous[scope][key] = value.clone();
                        }
                    } else {
                        previous[scope] = value.clone();
                    }
                }
            })?;
        }
        Ok(())
    }

    pub fn retries(&self, count: usize) -> Result<()> {
        self.update(|attempt| attempt.server_retry_notifications = count)
    }

    pub fn process_completed(&self) -> Result<()> {
        self.update(|attempt| attempt.status = "process_completed".into())
    }

    fn update(&self, update: impl FnOnce(&mut ModelAttempt)) -> Result<()> {
        let mut tracking = self
            .0
            .lock()
            .map_err(|_| anyhow!("host_model_attempt_unavailable"))?;
        update(&mut tracking.attempt);
        tracking.attempt.elapsed_ms = tracking.started.elapsed().as_millis();
        tracking.accounting.record_model_attempt(&tracking.attempt)
    }
}

fn safe_error(error: &anyhow::Error) -> Value {
    if let Some(protocol) = error.downcast_ref::<HostProtocolError>() {
        return json!({"code":protocol.code(),"protocol":protocol});
    }
    if let Some(completion) = error.downcast_ref::<crate::CompletionError>() {
        return json!({"code":completion.code()});
    }
    let code = error.to_string();
    if code.starts_with("host_")
        && code.len() <= 128
        && code
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte == b'_')
    {
        json!({"code":code})
    } else {
        json!({"code":"host_model_attempt_failed"})
    }
}

fn observed_usage(value: &Value) -> Option<Value> {
    let mut result = Map::new();
    for scope in ["total", "last"] {
        let mut tokens = Map::new();
        for field in [
            "totalTokens",
            "inputTokens",
            "cachedInputTokens",
            "cacheWriteInputTokens",
            "outputTokens",
            "reasoningOutputTokens",
        ] {
            if let Some(number) = value[scope][field].as_u64() {
                tokens.insert(field.into(), number.into());
            }
        }
        if !tokens.is_empty() {
            result.insert(scope.into(), Value::Object(tokens));
        }
    }
    if let Some(number) = value["modelContextWindow"].as_u64() {
        result.insert("modelContextWindow".into(), number.into());
    }
    (!result.is_empty()).then_some(Value::Object(result))
}
