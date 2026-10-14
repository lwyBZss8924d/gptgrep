//! Safe observations from an unsuccessful submission, never response or request contents.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{fmt, time::Duration};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DecisionFailureKind {
    Timeout,
    Deadline,
    Connect,
    Transport,
    ResponseBody,
    HttpStatus,
    ResponseSize,
    InvalidJson,
    ResponseSchema,
    ResponseValidation,
    /// The bounded transient retry submission was not admitted or was stopped
    /// before its HTTP send; any reservation it took remains charged.
    RetryAdmission,
    /// The bounded transient retry submission was sent, then a local step
    /// failed: its terminal ledger record, a caller observer, or local response
    /// handling. The provider may have completed and billed it. A validated
    /// response's usage is kept in `reported_usage`; otherwise usage is unknown.
    RetryPostSend,
    /// A caller observer failed on the initial submission. The typed observer
    /// diagnostic records whether HTTP send or response validation occurred.
    ObserverCallback,
}

/// The closed set of observer callbacks; arbitrary callback error text is never kept.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DecisionObserverCallback {
    SendStarted,
    ValidatedResponse,
    Failed,
}

impl DecisionObserverCallback {
    fn name(self) -> &'static str {
        match self {
            Self::SendStarted => "send_started",
            Self::ValidatedResponse => "validated_response",
            Self::Failed => "failed",
        }
    }
}

/// Safe progress observations retained when one or more callbacks fail.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct DecisionObserverFailure {
    /// Failed callbacks in invocation order for the final submission.
    pub callbacks: Vec<DecisionObserverCallback>,
    /// The validated model identifier requested by the caller.
    pub requested_model: String,
    /// HTTP send began; the reservation's earlier local observation alone is insufficient.
    pub send_started: bool,
    /// A reply passed validation before a local failure; its usage is retained separately.
    pub response_validated: bool,
}

/// Why a received transient status was not retried because of its
/// `Retry-After`. Only one non-negative integer number of seconds of at most
/// ten digits is honoured; any other returned form refuses the bounded retry
/// instead of being ignored.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RetryAfterRefusal {
    /// More than one `Retry-After` field was returned.
    Repeated,
    /// An unsigned integer of more than ten digits.
    OutOfRange,
    /// Not an unsigned integer: for example an HTTP-date, or a negative,
    /// signed, fractional or empty value.
    NotDeltaSeconds,
}

impl RetryAfterRefusal {
    fn reason(self) -> &'static str {
        match self {
            Self::Repeated => "its Retry-After header was repeated",
            Self::OutOfRange => "its Retry-After exceeded ten digits",
            Self::NotDeltaSeconds => {
                "its Retry-After was not a non-negative integer number of seconds"
            }
        }
    }
}

