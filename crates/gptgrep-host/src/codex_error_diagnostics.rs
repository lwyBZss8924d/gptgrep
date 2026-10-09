//! Closed error projections. Text is inspected in memory, never copied into a
//! receipt, trace, Display or Debug value. Unknown evidence stays unknown.
use super::CodexErrorInfo;
use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const FIELD_LIMIT: usize = 16_384;

#[derive(Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Class {
    ToolDefinition,
    OutputSchema,
    Model,
    Authentication,
    Authorization,
    Transport,
    Server,
    RateLimit,
    ResourceLimit,
    Request,
    Policy,
    Sandbox,
    Unknown,
}

#[derive(Serialize)]
struct Evidence {
    source: &'static str,
    basis: &'static str,
    classification: Class,
    code: Option<&'static str>,
    error_type: Option<&'static str>,
    parameter_family: Option<&'static str>,
    signature: Option<&'static str>,
    http_status_code: Option<u16>,
    conflicting: bool,
}

pub(super) fn project(error: &Value, info: Option<CodexErrorInfo>, valid: bool) -> Option<Value> {
    let object = error.as_object()?;
    let mut evidence = Vec::with_capacity(3);
    if valid {
        let class = native_class(info);
        if class != Class::Unknown {
            evidence.push(Evidence {
                source: "codex_error_info",
                basis: "typed_variant",
                classification: class,
                code: None,
                error_type: None,
                parameter_family: None,
                signature: None,
                http_status_code: None,
                conflicting: false,
            });
        }
    }
    for field in ["message", "additionalDetails"] {
        if let Some(text) = object
            .get(field)
            .and_then(Value::as_str)
            .filter(|text| text.len() <= FIELD_LIMIT)
            && let Some(observed) = inspect(text, field)
        {
            evidence.push(observed);
        }
    }
    let (class, conflict) = merge(evidence.iter().map(|item| item.classification));
    let conflict = conflict || evidence.iter().any(|item| item.conflicting);
    Some(json!({
        "schema_version":"gptgrep.codex.error-diagnostics.v1",
        "classification":if conflict { Class::Unknown } else { class },
        "conflicting_evidence":conflict,"native_error_shape_valid":valid,
        "message":fingerprint(object.get("message")),
        "additional_details":fingerprint(object.get("additionalDetails")),
        "evidence":evidence,
        "field_limit_bytes":FIELD_LIMIT
    }))
}

fn fingerprint(value: Option<&Value>) -> Value {
    match value {
        Some(Value::String(text)) => {
            let inspected = text.len() <= FIELD_LIMIT;
            json!({"shape":"string","bytes":text.len(),"inspected":inspected,
                "sha256":inspected.then(|| format!("{:x}", Sha256::digest(text.as_bytes())))})
        }
        other => json!({"shape":match other {
            None => "missing", Some(Value::Null) => "null", Some(Value::Bool(_)) => "boolean",
            Some(Value::Number(_)) => "number", Some(Value::Array(_)) => "array",
            Some(Value::Object(_)) => "object", Some(Value::String(_)) => unreachable!(),
        },"bytes":null,"inspected":false,"sha256":null}),
    }
}

fn inspect(text: &str, source: &'static str) -> Option<Evidence> {
    let text = text.trim();
    if let Some((body, basis, status)) = envelope(text) {
        let (code, code_class) = code(body["code"].as_str());
        let (error_type, type_class) = error_type(body["type"].as_str());
        let (parameter_family, param_class) = parameter(body["param"].as_str());
        let signature = body["message"].as_str().and_then(message_signature);
        let (classification, conflicting) = merge([
            code_class,
            type_class,
            param_class,
            signature
                .as_ref()
                .map_or(Class::Unknown, |(_, class)| *class),
        ]);
        return Some(Evidence {
            source,
            basis,
            classification,
            code,
            error_type,
            parameter_family,
            signature: signature.map(|(name, _)| name),
            http_status_code: status,
            conflicting,
        });
    }
    let (signature, classification) = message_signature(text)?;
    Some(Evidence {
        source,
        basis: "anchored_message_signature",
        classification,
        code: None,
        error_type: None,
        parameter_family: None,
        signature: Some(signature),
        http_status_code: None,
        conflicting: false,
    })
}

