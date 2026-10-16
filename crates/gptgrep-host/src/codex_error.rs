use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::fmt;

#[path = "codex_error_diagnostics.rs"]
mod diagnostics;

/// Safe app-server error variants. Provider messages and details are never retained.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CodexErrorInfo {
    ContextWindowExceeded,
    SessionBudgetExceeded,
    UsageLimitExceeded,
    RateLimitExceeded,
    ServerOverloaded,
    CyberPolicy,
    MisalignmentPolicyViolation,
    HttpConnectionFailed,
    ResponseStreamConnectionFailed,
    InternalServerError,
    Unauthorized,
    BadRequest,
    ThreadRollbackFailed,
    SandboxError,
    ResponseStreamDisconnected,
    ResponseTooManyFailedAttempts,
    ActiveTurnNotSteerable,
    Other,
}
impl CodexErrorInfo {
    fn carries_http_status(self) -> bool {
        matches!(
            self,
            Self::HttpConnectionFailed
                | Self::ResponseStreamConnectionFailed
                | Self::ResponseStreamDisconnected
                | Self::ResponseTooManyFailedAttempts
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostProtocolErrorKind {
    TerminalError,
    MalformedError,
    IdentityMismatch,
    FailedTurn,
    InterruptedTurn,
    InvalidTurnStatus,
    ToolBudgetExhausted,
}

/// Dynamic retrieval budget only; the required initial Jev pass is separate.
/// Denied calls count recoverable responses, never additional tool execution.
/// Admitted calls include handlers returning recoverable argument errors.
/// The native PTC reader shares max_tool_calls with observed native items, so
/// its admitted count includes them and equals the limit at every denial, as
/// in the restricted reader; its native report keeps the GPTgrep/native split.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolBudgetDiagnostics {
    pub max_tool_calls: usize,
    pub admitted_tool_calls: usize,
    pub denied_tool_calls: usize,
    pub max_denied_tool_calls: usize,
}

/// A bounded, sanitized failure from an active Codex turn.
///
/// Retry notifications are server events, not physical or billed request counts.
/// Usage includes only observed numeric token fields and is never inferred as zero.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostProtocolError {
    pub kind: HostProtocolErrorKind,
    pub codex_error_info: Option<CodexErrorInfo>,
    pub will_retry: Option<bool>,
    pub http_status_code: Option<u16>,
    pub server_retry_notifications: usize,
    pub usage: Option<Value>,
    pub accounting_complete: bool,
    /// Closed classifications and bounded fingerprints; never provider text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diagnostics: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_budget: Option<ToolBudgetDiagnostics>,
}
impl HostProtocolError {
    pub fn code(&self) -> &'static str {
        match self.kind {
            HostProtocolErrorKind::TerminalError => "host_codex_terminal_error",
            HostProtocolErrorKind::MalformedError => "host_codex_malformed_error",
            HostProtocolErrorKind::IdentityMismatch => "host_codex_error_identity_mismatch",
            HostProtocolErrorKind::FailedTurn => "host_codex_failed_turn",
            HostProtocolErrorKind::InterruptedTurn => "host_codex_interrupted_turn",
            HostProtocolErrorKind::InvalidTurnStatus => "host_codex_invalid_turn_status",
            HostProtocolErrorKind::ToolBudgetExhausted => "host_tool_budget_exhausted",
        }
    }

    pub(crate) fn new(
        kind: HostProtocolErrorKind,
        error: &ParsedError,
        will_retry: Option<bool>,
        server_retry_notifications: usize,
        usage: Option<&Value>,
        tool_budget: Option<ToolBudgetDiagnostics>,
    ) -> Self {
        Self {
            kind,
            codex_error_info: error.info,
            will_retry,
            http_status_code: error.http_status_code,
            server_retry_notifications,
            usage: usage.and_then(observed_usage),
            accounting_complete: false,
            diagnostics: error.diagnostics.clone(),
            tool_budget,
        }
    }
}
impl fmt::Display for HostProtocolError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.code())?;
        if let Some(info) = self.codex_error_info {
            write!(formatter, " (codex_error_info={info:?}")?;
            if let Some(status) = self.http_status_code {
                write!(formatter, ", http_status_code={status}")?;
            }
            formatter.write_str(")")?;
        }
        Ok(())
    }
}
impl std::error::Error for HostProtocolError {}