/// The `Retry-After` returned with an HTTP status failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RetryAfter {
    Absent,
    /// One non-negative integer number of seconds of at most ten digits.
    Seconds(Duration),
    /// Present in a form that cannot be honoured; it refuses the bounded retry.
    Unusable(RetryAfterRefusal),
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum UsagePresence {
    Absent,
    Null,
    Invalid,
    Reported,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UsageObservation<T> {
    pub presence: UsagePresence,
    pub value: Option<T>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FailedResponseUsage {
    pub input_tokens: UsageObservation<u64>,
    pub output_tokens: UsageObservation<u64>,
    pub cost: UsageObservation<f64>,
}

impl FailedResponseUsage {
    pub(crate) fn from_response(value: &Value) -> Self {
        let usage = value.get("usage");
        fn field<T>(
            usage: Option<&Value>,
            name: &str,
            parse: impl FnOnce(&Value) -> Option<T>,
        ) -> UsageObservation<T> {
            let value = match usage {
                None => None,
                Some(Value::Null) => Some(&Value::Null),
                Some(Value::Object(map)) => map.get(name),
                Some(_) => {
                    return UsageObservation {
                        presence: UsagePresence::Invalid,
                        value: None,
                    };
                }
            };
            match value {
                None => UsageObservation {
                    presence: UsagePresence::Absent,
                    value: None,
                },
                Some(Value::Null) => UsageObservation {
                    presence: UsagePresence::Null,
                    value: None,
                },
                Some(value) => match parse(value) {
                    Some(value) => UsageObservation {
                        presence: UsagePresence::Reported,
                        value: Some(value),
                    },
                    None => UsageObservation {
                        presence: UsagePresence::Invalid,
                        value: None,
                    },
                },
            }
        }
        Self {
            input_tokens: field(usage, "input_tokens", Value::as_u64),
            output_tokens: field(usage, "output_tokens", Value::as_u64),
            cost: field(usage, "cost", |value| {
                value
                    .as_f64()
                    .filter(|cost| cost.is_finite() && *cost >= 0.0)
            }),
        }
    }
}

/// Bounded provider identity fields from a parsed but rejected response.
///
/// `model` is the served identifier the provider returned, `id` its response
/// (generation) identifier and `provider` the serving provider name. Each is
/// retained only as a nonblank string of at most
/// [`crate::MAX_PROVIDER_METADATA_BYTES`] bytes without control characters;
/// anything else is `invalid` with no value. No other response field is kept.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct FailedResponseMetadata {
    pub model: UsageObservation<String>,
    pub id: UsageObservation<String>,
    pub provider: UsageObservation<String>,
}

impl FailedResponseMetadata {
    pub(crate) fn from_response(value: &Value) -> Self {
        let field = |name: &str| match value.get(name) {
            None => UsageObservation {
                presence: UsagePresence::Absent,
                value: None,
            },
            Some(Value::Null) => UsageObservation {
                presence: UsagePresence::Null,
                value: None,
            },
            Some(Value::String(text))
                if !text.trim().is_empty()
                    && text.len() <= crate::MAX_PROVIDER_METADATA_BYTES
                    && !text.chars().any(char::is_control) =>
            {
                UsageObservation {
                    presence: UsagePresence::Reported,
                    value: Some(text.clone()),
                }
            }
            Some(_) => UsageObservation {
                presence: UsagePresence::Invalid,
                value: None,
            },
        };
        Self {
            model: field("model"),
            id: field("id"),
            provider: field("provider"),
        }
    }
}

/// One failed submission of a logical Decisions request that the bounded
/// transient retry ([`crate::TransientRetry`]) then repeated with identical
/// bytes. It holds no request or response contents. The failed attempt keeps
/// its status; its usage and cost stay as reported, or unknown when the
/// provider reported none. An HTTP error body is not read, so the transient
/// statuses that admit a retry always leave `reported_usage` unknown (`None`),
/// never zero.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DecisionRetriedAttempt {
    /// One-based submission ordinal within the logical request.
    pub attempt: u32,
    pub kind: DecisionFailureKind,
    pub http_status: Option<u16>,
    /// Provider-reported usage of this failed submission; `None` means unknown.
    pub reported_usage: Option<FailedResponseUsage>,
    /// Actual delay before the next submission: the jittered backoff, or a
    /// longer honoured numeric `Retry-After`.
    pub retry_wait_ms: u64,
    /// Numeric `Retry-After` the provider returned for this attempt, if any.
    pub retry_after_ms: Option<u64>,
}

/// This is an observation, not a model/cost/citation acceptance or a retry permit.
/// Usage is independently checked numeric provider metadata from unique JSON; invalid
/// answers remain rejected and reported cost does not establish a final bill.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct DecisionFailureDiagnostic {
    pub kind: DecisionFailureKind,
    pub http_status: Option<u16>,
    pub response_bytes: Option<usize>,
    pub validation_code: Option<&'static str>,
    pub reported_usage: Option<FailedResponseUsage>,
    /// Served model, response ID and provider of a parsed response, including a
    /// validated reply followed by a local failure.
    /// Absent when no unique JSON response body was available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_metadata: Option<FailedResponseMetadata>,
    /// Earlier failed submissions of this same logical request that the bounded
    /// transient retry repeated, oldest first. The fields above describe the
    /// final submission. Empty, and omitted, when no retry was made.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub prior_attempts: Vec<DecisionRetriedAttempt>,
    /// Set when an attached bounded transient retry would otherwise have been
    /// considered for this transient status, but its `Retry-After` could not be
    /// honoured, so the request was not retried. Omitted otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_after_refusal: Option<RetryAfterRefusal>,
    /// Explicit callback failures and final submission progress. An existing
    /// provider failure keeps its kind, status and checked response observations.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observer_failure: Option<DecisionObserverFailure>,
}