/// Recognize a complete JSON error or a bounded native HTTP error wrapper.
/// No arbitrary substring JSON discovery and no URL/header extraction.
fn envelope(text: &str) -> Option<(Value, &'static str, Option<u16>)> {
    let (body_text, basis, status, allow_suffix) = if text.starts_with('{') {
        (text, "structured_error", None, false)
    } else {
        let rest = text.strip_prefix("unexpected status ")?;
        let digits = rest.get(..3)?;
        if !digits.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        let status: u16 = digits
            .parse()
            .ok()
            .filter(|status| (100..=599).contains(status))?;
        let rest = rest.get(3..)?;
        let (reason, body) = rest.split_once(':')?;
        if reason.len() > 64
            || !reason
                .bytes()
                .all(|byte| byte.is_ascii_alphabetic() || matches!(byte, b' ' | b'-'))
        {
            return None;
        }
        (
            body.trim_start(),
            "native_http_error_envelope",
            Some(status),
            true,
        )
    };
    if !body_text.starts_with('{') {
        return None;
    }
    // The stream offset excludes native URL/request-id suffixes. Reparse through
    // the unique-key parser so duplicated diagnostic keys never choose a cause.
    let json_text = if allow_suffix {
        let mut stream = serde_json::Deserializer::from_str(body_text).into_iter::<Value>();
        stream.next()?.ok()?;
        let offset = stream.byte_offset();
        if !allowed_http_suffix(body_text.get(offset..)?) {
            return None;
        }
        body_text.get(..offset)?
    } else {
        body_text
    };
    let parsed = gptgrep_jev::parse_unique_json(json_text.as_bytes()).ok()?;
    let object = parsed.as_object()?;
    let body = match object.get("error") {
        Some(Value::Object(body)) => Value::Object(body.clone()),
        Some(_) => return None,
        None if ["code", "type", "param"]
            .iter()
            .any(|key| object.contains_key(*key)) =>
        {
            parsed
        }
        None => return None,
    };
    Some((body, basis, status))
}

// Empty suffix or one explicit URL marker, optionally one request-id marker.
// Values are syntax-checked only and never retained. Any other tail, including
// another JSON envelope, makes the complete error unrecognized.
fn allowed_http_suffix(suffix: &str) -> bool {
    let suffix = suffix.trim();
    if suffix.is_empty() {
        return true;
    }
    let Some(rest) = suffix.strip_prefix(", url: ") else {
        return false;
    };
    let (url, request_id) = match rest.split_once(", request id: ") {
        Some((url, id)) => (url, Some(id)),
        None => (rest, None),
    };
    let address = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"));
    address.is_some_and(|address| !address.is_empty())
        && url.len() <= 2048
        && url.bytes().all(|byte| {
            byte.is_ascii_graphic() && !matches!(byte, b'"' | b'\\' | b'{' | b'}' | b'<' | b'>')
        })
        && request_id.is_none_or(|id| {
            !id.is_empty()
                && id.len() <= 128
                && id
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        })
}

fn code(value: Option<&str>) -> (Option<&'static str>, Class) {
    let (name, class) = match value {
        Some("invalid_function_parameters") => {
            ("invalid_function_parameters", Class::ToolDefinition)
        }
        Some("invalid_tool_schema") => ("invalid_tool_schema", Class::ToolDefinition),
        Some("invalid_json_schema") => ("invalid_json_schema", Class::OutputSchema),
        Some("model_not_found") => ("model_not_found", Class::Model),
        Some("unsupported_model") => ("unsupported_model", Class::Model),
        Some("invalid_api_key") => ("invalid_api_key", Class::Authentication),
        Some("authentication_error") => ("authentication_error", Class::Authentication),
        Some("permission_denied") => ("permission_denied", Class::Authorization),
        Some("insufficient_permissions") => ("insufficient_permissions", Class::Authorization),
        Some("rate_limit_exceeded") => ("rate_limit_exceeded", Class::RateLimit),
        Some("insufficient_quota") => ("insufficient_quota", Class::ResourceLimit),
        Some("context_length_exceeded") => ("context_length_exceeded", Class::ResourceLimit),
        Some("server_error") => ("server_error", Class::Server),
        Some("internal_server_error") => ("internal_server_error", Class::Server),
        Some("request_timeout") => ("request_timeout", Class::Transport),
        Some("connection_error") => ("connection_error", Class::Transport),
        Some("invalid_request_error") => ("invalid_request_error", Class::Request),
        Some("unsupported_parameter") => ("unsupported_parameter", Class::Request),
        Some("unsupported_value") => ("unsupported_value", Class::Request),
        Some("missing_required_parameter") => ("missing_required_parameter", Class::Request),
        Some("invalid_value") => ("invalid_value", Class::Request),
        _ => return (None, Class::Unknown),
    };
    (Some(name), class)
}