/// Safe diagnostics from an OCR runtime error notification.
/// Display preserves the existing completion-unknown code. These observations
/// do not prove a terminal provider lifetime, billed requests or complete usage.
#[derive(Debug, Clone, Serialize)]
pub struct OcrRuntimeError {
    pub codex_error_info: Option<CodexErrorInfo>,
    pub http_status_code: Option<u16>,
    pub will_retry: Option<bool>,
    pub usage: Option<Value>,
    /// Validated intermediate server notifications observed on this same turn.
    /// This is not a physical provider attempt or billed request count.
    pub server_retry_notifications: usize,
    /// Closed classifications and bounded fingerprints; never provider text.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diagnostics: Option<Value>,
}

impl OcrRuntimeError {
    pub fn code(&self) -> &'static str {
        "ocr_runtime_error_completion_unknown"
    }

    pub(crate) fn new(error: &Value, will_retry: Option<bool>, usage: Option<&Value>) -> Self {
        let parsed = ParsedError::parse(error);
        Self {
            codex_error_info: parsed.valid.then_some(parsed.info).flatten(),
            http_status_code: parsed.valid.then_some(parsed.http_status_code).flatten(),
            will_retry,
            usage: usage.and_then(observed_usage),
            server_retry_notifications: 0,
            diagnostics: parsed.diagnostics,
        }
    }

    pub(crate) fn with_server_retry_notifications(mut self, count: usize) -> Self {
        self.server_retry_notifications = count;
        self
    }
}

impl fmt::Display for OcrRuntimeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.code())
    }
}
impl std::error::Error for OcrRuntimeError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DocumentRuntimeStage {
    Startup,
    TurnStart,
    ActiveTurn,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum DocumentStartupMethod {
    #[serde(rename = "initialize")]
    Initialize,
    #[serde(rename = "account/read")]
    AccountRead,
    #[serde(rename = "config/read")]
    ConfigRead,
    #[serde(rename = "skills/list")]
    SkillsList,
    #[serde(rename = "thread/start")]
    ThreadStart,
}

impl DocumentStartupMethod {
    pub(crate) fn parse(method: &str) -> Option<Self> {
        Some(match method {
            "initialize" => Self::Initialize,
            "account/read" => Self::AccountRead,
            "config/read" => Self::ConfigRead,
            "skills/list" => Self::SkillsList,
            "thread/start" => Self::ThreadStart,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum DocumentRuntimeEvent {
    #[serde(rename = "error")]
    Error,
    #[serde(rename = "configWarning")]
    ConfigWarning,
    #[serde(rename = "config/warning")]
    ConfigWarningSlash,
    #[serde(rename = "model/rerouted")]
    ModelRerouted,
}

impl DocumentRuntimeEvent {
    pub(crate) fn parse(method: &Value) -> Option<Self> {
        Some(match method.as_str()? {
            "error" => Self::Error,
            "configWarning" => Self::ConfigWarning,
            "config/warning" => Self::ConfigWarningSlash,
            "model/rerouted" => Self::ModelRerouted,
            _ => return None,
        })
    }
}

/// Completed RPC writes and matching replies only; not provider admission or usage.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct DocumentRuntimeTransport {
    pub thread_start_sent: bool,
    pub thread_start_acknowledged: bool,
    pub turn_start_sent: bool,
    pub turn_start_acknowledged: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DocumentRuntimeIdentity {
    Startup,
    GlobalNotification,
    Matched,
    Mismatch,
}

/// A fail-closed Document notification with no provider text or native identities.
/// Observed usage is numeric and identity-bound; absent evidence stays unknown.
#[derive(Debug, Clone, Serialize)]
pub struct DocumentRuntimeError {
    pub stage: DocumentRuntimeStage,
    pub startup_method: Option<DocumentStartupMethod>,
    pub event: DocumentRuntimeEvent,
    pub identity: DocumentRuntimeIdentity,
    pub transport: DocumentRuntimeTransport,
    pub codex_error_info: Option<CodexErrorInfo>,
    pub http_status_code: Option<u16>,
    pub will_retry: Option<bool>,
    /// Last valid identity-bound numeric observation, not cumulative billing.
    pub usage: Option<Value>,
    /// Matching updates without numeric fields do not erase the last observation.
    /// Absent when the refusal notification's identity does not match.
    pub usage_updates_without_numeric: Option<usize>,
    pub provider_request_count: Option<usize>,
    pub accounting_complete: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diagnostics: Option<Value>,
}

impl DocumentRuntimeError {
    pub fn code(&self) -> &'static str {
        "document_runtime_refused_no_retry"
    }

    pub(crate) fn new(
        stage: DocumentRuntimeStage,
        startup_method: Option<DocumentStartupMethod>,
        event: DocumentRuntimeEvent,
        identity: DocumentRuntimeIdentity,
        params: &Value,
        transport: DocumentRuntimeTransport,
        usage: Option<&Value>,
    ) -> Self {
        let parsed = if identity == DocumentRuntimeIdentity::Mismatch {
            ParsedError::default()
        } else if matches!(
            event,
            DocumentRuntimeEvent::ConfigWarning | DocumentRuntimeEvent::ConfigWarningSlash
        ) {
            // Native ConfigWarningNotification has summary/details, not the
            // error DTO's message/additionalDetails. Only these strings enter
            // the existing bounded projection; path/range are never retained.
            ParsedError {
                diagnostics: diagnostics::project_warning(params),
                ..ParsedError::default()
            }
        } else if event == DocumentRuntimeEvent::Error {
            ParsedError::parse(&params["error"])
        } else {
            ParsedError::default()
        };
        Self {
            stage,
            startup_method,
            event,
            identity,
            transport,
            codex_error_info: parsed.valid.then_some(parsed.info).flatten(),
            http_status_code: parsed.valid.then_some(parsed.http_status_code).flatten(),
            will_retry: (event == DocumentRuntimeEvent::Error
                && identity != DocumentRuntimeIdentity::Mismatch)
                .then(|| params["willRetry"].as_bool())
                .flatten(),
            usage: (identity != DocumentRuntimeIdentity::Mismatch)
                .then(|| usage.and_then(observed_usage))
                .flatten(),
            usage_updates_without_numeric: None,
            provider_request_count: None,
            accounting_complete: false,
            diagnostics: parsed.diagnostics,
        }
    }
}

impl fmt::Display for DocumentRuntimeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.code())
    }
}
impl std::error::Error for DocumentRuntimeError {}

#[derive(Default)]
pub(crate) struct ParsedError {
    pub info: Option<CodexErrorInfo>,
    pub http_status_code: Option<u16>,
    pub valid: bool,
    pub diagnostics: Option<Value>,
}
impl ParsedError {
    /// Narrow native recovery observation, without retaining provider strings.
    /// An announced retry may continue receiving this turn; it grants no new RPC.
    pub(crate) fn retryable_transport(error: &Value) -> bool {
        let parsed = Self::parse(error);
        if !parsed.valid
            || !matches!(
                parsed.info,
                Some(
                    CodexErrorInfo::HttpConnectionFailed
                        | CodexErrorInfo::ResponseStreamConnectionFailed
                        | CodexErrorInfo::ResponseStreamDisconnected
                )
            )
        {
            return false;
        }
        let object = error.as_object().expect("parsed error object");
        if !object.keys().all(|key| {
            matches!(
                key.as_str(),
                "message" | "codexErrorInfo" | "additionalDetails" | "misalignment"
            )
        }) || !matches!(
            object.get("additionalDetails"),
            None | Some(Value::Null | Value::String(_))
        ) || !matches!(object.get("misalignment"), None | Some(Value::Null))
        {
            return false;
        }
        // Transport variants have one object whose only optional field is HTTP status.
        // parse() validates its observed numeric range; absent status remains unknown.
        error["codexErrorInfo"]
            .as_object()
            .and_then(|info| info.values().next())
            .and_then(Value::as_object)
            .is_some_and(|details| details.keys().all(|key| key == "httpStatusCode"))
    }