/// Downcast an unsuccessful client result to retain safe typed observations.
/// No request contents, URL, headers, provider text or underlying error is stored;
/// only the requested model and numeric usage and bounded response identity metadata.
#[derive(Debug, Clone)]
pub struct DecisionFailure {
    diagnostic: DecisionFailureDiagnostic,
    message: &'static str,
    /// `Retry-After` of an HTTP status failure; never serialized.
    retry_after: RetryAfter,
}

impl DecisionFailure {
    pub fn diagnostic(&self) -> &DecisionFailureDiagnostic {
        &self.diagnostic
    }

    /// Numeric `Retry-After` returned with an HTTP status failure, if any.
    pub fn retry_after(&self) -> Option<Duration> {
        match self.retry_after {
            RetryAfter::Seconds(after) => Some(after),
            RetryAfter::Absent | RetryAfter::Unusable(_) => None,
        }
    }

    /// A returned `Retry-After` that cannot be honoured, if any.
    pub(crate) fn unusable_retry_after(&self) -> Option<RetryAfterRefusal> {
        match self.retry_after {
            RetryAfter::Unusable(refusal) => Some(refusal),
            RetryAfter::Absent | RetryAfter::Seconds(_) => None,
        }
    }

    /// Record that the bounded retry was refused because of `Retry-After`.
    pub(crate) fn refuse_retry(&mut self, refusal: RetryAfterRefusal) {
        self.diagnostic.retry_after_refusal = Some(refusal);
    }

    pub(crate) fn transport(
        error: &reqwest::Error,
        response: bool,
        status: Option<u16>,
        bytes: Option<usize>,
    ) -> Self {
        let (kind, message) = if error.is_timeout() {
            (
                DecisionFailureKind::Timeout,
                "Jev request timed out; usage may be unknown and the request was not retried",
            )
        } else if response {
            (
                DecisionFailureKind::ResponseBody,
                "Jev response transport failed; usage may be unknown and the request was not retried",
            )
        } else if error.is_connect() {
            (
                DecisionFailureKind::Connect,
                "Jev transport failed; usage may be unknown and the request was not retried",
            )
        } else {
            (
                DecisionFailureKind::Transport,
                "Jev transport failed; usage may be unknown and the request was not retried",
            )
        };
        Self::new(kind, message, status, bytes)
    }

    pub(crate) fn http(status: u16, retry_after: RetryAfter) -> Self {
        let mut failure = Self::new(
            DecisionFailureKind::HttpStatus,
            "Jev provider returned HTTP",
            Some(status),
            None,
        );
        failure.retry_after = retry_after;
        failure
    }

    /// The bounded retry submission failed admission or stopped before its
    /// HTTP send. The underlying admission error text is not retained.
    pub(crate) fn retry_admission() -> Self {
        Self::new(
            DecisionFailureKind::RetryAdmission,
            "Jev bounded retry was not admitted or sent; any reserved retry submission remains charged",
            None,
            None,
        )
    }

    /// The bounded retry submission was sent, then a local step failed. When its
    /// response had already been validated, that response's usage and identity
    /// are kept; otherwise its usage is unknown. The local error text is not kept.
    pub(crate) fn retry_post_send(
        validated: Option<(FailedResponseUsage, FailedResponseMetadata)>,
    ) -> Self {
        let mut failure = Self::new(
            DecisionFailureKind::RetryPostSend,
            "Jev bounded retry was sent, then a local accounting or observer step failed; the provider may have completed it",
            None,
            None,
        );
        if let Some((usage, metadata)) = validated {
            failure.diagnostic.reported_usage = Some(usage);
            failure.diagnostic.response_metadata = Some(metadata);
        }
        failure
    }

    pub(crate) fn observer_callback() -> Self {
        Self::new(
            DecisionFailureKind::ObserverCallback,
            "Jev submission failed during an observer callback; request was not retried",
            None,
            None,
        )
    }

    pub(crate) fn with_observer_failure(
        mut self,
        observer: DecisionObserverFailure,
        validated: Option<(FailedResponseUsage, FailedResponseMetadata)>,
    ) -> Self {
        self.diagnostic.observer_failure = Some(observer);
        if let Some((usage, metadata)) = validated {
            self.diagnostic.reported_usage = Some(usage);
            self.diagnostic.response_metadata = Some(metadata);
        }
        self
    }

