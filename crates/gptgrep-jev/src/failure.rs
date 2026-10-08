//! Safe observations from an unsuccessful submission, never response or request contents.

use serde::Serialize;
use serde_json::Value;
use std::fmt;

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
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
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum UsagePresence {
    Absent,
    Null,
    Invalid,
    Reported,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct UsageObservation<T> {
    pub presence: UsagePresence,
    pub value: Option<T>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
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
}

/// Downcast an unsuccessful client result to retain safe typed observations.
/// No request, response, URL, headers, provider text or underlying error is stored.
#[derive(Debug, Clone)]
pub struct DecisionFailure {
    diagnostic: DecisionFailureDiagnostic,
    message: &'static str,
}

impl DecisionFailure {
    pub fn diagnostic(&self) -> &DecisionFailureDiagnostic {
        &self.diagnostic
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

    pub(crate) fn http(status: u16) -> Self {
        Self::new(
            DecisionFailureKind::HttpStatus,
            "Jev provider returned HTTP",
            Some(status),
            None,
        )
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

    fn new(
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
            },
            message,
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

    pub(crate) fn validation(error: &anyhow::Error, usage: FailedResponseUsage) -> Self {
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
        failure
    }

    pub(crate) fn with_response(mut self, status: u16, bytes: usize) -> Self {
        self.diagnostic.http_status = Some(status);
        self.diagnostic.response_bytes = Some(bytes);
        self
    }
}

impl fmt::Display for DecisionFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.diagnostic.kind == DecisionFailureKind::HttpStatus {
            match self.diagnostic.http_status {
                Some(status) => write!(f, "{} {status}; request was not retried", self.message),
                None => f.write_str(
                    "Jev provider returned an unavailable HTTP status; request was not retried",
                ),
            }
        } else {
            f.write_str(self.message)
        }
    }
}

impl std::error::Error for DecisionFailure {}