fn error_type(value: Option<&str>) -> (Option<&'static str>, Class) {
    let (name, class) = match value {
        Some("invalid_request_error") => ("invalid_request_error", Class::Request),
        Some("authentication_error") => ("authentication_error", Class::Authentication),
        Some("permission_error") => ("permission_error", Class::Authorization),
        Some("rate_limit_error") => ("rate_limit_error", Class::RateLimit),
        Some("server_error") => ("server_error", Class::Server),
        Some("api_error") => ("api_error", Class::Server),
        Some("timeout_error") => ("timeout_error", Class::Transport),
        Some("connection_error") => ("connection_error", Class::Transport),
        _ => return (None, Class::Unknown),
    };
    (Some(name), class)
}

fn parameter(value: Option<&str>) -> (Option<&'static str>, Class) {
    let Some(value) = value.filter(|value| value.len() <= 256) else {
        return (None, Class::Unknown);
    };
    for (name, class) in [
        ("tools", Class::ToolDefinition),
        ("response_format", Class::OutputSchema),
        ("text.format", Class::OutputSchema),
        ("output_schema", Class::OutputSchema),
        ("model", Class::Model),
        ("reasoning", Class::Request),
        ("service_tier", Class::Request),
        ("input", Class::Request),
    ] {
        if value == name
            || value
                .strip_prefix(name)
                .is_some_and(|rest| rest.starts_with('.') || rest.starts_with('['))
        {
            return (Some(name), class);
        }
    }
    (None, Class::Unknown)
}

fn message_signature(text: &str) -> Option<(&'static str, Class)> {
    // Anchored, explicit error formulations only. A word occurring inside a
    // quoted document, header or arbitrary explanation does not classify it.
    for (prefix, signature, class) in [
        (
            "Invalid schema for function ",
            "invalid_function_schema",
            Class::ToolDefinition,
        ),
        (
            "Invalid schema for tool ",
            "invalid_tool_schema",
            Class::ToolDefinition,
        ),
        (
            "Invalid schema for response_format ",
            "invalid_response_format_schema",
            Class::OutputSchema,
        ),
        (
            "Invalid schema for response format ",
            "invalid_response_format_schema",
            Class::OutputSchema,
        ),
        (
            "Invalid schema for output_schema ",
            "invalid_output_schema",
            Class::OutputSchema,
        ),
        (
            "Unsupported tool type:",
            "unsupported_tool_type",
            Class::ToolDefinition,
        ),
        (
            "Incorrect API key provided:",
            "incorrect_api_key",
            Class::Authentication,
        ),
        (
            "Your authentication token has expired",
            "authentication_token_expired",
            Class::Authentication,
        ),
        (
            "error sending request for url",
            "request_transport_error",
            Class::Transport,
        ),
        (
            "stream disconnected before completion",
            "response_stream_disconnected",
            Class::Transport,
        ),
        (
            "The server had an error processing your request",
            "server_processing_error",
            Class::Server,
        ),
    ] {
        if text
            .get(..prefix.len())
            .is_some_and(|head| head.eq_ignore_ascii_case(prefix))
        {
            return Some((signature, class));
        }
    }
    None
}