    /// Attach the failed earlier submissions that the bounded retry repeated.
    pub(crate) fn with_prior_attempts(mut self, prior: Vec<DecisionRetriedAttempt>) -> Self {
        self.diagnostic.prior_attempts = prior;
        self
    }

    pub(crate) fn response_size(status: u16, bytes: Option<usize>) -> Self {
        Self::new(
            DecisionFailureKind::ResponseSize,
            "Jev response exceeded the byte limit",
            Some(status),
            bytes,
        )
    }

    pub(crate) fn deadline() -> Self {
        Self::new(
            DecisionFailureKind::Deadline,
            "Jev admission or request deadline expired; any reserved submission remains charged and unknown; request was not retried",
            None,
            None,
        )
    }

    pub(crate) fn new(
        kind: DecisionFailureKind,
        message: &'static str,
        status: Option<u16>,
        bytes: Option<usize>,
    ) -> Self {
        Self {
            diagnostic: DecisionFailureDiagnostic {
                kind,
                http_status: status,
                response_bytes: bytes,
                validation_code: None,
                reported_usage: None,
                response_metadata: None,
                prior_attempts: Vec::new(),
                retry_after_refusal: None,
                observer_failure: None,
            },
            message,
            retry_after: RetryAfter::Absent,
        }
    }

    pub(crate) fn invalid_json() -> Self {
        Self::new(
            DecisionFailureKind::InvalidJson,
            "Jev response is invalid JSON or contains duplicate keys",
            None,
            None,
        )
    }

    pub(crate) fn validation(
        error: &anyhow::Error,
        usage: FailedResponseUsage,
        metadata: FailedResponseMetadata,
    ) -> Self {
        // Only our validator's known static messages can become public diagnostics.
        // An unexpected future error falls back to a fixed string, never its Display.
        let (code, message) = match error.to_string().as_str() {
            "Jev response does not match the Decisions schema" => {
                ("schema", "Jev response does not match the Decisions schema")
            }
            "Jev response has an invalid model identifier" => (
                "model_identifier",
                "Jev response has an invalid model identifier",
            ),
            "Jev response has invalid provider metadata" => (
                "provider_metadata",
                "Jev response has invalid provider metadata",
            ),
            "Jev response answer IDs do not match the request" => (
                "answer_ids",
                "Jev response answer IDs do not match the request",
            ),
            "Jev response selected an unknown option" => {
                ("choice_option", "Jev response selected an unknown option")
            }
            "Jev response choice disagrees with its distribution" => (
                "choice_distribution",
                "Jev response choice disagrees with its distribution",
            ),
            "Jev response score is outside its rubric" => {
                ("score_range", "Jev response score is outside its rubric")
            }
            "Jev response score disagrees with its distribution" => (
                "score_distribution",
                "Jev response score disagrees with its distribution",
            ),
            "Jev response legend does not match its rubric" => (
                "score_legend",
                "Jev response legend does not match its rubric",
            ),
            "Jev response answer type does not match the request" => (
                "answer_type",
                "Jev response answer type does not match the request",
            ),
            "Jev response has invalid usage metadata" => {
                ("usage_shape", "Jev response has invalid usage metadata")
            }
            "Jev response has invalid token usage" => {
                ("token_usage", "Jev response has invalid token usage")
            }
            "Jev response has invalid cost metadata" => {
                ("cost_metadata", "Jev response has invalid cost metadata")
            }
            "Jev response contains an invalid probability or confidence" => (
                "probability",
                "Jev response contains an invalid probability or confidence",
            ),
            "Jev response distribution does not match its question" => (
                "distribution_keys",
                "Jev response distribution does not match its question",
            ),
            "Jev response probabilities do not sum to one" => (
                "distribution_sum",
                "Jev response probabilities do not sum to one",
            ),
            "Decisions response model does not match the requested model" => (
                "model_mismatch",
                "Decisions response model does not match the requested model",
            ),
            _ => ("other_validation", "Jev response failed validation"),
        };
        let kind = if code == "schema" {
            DecisionFailureKind::ResponseSchema
        } else {
            DecisionFailureKind::ResponseValidation
        };
        let mut failure = Self::new(kind, message, None, None);
        failure.diagnostic.validation_code = Some(code);
        failure.diagnostic.reported_usage = Some(usage);
        failure.diagnostic.response_metadata = Some(metadata);
        failure
    }