    pub fn parse(error: &Value) -> Self {
        let mut result = Self {
            valid: error.is_object() && error["message"].is_string(),
            ..Self::default()
        };
        match error.get("codexErrorInfo") {
            None | Some(Value::Null) => {}
            Some(Value::String(name)) => {
                result.info = serde_json::from_value(Value::String(name.clone())).ok();
                result.valid &= result.info.is_some_and(|info| {
                    !info.carries_http_status() && info != CodexErrorInfo::ActiveTurnNotSteerable
                });
            }
            Some(Value::Object(object)) if object.len() == 1 => {
                let (name, details) = object.iter().next().expect("one variant");
                result.info = serde_json::from_value(Value::String(name.clone())).ok();
                result.valid &= details.is_object();
                match result.info {
                    Some(info) if info.carries_http_status() => {
                        match details.get("httpStatusCode") {
                            None | Some(Value::Null) => {}
                            Some(value) => {
                                result.http_status_code = value
                                    .as_u64()
                                    .filter(|status| (100..=599).contains(status))
                                    .map(|status| status as u16);
                                result.valid &= result.http_status_code.is_some();
                            }
                        }
                    }
                    Some(CodexErrorInfo::ActiveTurnNotSteerable) => {
                        result.valid &=
                            matches!(details["turnKind"].as_str(), Some("review" | "compact"));
                    }
                    _ => result.valid = false,
                }
            }
            _ => result.valid = false,
        }
        result.diagnostics = diagnostics::project(error, result.info, result.valid);
        result
    }
}

pub(crate) fn observed_usage(value: &Value) -> Option<Value> {
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