fn native_class(info: Option<CodexErrorInfo>) -> Class {
    use CodexErrorInfo as Info;
    match info {
        Some(Info::Unauthorized) => Class::Authentication,
        Some(
            Info::HttpConnectionFailed
            | Info::ResponseStreamConnectionFailed
            | Info::ResponseStreamDisconnected
            | Info::ResponseTooManyFailedAttempts,
        ) => Class::Transport,
        Some(Info::ServerOverloaded | Info::InternalServerError) => Class::Server,
        Some(Info::RateLimitExceeded) => Class::RateLimit,
        Some(
            Info::ContextWindowExceeded | Info::SessionBudgetExceeded | Info::UsageLimitExceeded,
        ) => Class::ResourceLimit,
        Some(Info::BadRequest) => Class::Request,
        Some(Info::CyberPolicy | Info::MisalignmentPolicyViolation) => Class::Policy,
        Some(Info::SandboxError) => Class::Sandbox,
        _ => Class::Unknown,
    }
}

fn merge(classes: impl IntoIterator<Item = Class>) -> (Class, bool) {
    let mut result = Class::Unknown;
    for class in classes {
        if class == Class::Unknown || class == result {
            continue;
        }
        if result == Class::Unknown {
            result = class;
            continue;
        }
        // Invalid request is a broad container for rejected tool/schema/model
        // parameters; a more specific explicit field may refine only that class.
        if result == Class::Request
            && matches!(
                class,
                Class::ToolDefinition | Class::OutputSchema | Class::Model
            )
        {
            result = class;
            continue;
        }
        if class == Class::Request
            && matches!(
                result,
                Class::ToolDefinition | Class::OutputSchema | Class::Model
            )
        {
            continue;
        }
        return (Class::Unknown, true);
    }
    (result, false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn diagnostic(message: Value, details: Value) -> Value {
        project(
            &json!({"message":message,"additionalDetails":details}),
            Some(CodexErrorInfo::Other),
            true,
        )
        .unwrap()
    }

    #[test]
    fn closed_structured_codes_distinguish_actionable_error_families() {
        for (code, parameter, expected) in [
            (
                "invalid_function_parameters",
                "tools[0].parameters.private-field",
                "tool_definition",
            ),
            (
                "invalid_json_schema",
                "text.format.schema.private-field",
                "output_schema",
            ),
            ("model_not_found", "model", "model"),
            ("invalid_api_key", "private-parameter", "authentication"),
            ("permission_denied", "private-parameter", "authorization"),
            ("connection_error", "private-parameter", "transport"),
            ("server_error", "private-parameter", "server"),
            ("rate_limit_exceeded", "private-parameter", "rate_limit"),
        ] {
            let body = json!({"error":{"code":code,"param":parameter,
                "message":"private-provider-content","secret":"private-credential"},
                "headers":{"authorization":"private-header"}});
            let result = diagnostic(json!(body.to_string()), Value::Null);
            assert_eq!(result["classification"], expected);
            assert_eq!(result["evidence"][0]["code"], code);
            assert!(!serde_json::to_string(&result).unwrap().contains("private-"));
        }
    }

    #[test]
    fn only_anchored_explicit_signatures_classify_plain_error_messages() {
        for (message, expected) in [
            (
                "Invalid schema for function 'private-name': private-schema",
                "tool_definition",
            ),
            (
                "Invalid schema for response_format 'private-name': private-schema",
                "output_schema",
            ),
            ("Incorrect API key provided: private-key", "authentication"),
            (
                "error sending request for url (https://private-url)",
                "transport",
            ),
            (
                "The server had an error processing your request: private-detail",
                "server",
            ),
        ] {
            let result = diagnostic(json!(message), Value::Null);
            assert_eq!(result["classification"], expected);
            assert_eq!(result["evidence"][0]["basis"], "anchored_message_signature");
            assert!(!result.to_string().contains("private-"));
            let quoted = diagnostic(
                json!(format!("Provider quoted a document: {message}")),
                Value::Null,
            );
            assert_eq!(quoted["classification"], "unknown");
            assert_eq!(quoted["evidence"], json!([]));
        }
    }

    #[test]
    fn unknown_codes_embedded_json_and_duplicate_keys_do_not_guess_a_cause() {
        for message in [
            r#"{"error":{"code":"private-unknown-code","param":"private-unknown-param","message":"private-message"}}"#,
            r#"Document data: {"error":{"code":"invalid_api_key"}}"#,
            r#"{"error":{"code":"invalid_api_key","code":"server_error"}}"#,
            r#"unexpected status 400 Bad Request: {"error":{"code":"server_error","code":"invalid_api_key"}}, url: private-url"#,
            "private-message referring to invalid_json_schema and model_not_found",
            r#"unexpected status 400 Bad Request: {"error":{"code":"invalid_api_key"}} {"error":{"code":"server_error"}}"#,
            r#"unexpected status 400 Bad Request: {"error":{"code":"invalid_api_key"}}, url: https://private-host {"error":{"code":"server_error"}}"#,
            r#"unexpected status 400 Bad Request: {"error":{"code":"invalid_api_key"}}, headers: private-header"#,
            r#"unexpected status 400 Bad Request: {"error":{"code":"invalid_api_key"}}, url: https://private-host, request id: {"code":"server_error"}"#,
        ] {
            let result = diagnostic(json!(message), Value::Null);
            assert_eq!(result["classification"], "unknown");
            assert!(!result.to_string().contains("private-"));
        }
    }

    #[test]
    fn conflicts_are_visible_and_generic_bad_request_can_be_refined() {
        let message = json!(
            json!({"error":{"code":"invalid_json_schema","param":"text.format"}}).to_string()
        );
        let result = project(
            &json!({"message":message}),
            Some(CodexErrorInfo::BadRequest),
            true,
        )
        .unwrap();
        assert_eq!(result["classification"], "output_schema");
        assert_eq!(result["conflicting_evidence"], false);
        let result = project(
            &json!({"message":message}),
            Some(CodexErrorInfo::Unauthorized),
            true,
        )
        .unwrap();
        assert_eq!(result["classification"], "unknown");
        assert_eq!(result["conflicting_evidence"], true);
        let conflict = json!({"error":{"code":"invalid_api_key","param":"tools[0]"}});
        let result = diagnostic(json!(conflict.to_string()), Value::Null);
        assert_eq!(result["classification"], "unknown");
        assert_eq!(result["conflicting_evidence"], true);
    }

    #[test]
    fn bounded_fingerprints_do_not_scan_hash_or_truncate_oversized_fields() {
        let message = format!(
            "Invalid schema for function {}",
            "private-tail".repeat(FIELD_LIMIT)
        );
        let result = diagnostic(
            json!(message),
            json!({"code":"invalid_api_key","private":"credential"}),
        );
        assert_eq!(result["classification"], "unknown");
        assert_eq!(result["message"]["inspected"], false);
        assert_eq!(result["message"]["bytes"], message.len());
        assert!(result["message"]["sha256"].is_null());
        assert_eq!(result["additional_details"]["shape"], "object");
        assert!(!result.to_string().contains("private"));
        assert!(serde_json::to_vec(&result).unwrap().len() < 2048);
        let result = diagnostic(json!("private-known-length"), Value::Null);
        assert_eq!(result["message"]["bytes"], 20);
        assert_eq!(
            result["message"]["sha256"],
            format!("{:x}", Sha256::digest(b"private-known-length"))
        );
    }

    #[test]
    fn additional_details_share_the_projection_and_keep_a_closed_output_bound() {
        let details = json!({"error":{"code":"invalid_json_schema","type":"invalid_request_error",
            "param":"text.format.schema.private-property","message":"Invalid schema for response_format 'private-name': private-data"},
            "request_body":"private-body","headers":{"authorization":"private-token"}});
        let result = project(
            &json!({"message":details.to_string(),"additionalDetails":details.to_string()}),
            Some(CodexErrorInfo::BadRequest),
            true,
        )
        .unwrap();
        assert_eq!(result["classification"], "output_schema");
        assert_eq!(result["evidence"].as_array().unwrap().len(), 3);
        assert_eq!(result["evidence"][2]["source"], "additionalDetails");
        let bytes = serde_json::to_vec(&result).unwrap();
        assert!(bytes.len() < 2048);
        assert!(!String::from_utf8(bytes).unwrap().contains("private-"));
    }
}