    pub(crate) fn with_response(mut self, status: u16, bytes: usize) -> Self {
        self.diagnostic.http_status = Some(status);
        self.diagnostic.response_bytes = Some(bytes);
        self
    }
}

impl DecisionFailure {
    /// Short sanitized outcome of the final submission after a bounded retry.
    fn retried_outcome(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let diagnostic = &self.diagnostic;
        match diagnostic.kind {
            DecisionFailureKind::HttpStatus => match diagnostic.http_status {
                Some(status) => write!(f, "HTTP {status}"),
                None => f.write_str("unavailable HTTP status"),
            },
            DecisionFailureKind::Timeout => f.write_str("request timed out, usage may be unknown"),
            DecisionFailureKind::Deadline => f.write_str(
                "admission or request deadline expired, any reserved submission remains charged and unknown",
            ),
            DecisionFailureKind::Connect | DecisionFailureKind::Transport => {
                f.write_str("transport failed, usage may be unknown")
            }
            DecisionFailureKind::ResponseBody => {
                f.write_str("response transport failed, usage may be unknown")
            }
            DecisionFailureKind::ResponseSize => f.write_str("response exceeded the byte limit"),
            DecisionFailureKind::InvalidJson => {
                f.write_str("response is invalid JSON or contains duplicate keys")
            }
            DecisionFailureKind::ResponseSchema | DecisionFailureKind::ResponseValidation => write!(
                f,
                "response failed validation ({})",
                diagnostic.validation_code.unwrap_or("other_validation")
            ),
            DecisionFailureKind::RetryAdmission => f.write_str(
                "not admitted or sent, any reserved retry submission remains charged",
            ),
            DecisionFailureKind::RetryPostSend => f.write_str(
                if diagnostic.reported_usage.is_some() {
                    "sent and validated, then a local accounting or observer step failed, its reported usage is retained"
                } else {
                    "sent, then a local accounting or observer step failed, the provider may have completed it and its usage is unknown"
                },
            ),
            DecisionFailureKind::ObserverCallback => f.write_str("observer callback failed"),
        }
    }
}

impl fmt::Display for DecisionFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if !self.diagnostic.prior_attempts.is_empty() {
            // One bounded transient retry was made; this failure is its outcome.
            f.write_str("Jev request failed after one bounded retry (")?;
            for prior in &self.diagnostic.prior_attempts {
                match prior.http_status {
                    Some(status) => write!(f, "attempt {}: HTTP {status}; ", prior.attempt)?,
                    None => write!(f, "attempt {}: failed; ", prior.attempt)?,
                }
            }
            write!(f, "attempt {}: ", self.diagnostic.prior_attempts.len() + 1)?;
            self.retried_outcome(f)?;
            let unknown = self
                .diagnostic
                .prior_attempts
                .iter()
                .any(|prior| prior.reported_usage.is_none());
            f.write_str(if unknown {
                "); earlier failed attempt usage is unknown; no further retry"
            } else {
                "); no further retry"
            })?;
        } else if self.diagnostic.kind == DecisionFailureKind::HttpStatus {
            match self.diagnostic.http_status {
                Some(status) => {
                    write!(f, "{} {status}; request was not retried", self.message)?;
                    match self.diagnostic.retry_after_refusal {
                        Some(refusal) => write!(f, " because {}", refusal.reason()),
                        None => Ok(()),
                    }
                }
                None => f.write_str(
                    "Jev provider returned an unavailable HTTP status; request was not retried",
                ),
            }?;
        } else {
            f.write_str(self.message)?;
        }
        if let Some(observer) = &self.diagnostic.observer_failure {
            f.write_str("; observer callbacks failed: ")?;
            for (index, callback) in observer.callbacks.iter().enumerate() {
                if index > 0 {
                    f.write_str(", ")?;
                }
                f.write_str(callback.name())?;
            }
        }
        Ok(())
    }
}

impl std::error::Error for DecisionFailure {}
