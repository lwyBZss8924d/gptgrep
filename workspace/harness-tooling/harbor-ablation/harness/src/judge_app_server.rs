//! One-turn app-server transport with explicitly bound v2 execution admission.
//! Tool rejection observes an event; it does not attest prevention or remote cleanup.
use super::*;
use serde::de::{DeserializeSeed, MapAccess, SeqAccess, Visitor};
use serde_json::json;
use std::{cell::Cell, collections::BTreeMap, fmt};

const ID_MAX: usize = 256;
const JSON_DEPTH_MAX: usize = 32;
const JSON_NODES_MAX: usize = 4096;
const ITEMS_MAX: usize = 64;
// Separate from the incoming event cap: JSON escaping of the original bounded prompt/schema.
const OUTGOING_MAX: usize = 6 * (REQUEST_MAX + ARTIFACT_MAX) + 16_384;
const MAPPING: &str = "official-0.161-dto-with-bounded-native-metadata-source-only";

// Match only complete internal literals. Never publish formatted error chains or provider text.
const STATIC_FAILURE_CODE_CAP: usize = 64;
const STATIC_FAILURE_CODES: &[&str] = &[
    "app_agent_phase",
    "app_approval_or_input_observed",
    "app_async_or_question_rejected",
    "app_automatic_metadata_limit",
    "app_automatic_metadata_server_request",
    "app_configured_profile_mismatch",
    "app_deadline",
    "app_delta_shape",
    "app_duplicate_final",
    "app_duplicate_or_changed_item",
    "app_duplicate_response_id",
    "app_duplicate_turn",
    "app_early_limit",
    "app_empty_frame",
    "app_event_count",
    "app_exit_failure",
    "app_field_shape",
    "app_final_limit",
    "app_final_missing",
    "app_final_shape",
    "app_frame_limit",
    "app_id_shape",
    "app_incomplete_items",
    "app_incomplete_terminal",
    "app_initialize_shape",
    "app_invalid_automatic_metadata",
    "app_invalid_json",
    "app_item_delta_kind",
    "app_item_limit",
    "app_item_not_started",
    "app_item_order",
    "app_lifecycle_publication_unknown_no_retry",
    "app_method_shape",
    "app_native_chatgpt_auth_required",
    "app_native_id_mismatch",
    "app_notification_order",
    "app_notification_rejected",
    "app_object_shape",
    "app_outgoing_count",
    "app_outgoing_limit",
    "app_pipe",
    "app_pipe_failure",
    "app_post_terminal_activity",
    "app_prompt_limit",
    "app_publication_unknown_no_retry",
    "app_reason_limit",
    "app_receipt_allowance",
    "app_receipt_limit_no_retry",
    "app_recipe_mismatch",
    "app_remote_control_not_disabled",
    "app_rpc_envelope",
    "app_rpc_error",
    "app_rpc_id",
    "app_rpc_order",
    "app_server_request_rejected",
    "app_stale_or_deadline_no_retry",
    "app_stderr_limit",
    "app_stdin_closed",
    "app_stdin_failure",
    "app_stdout_limit",
    "app_summary_final_mismatch",
    "app_summary_mismatch",
    "app_summary_shape",
    "app_thread_missing",
    "app_thread_started_order",
    "app_thread_status_rejected",
    "app_tool_or_item_rejected",
    "app_turn_ack_status",
    "app_turn_missing",
    "app_turn_not_started",
    "app_turn_status",
    "app_turn_terminal_status",
    "app_unavailable_no_retry",
    "app_unsent_request",
    "app_usage_shape",
    "app_user_input_mismatch",
    "app_user_input_shape",
];
fn static_failure_code(error: &anyhow::Error) -> &'static str {
    let message = error.to_string();
    STATIC_FAILURE_CODES
        .iter()
        .copied()
        .find(|code| *code == message)
        .unwrap_or("unknown_internal_failure")
}
// Selected official notification literals, including refused diagnostic events.
// Membership supplies a label only; it never admits an event or changes ordering.
const NOTIFICATION_CATEGORIES: &[&str] = &[
    "error",
    "thread/started",
    "thread/status/changed",
    "thread/tokenUsage/updated",
    "turn/started",
    "turn/completed",
    "rawResponse/completed",
    "item/started",
    "item/completed",
    "item/agentMessage/delta",
    "item/reasoning/textDelta",
    "item/reasoning/summaryTextDelta",
    "item/reasoning/summaryPartAdded",
    "account/updated",
    "account/rateLimits/updated",
    "remoteControl/status/changed",
    "configWarning",
    "warning",
    "model/rerouted",
    "model/verification",
    "turn/plan/updated",
    "turn/diff/updated",
    "serverRequest/resolved",
];
fn notification_category(method: Option<&str>) -> &'static str {
    NOTIFICATION_CATEGORIES
        .iter()
        .copied()
        .find(|known| Some(*known) == method)
        .unwrap_or("unknown_notification")
}
fn native_error_category(info: &Value) -> &'static str {
    const CODES: &[&str] = &[
        "contextWindowExceeded",
        "sessionBudgetExceeded",
        "usageLimitExceeded",
        "rateLimitExceeded",
        "flexUnavailable",
        "serverOverloaded",
        "cyberPolicy",
        "misalignmentPolicyViolation",
        "tooManyDenials",
        "internalServerError",
        "unauthorized",
        "badRequest",
        "threadRollbackFailed",
        "sandboxError",
        "other",
    ];
    if let Some(code) = CODES
        .iter()
        .copied()
        .find(|code| Some(*code) == info.as_str())
    {
        return code;
    }
    if let Some(o) = info.as_object().filter(|o| o.len() == 1) {
        for code in [
            "httpConnectionFailed",
            "responseStreamConnectionFailed",
            "responseStreamDisconnected",
            "responseTooManyFailedAttempts",
        ] {
            if o.get(code).and_then(Value::as_object).is_some_and(|p| {
                p.keys().all(|key| key == "httpStatusCode")
                    && p.get("httpStatusCode").is_none_or(|value| {
                        value.is_null() || value.as_u64().is_some_and(|n| u16::try_from(n).is_ok())
                    })
            }) {
                return code;
            }
        }
    }
    "unknown_native_error"
}

struct Seed<'a> {
    nodes: &'a Cell<usize>,
    depth: usize,
}
impl<'de> DeserializeSeed<'de> for Seed<'_> {
    type Value = Value;
    fn deserialize<D: serde::Deserializer<'de>>(
        self,
        d: D,
    ) -> std::result::Result<Value, D::Error> {
        use serde::de::Error;
        if self.depth > JSON_DEPTH_MAX || self.nodes.get() == 0 {
            return Err(D::Error::custom("bounded_json"));
        }
        self.nodes.set(self.nodes.get() - 1);
        d.deserialize_any(self)
    }
}
impl<'de> Visitor<'de> for Seed<'_> {
    type Value = Value;
    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("bounded unique-key JSON")
    }
    fn visit_unit<E: serde::de::Error>(self) -> std::result::Result<Value, E> {
        Ok(Value::Null)
    }
    fn visit_bool<E: serde::de::Error>(self, v: bool) -> std::result::Result<Value, E> {
        Ok(v.into())
    }
    fn visit_i64<E: serde::de::Error>(self, v: i64) -> std::result::Result<Value, E> {
        Ok(v.into())
    }
    fn visit_u64<E: serde::de::Error>(self, v: u64) -> std::result::Result<Value, E> {
        Ok(v.into())
    }
    fn visit_f64<E: serde::de::Error>(self, _: f64) -> std::result::Result<Value, E> {
        // This broker subset needs only exact integers, booleans, strings and containers.
        // Refuse float-rounded integer overflow and decimal telemetry rather than laundering it.
        Err(E::custom("noninteger_native_json"))
    }
    fn visit_str<E: serde::de::Error>(self, v: &str) -> std::result::Result<Value, E> {
        Ok(v.into())
    }
    fn visit_string<E: serde::de::Error>(self, v: String) -> std::result::Result<Value, E> {
        Ok(v.into())
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut a: A) -> std::result::Result<Value, A::Error> {
        let mut result = Vec::new();
        while let Some(v) = a.next_element_seed(Seed {
            nodes: self.nodes,
            depth: self.depth + 1,
        })? {
            result.push(v);
        }
        Ok(Value::Array(result))
    }
    fn visit_map<A: MapAccess<'de>>(self, mut a: A) -> std::result::Result<Value, A::Error> {
        use serde::de::Error;
        let mut result = serde_json::Map::new();
        while let Some(k) = a.next_key::<String>()? {
            // serde_json arbitrary_precision can represent an out-of-range number through
            // this private pseudo-map. Reject it before it can be mistaken for an object.
            if k == "$serde_json::private::Number" {
                return Err(A::Error::custom("noninteger_native_json"));
            }
            if result.contains_key(&k) {
                return Err(A::Error::custom("duplicate_json_key"));
            }
            let v = a.next_value_seed(Seed {
                nodes: self.nodes,
                depth: self.depth + 1,
            })?;
            result.insert(k, v);
        }
        Ok(Value::Object(result))
    }
}
fn decode(bytes: &[u8], cap: usize) -> Result<Value> {
    ensure!(!bytes.is_empty() && bytes.len() <= cap, "app_frame_limit");
    let nodes = Cell::new(JSON_NODES_MAX);
    let mut d = serde_json::Deserializer::from_slice(bytes);
    let value = Seed {
        nodes: &nodes,
        depth: 0,
    }
    .deserialize(&mut d)
    .map_err(|_| anyhow::anyhow!("app_invalid_json"))?;
    d.end().map_err(|_| anyhow::anyhow!("app_invalid_json"))?;
    Ok(value)
}
fn opaque(v: &Value) -> Result<&str> {
    let s = v.as_str().context("app_id_shape")?;
    ensure!(
        !s.is_empty() && s.len() <= ID_MAX && !s.chars().any(char::is_control),
        "app_id_shape"
    );
    Ok(s)
}
fn object(v: &Value) -> Result<&serde_json::Map<String, Value>> {
    v.as_object().context("app_object_shape")
}
fn field<'a>(v: &'a Value, name: &str) -> Result<&'a Value> {
    object(v)?.get(name).context("app_field_shape")
}
fn same_id(v: &Value, expected: &str) -> Result<()> {
    ensure!(opaque(v)? == expected, "app_native_id_mismatch");
    Ok(())
}

fn fixed_argv(a: &Admission) -> Result<Vec<String>> {
    let mut argv = vec![
        "-c".into(),
        format!(
            "sqlite_home={}",
            toml_path(&a.control.environment.sqlite_home)?
        ),
        "-c".into(),
        format!("log_dir={}", toml_path(&a.control.environment.log_dir)?),
        "-c".into(),
        "web_search=\"disabled\"".into(),
    ];
    // Fixed official configuration projection; no OCR instructions/model descriptor.
    for key in [
        "shell_tool",
        "view_image",
        "sleep_tool",
        "code_mode_only",
        "code_mode_prewarm",
        "memories",
        "hooks",
        "multi_agent_v2",
        "apps",
        "plugins",
        "tool_suggest",
        "image_generation",
        "goals",
        "token_budget",
        "browser_use",
        "browser_use_external",
        "computer_use",
        "in_app_browser",
        "artifact",
        "realtime_conversation",
        "send_message_to_user_async",
        "current_time_reminder",
        "deferred_executor",
        "request_permissions_tool",
    ] {
        argv.extend(["-c".into(), format!("features.{key}=false")]);
    }
    for value in [
        "agents.enabled=false",
        "features.code_mode.enabled=false",
        "features.code_mode_host.enabled=false",
        "orchestrator.mcp.enabled=false",
        "cloud.skills.enabled=false",
        "tools.update_plan.enabled=false",
        "tools.experimental_request_user_input.enabled=false",
        "analytics.enabled=false",
        "project_doc_max_bytes=0",
        "notify=[]",
        "mcp_servers={}",
        "approval_policy=\"never\"",
        "sandbox_mode=\"read-only\"",
        "model_reasoning_effort=\"max\"",
        "service_tier=\"fast\"",
        "features.fast_mode=true",
    ] {
        argv.extend(["-c".into(), value.into()]);
    }
    argv.extend([
        "app-server".into(),
        "--listen".into(),
        "stdio://".into(),
        "--strict-config".into(),
    ]);
    Ok(argv)
}
fn initialize() -> Value {
    json!({"id":1,"method":"initialize","params":{"clientInfo":{"name":"gptgrep-original-judge","version":"1"},"capabilities":{"experimentalApi":false}}})
}
fn thread_options(cwd: &str) -> Value {
    json!({"model":"gpt-6-luna","serviceTier":"fast","approvalPolicy":"never","sandbox":"read-only","ephemeral":true,"cwd":cwd})
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Turn<'a> {
    thread_id: &'a str,
    input: [Text<'a>; 1],
    model: &'static str,
    effort: &'static str,
    service_tier: &'static str,
    output_schema: &'a Value,
}
#[derive(Serialize)]
struct Text<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    text: &'a str,
    text_elements: [(); 0],
}
#[derive(Serialize)]
struct TurnRpc<'a> {
    id: &'static str,
    method: &'static str,
    params: Turn<'a>,
}
fn turn_frame(thread_id: &str, prompt: &str, schema: &Value) -> Result<Vec<u8>> {
    let mut bytes = json_bytes(
        &TurnRpc {
            id: "judge-turn",
            method: "turn/start",
            params: Turn {
                thread_id,
                input: [Text {
                    kind: "text",
                    text: prompt,
                    text_elements: [],
                }],
                model: "gpt-6-luna",
                effort: "max",
                service_tier: "fast",
                output_schema: schema,
            },
        },
        OUTGOING_MAX - 1,
    )?;
    bytes.push(b'\n');
    Ok(bytes)
}
fn frame(v: &Value) -> Result<Vec<u8>> {
    let mut b = json_bytes(v, OUTGOING_MAX - 1)?;
    b.push(b'\n');
    Ok(b)
}
pub(super) fn recipe(a: &Admission) -> Result<String> {
    Ok(digest(&json_bytes(
        &json!({
            "schema_version":"gptgrep.judge.app-server-recipe.v1", "mapping_source":MAPPING,
            "argv":fixed_argv(a)?, "initialize":initialize(),
            "thread_start_options":thread_options("<owned-private-staging>"),
            "turn_start":{"id":"judge-turn","method":"turn/start","native_thread_id":"bound_from_response_only",
                "model":"gpt-6-luna","effort":"max","serviceTier":"fast",
                "prompt_sha256":digest(a.prepared.decoded.formatted.prompt.as_bytes()),
                "output_schema_sha256":a.prepared.decoded.formatted.verdict_schema_sha256},
            "raw_event_opt_in":false,"no_retry":true
        }),
        CONTROL_MAX,
    )?))
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct TokenUsage {
    total_tokens: i64,
    input_tokens: i64,
    cached_input_tokens: i64,
    // Upstream defaults omission to zero. Preserve omission rather than inventing telemetry.
    #[serde(default)]
    cache_write_input_tokens: Option<i64>,
    output_tokens: i64,
    reasoning_output_tokens: i64,
}
impl TokenUsage {
    fn parse(v: &Value) -> Result<Self> {
        let o = object(v)?;
        ensure!(
            o.keys().all(|k| matches!(
                k.as_str(),
                "totalTokens"
                    | "inputTokens"
                    | "cachedInputTokens"
                    | "cacheWriteInputTokens"
                    | "outputTokens"
                    | "reasoningOutputTokens"
            )),
            "app_usage_shape"
        );
        let count = |name| -> Result<i64> { field(v, name)?.as_i64().context("app_usage_shape") };
        let u = Self {
            total_tokens: count("totalTokens")?,
            input_tokens: count("inputTokens")?,
            cached_input_tokens: count("cachedInputTokens")?,
            cache_write_input_tokens: match o.get("cacheWriteInputTokens") {
                None | Some(Value::Null) => None,
                Some(v) => Some(v.as_i64().context("app_usage_shape")?),
            },
            output_tokens: count("outputTokens")?,
            reasoning_output_tokens: count("reasoningOutputTokens")?,
        };
        ensure!(
            [
                u.total_tokens,
                u.input_tokens,
                u.cached_input_tokens,
                u.output_tokens,
                u.reasoning_output_tokens
            ]
            .iter()
            .all(|n| *n >= 0)
                && u.cache_write_input_tokens.is_none_or(|n| n >= 0),
            "app_usage_shape"
        );
        Ok(u)
    }
}
#[derive(Serialize)]
struct NativeUsage {
    total: TokenUsage,
    last: TokenUsage,
}
#[derive(Serialize)]
struct ConfiguredAck {
    model: String,
    model_provider: String,
    service_tier: Option<String>,
}
#[derive(Serialize)]
struct OutboundFrame {
    method: &'static str,
    prepared_bytes: usize,
    prepared_sha256: String,
    bytes_written: usize,
    full_write_observed: bool,
}
#[derive(Serialize)]
struct ProtocolObservations {
    mapping_source: &'static str,
    configured_ack: Option<ConfiguredAck>,
    thread_usage: Option<NativeUsage>,
    raw_response_usage: Option<TokenUsage>,
    rpc_error_code: Option<i64>,
    failing_notification_category: Option<&'static str>,
    native_error_category: Option<&'static str>,
    native_error_will_retry: Option<bool>,
    events_observed: usize,
    automatic_metadata_notifications: usize,
    tools_observed_and_rejected: bool,
    tool_prevention_qualified: bool,
    served_profile_qualified: bool,
    outbound_frames: Vec<OutboundFrame>,
}
#[derive(Serialize)]
struct AppReport {
    #[serde(flatten)]
    base: Report,
    internal_failure_code: Option<&'static str>,
    protocol: ProtocolObservations,
}
#[derive(Serialize)]
struct AppOutcome {
    schema_version: &'static str,
    publication_sha256: Option<String>,
    publication_status: &'static str,
    output_may_exist: bool,
    no_retry: bool,
    report: AppReport,
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum Stage {
    SendInit,
    Init,
    SendThread,
    Thread,
    ThreadStarted,
    SendTurn,
    Turn,
    Running,
    Complete,
}
#[derive(Default)]
struct AutomaticMetadata {
    notifications: usize,
}

impl AutomaticMetadata {
    fn observe(&mut self, message: &Value) -> Result<bool> {
        let method = message["method"].as_str();
        if !matches!(
            method,
            Some("account/updated" | "account/rateLimits/updated" | "remoteControl/status/changed")
        ) {
            return Ok(false);
        }
        ensure!(
            message.get("id").is_none(),
            "app_automatic_metadata_server_request"
        );
        ensure!(self.notifications < 32, "app_automatic_metadata_limit");
        let params = &message["params"];
        match method {
            Some("account/updated") => {
                metadata_object(params, &["authMode"], &["authMode", "planType"])?;
                ensure!(
                    params["authMode"] == "chatgpt",
                    "app_native_chatgpt_auth_required"
                );
                ensure!(
                    nullable_enum(&params["planType"], PLAN_TYPES),
                    "app_invalid_automatic_metadata"
                );
            }
            Some("remoteControl/status/changed") => {
                metadata_object(
                    params,
                    &["installationId", "serverName", "status"],
                    &["installationId", "serverName", "status", "environmentId"],
                )?;
                ensure!(
                    params["installationId"].is_string()
                        && params["serverName"].is_string()
                        && nullable_string(&params["environmentId"]),
                    "app_invalid_automatic_metadata"
                );
                ensure!(
                    params["status"] == "disabled",
                    "app_remote_control_not_disabled"
                );
            }
            Some("account/rateLimits/updated") => validate_rate_metadata(params)?,
            _ => unreachable!(),
        }
        self.notifications += 1;
        Ok(true)
    }
}

// Public enum values from the installed official AccountUpdatedNotification and
// AccountRateLimitsUpdatedNotification DTOs. These are syntax, not entitlement.
const PLAN_TYPES: &[&str] = &[
    "free",
    "go",
    "plus",
    "pro",
    "prolite",
    "promax",
    "team",
    "self_serve_business_prolite",
    "self_serve_business_usage_based",
    "business",
    "ent26",
    "enterprise_cbp_automation",
    "enterprise_cbp_usage_based",
    "enterprise",
    "edu",
    "edu_plus",
    "edu_pro",
    "unknown",
];
const RATE_REACHED_TYPES: &[&str] = &[
    "rate_limit_reached",
    "workspace_owner_credits_depleted",
    "workspace_member_credits_depleted",
    "workspace_owner_usage_limit_reached",
    "workspace_member_usage_limit_reached",
];

fn metadata_object<'a>(
    value: &'a Value,
    required: &[&str],
    allowed: &[&str],
) -> Result<&'a serde_json::Map<String, Value>> {
    let object = value
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("app_invalid_automatic_metadata"))?;
    ensure!(
        required.iter().all(|key| object.contains_key(*key))
            && object.keys().all(|key| allowed.contains(&key.as_str())),
        "app_invalid_automatic_metadata"
    );
    Ok(object)
}
fn nullable_string(value: &Value) -> bool {
    value.is_null() || value.is_string()
}
fn nullable_enum(value: &Value, allowed: &[&str]) -> bool {
    value.is_null() || value.as_str().is_some_and(|name| allowed.contains(&name))
}
fn metadata_i32(value: &Value) -> bool {
    value.as_i64().is_some_and(|n| i32::try_from(n).is_ok())
}
fn validate_rate_metadata(params: &Value) -> Result<()> {
    metadata_object(params, &["rateLimits"], &["rateLimits"])?;
    let rates = &params["rateLimits"];
    metadata_object(
        rates,
        &[],
        &[
            "credits",
            "individualLimit",
            "limitId",
            "limitName",
            "normalModelSlug",
            "planType",
            "primary",
            "rateLimitReachedType",
            "secondary",
            "spendControlReached",
        ],
    )?;
    ensure!(
        ["limitId", "limitName", "normalModelSlug"]
            .iter()
            .all(|key| nullable_string(&rates[*key]))
            && nullable_enum(&rates["planType"], PLAN_TYPES)
            && nullable_enum(&rates["rateLimitReachedType"], RATE_REACHED_TYPES)
            && (rates["spendControlReached"].is_null()
                || rates["spendControlReached"].is_boolean()),
        "app_invalid_automatic_metadata"
    );
    for name in ["primary", "secondary"] {
        let window = &rates[name];
        if !window.is_null() {
            metadata_object(
                window,
                &["usedPercent"],
                &["usedPercent", "resetsAt", "windowDurationMins"],
            )?;
            ensure!(
                metadata_i32(&window["usedPercent"])
                    && ["resetsAt", "windowDurationMins"]
                        .iter()
                        .all(|key| window[*key].is_null() || window[*key].is_i64()),
                "app_invalid_automatic_metadata"
            );
        }
    }
    let credits = &rates["credits"];
    if !credits.is_null() {
        metadata_object(
            credits,
            &["hasCredits", "unlimited"],
            &["hasCredits", "unlimited", "balance"],
        )?;
        ensure!(
            credits["hasCredits"].is_boolean()
                && credits["unlimited"].is_boolean()
                && nullable_string(&credits["balance"]),
            "app_invalid_automatic_metadata"
        );
    }
    let limit = &rates["individualLimit"];
    if !limit.is_null() {
        metadata_object(
            limit,
            &["limit", "used", "remainingPercent", "resetsAt"],
            &["limit", "used", "remainingPercent", "resetsAt"],
        )?;
        ensure!(
            limit["limit"].is_string()
                && limit["used"].is_string()
                && metadata_i32(&limit["remainingPercent"])
                && limit["resetsAt"].is_i64(),
            "app_invalid_automatic_metadata"
        );
    }
    Ok(())
}

struct Session<'a> {
    prompt: &'a str,
    schema: &'a Value,
    cwd: &'a str,
    stage: Stage,
    outgoing: Vec<u8>,
    sent: usize,
    // At most two frames share one pending buffer (initialized then thread/start).
    ranges: Vec<(usize, usize, usize)>,
    thread_id: Option<String>,
    turn_id: Option<String>,
    started: bool,
    items: BTreeMap<String, (&'static str, bool)>,
    final_item: Option<String>,
    final_text: Option<String>,
    response_id: Option<String>,
    early: Vec<Vec<u8>>,
    early_bytes: usize,
    protocol: ProtocolObservations,
    metadata: AutomaticMetadata,
}
impl<'a> Session<'a> {
    fn new(prompt: &'a str, schema: &'a Value, cwd: &'a str) -> Result<Self> {
        ensure!(prompt.len() <= REQUEST_MAX, "app_prompt_limit");
        schema_valid(schema)?;
        let initial = frame(&initialize())?;
        let initial_fact = OutboundFrame {
            method: "initialize",
            prepared_bytes: initial.len(),
            prepared_sha256: digest(&initial),
            bytes_written: 0,
            full_write_observed: false,
        };
        let initial_range = (0, initial.len(), 0);
        Ok(Self {
            prompt,
            schema,
            cwd,
            stage: Stage::SendInit,
            outgoing: initial,
            sent: 0,
            ranges: vec![initial_range],
            thread_id: None,
            turn_id: None,
            started: false,
            items: BTreeMap::new(),
            final_item: None,
            final_text: None,
            response_id: None,
            early: Vec::new(),
            early_bytes: 0,
            metadata: AutomaticMetadata::default(),
            protocol: ProtocolObservations {
                mapping_source: MAPPING,
                configured_ack: None,
                thread_usage: None,
                raw_response_usage: None,
                rpc_error_code: None,
                failing_notification_category: None,
                native_error_category: None,
                native_error_will_retry: None,
                events_observed: 0,
                automatic_metadata_notifications: 0,
                tools_observed_and_rejected: false,
                tool_prevention_qualified: false,
                served_profile_qualified: false,
                outbound_frames: vec![initial_fact],
            },
        })
    }
    fn queue(&mut self, parts: &[(&'static str, &[u8])], stage: Stage) -> Result<()> {
        ensure!(self.sent == self.outgoing.len(), "app_unsent_request");
        ensure!(
            !parts.is_empty()
                && parts.len() <= 2
                && self.protocol.outbound_frames.len() + parts.len() <= 4,
            "app_outgoing_count"
        );
        let size = parts.iter().try_fold(0usize, |n, (_, b)| {
            n.checked_add(b.len()).context("app_outgoing_limit")
        })?;
        ensure!(size <= OUTGOING_MAX, "app_outgoing_limit");
        let mut bytes = Vec::new();
        bytes.try_reserve_exact(size)?;
        self.ranges.clear();
        for (method, part) in parts {
            let start = bytes.len();
            bytes.extend_from_slice(part);
            let index = self.protocol.outbound_frames.len();
            self.protocol.outbound_frames.push(OutboundFrame {
                method,
                prepared_bytes: part.len(),
                prepared_sha256: digest(part),
                bytes_written: 0,
                full_write_observed: false,
            });
            self.ranges.push((start, bytes.len(), index));
        }
        self.outgoing = bytes;
        self.sent = 0;
        self.stage = stage;
        Ok(())
    }
    fn written(&mut self) {
        for (start, end, index) in &self.ranges {
            let fact = &mut self.protocol.outbound_frames[*index];
            fact.bytes_written = self.sent.saturating_sub(*start).min(end - start);
            fact.full_write_observed = fact.bytes_written == fact.prepared_bytes;
        }
        if self.sent == self.outgoing.len() {
            self.stage = match self.stage {
                Stage::SendInit => Stage::Init,
                Stage::SendThread => Stage::Thread,
                Stage::SendTurn => Stage::Turn,
                s => s,
            };
        }
    }
    fn line(&mut self, bytes: &[u8], limits: &Limits) -> Result<()> {
        ensure!(
            self.protocol.events_observed < limits.event_count,
            "app_event_count"
        );
        self.protocol.events_observed += 1;
        let v = decode(bytes, limits.event_bytes)?;
        let o = object(&v)?;
        ensure!(!o.contains_key("jsonrpc"), "app_rpc_envelope");
        if o.contains_key("method") {
            if o.contains_key("id") {
                self.protocol.tools_observed_and_rejected = true;
                bail!("app_server_request_rejected");
            }
            // Official notifications optionally add emittedAtMs: i64; discard its value.
            ensure!(
                o.contains_key("params")
                    && o.keys()
                        .all(|key| { matches!(key.as_str(), "method" | "params" | "emittedAtMs") })
                    && o.get("emittedAtMs")
                        .is_none_or(|value| value.as_i64().is_some()),
                "app_rpc_envelope"
            );
            if self.metadata.observe(&v)? {
                self.protocol.automatic_metadata_notifications = self.metadata.notifications;
                return Ok(());
            }
            if self.stage == Stage::Turn {
                // Real turn events may precede turn/start's RPC response. Hold bounded original
                // bytes, not provisional IDs; replay after binding the returned native turn ID.
                ensure!(
                    self.early.len() < ITEMS_MAX
                        && self
                            .early_bytes
                            .checked_add(bytes.len())
                            .is_some_and(|n| n <= limits.event_bytes),
                    "app_early_limit"
                );
                self.early_bytes += bytes.len();
                self.early.push(bytes.to_vec());
                return Ok(());
            }
            return self.notification(&v, limits);
        }
        ensure!(
            o.len() == 2
                && o.contains_key("id")
                && (o.contains_key("result") || o.contains_key("error")),
            "app_rpc_envelope"
        );
        let expected = match self.stage {
            Stage::Init => json!(1),
            Stage::Thread => json!("judge-thread"),
            Stage::Turn => json!("judge-turn"),
            _ => bail!("app_rpc_order"),
        };
        // Native RequestId is String|i64. String "1" is not integer 1.
        ensure!(v["id"] == expected, "app_rpc_id");
        if let Some(error) = o.get("error") {
            self.protocol.rpc_error_code = object(error)?.get("code").and_then(Value::as_i64);
            bail!("app_rpc_error"); // Never retain message/data/home/prompt/error strings.
        }
        let r = field(&v, "result")?;
        match self.stage {
            Stage::Init => {
                object(r)?;
                for name in ["userAgent", "codexHome", "platformFamily", "platformOs"] {
                    let s = field(r, name)?.as_str().context("app_initialize_shape")?;
                    ensure!(s.len() <= 4096, "app_initialize_shape");
                }
                let bytes = frame(&json!({"method":"initialized"}))?;
                let thread = frame(
                    &json!({"id":"judge-thread","method":"thread/start","params":thread_options(self.cwd)}),
                )?;
                ensure!(
                    bytes
                        .len()
                        .checked_add(thread.len())
                        .is_some_and(|n| n <= OUTGOING_MAX),
                    "app_outgoing_limit"
                );
                self.queue(
                    &[("initialized", &bytes), ("thread/start", &thread)],
                    Stage::SendThread,
                )?;
            }
            Stage::Thread => {
                let id = opaque(field(field(r, "thread")?, "id")?)?.to_owned();
                let model = opaque(field(r, "model")?)?.to_owned();
                let model_provider = opaque(field(r, "modelProvider")?)?.to_owned();
                let service_tier = match object(r)?.get("serviceTier") {
                    Some(Value::Null) | None => None,
                    Some(v) => Some(opaque(v)?.to_owned()),
                };
                ensure!(
                    model == "gpt-6-luna"
                        && matches!(service_tier.as_deref(), Some("fast" | "priority")),
                    "app_configured_profile_mismatch"
                );
                self.protocol.configured_ack = Some(ConfiguredAck {
                    model,
                    model_provider,
                    service_tier,
                });
                self.thread_id = Some(id);
                self.stage = Stage::ThreadStarted;
            }
            Stage::Turn => {
                let turn = field(r, "turn")?;
                self.turn_id = Some(opaque(field(turn, "id")?)?.to_owned());
                ensure!(
                    field(turn, "status")?.as_str() == Some("inProgress"),
                    "app_turn_ack_status"
                );
                self.stage = Stage::Running;
                let early = std::mem::take(&mut self.early);
                self.early_bytes = 0;
                for b in early {
                    let event = decode(&b, limits.event_bytes)?;
                    self.notification(&event, limits)?;
                }
            }
            _ => bail!("app_rpc_order"),
        }
        Ok(())
    }
    fn correlated(&self, p: &Value) -> Result<()> {
        same_id(
            field(p, "threadId")?,
            self.thread_id.as_deref().context("app_thread_missing")?,
        )?;
        same_id(
            field(p, "turnId")?,
            self.turn_id.as_deref().context("app_turn_missing")?,
        )
    }
    fn item_kind(&mut self, item: &Value) -> Result<&'static str> {
        match field(item, "type")?.as_str() {
            Some("agentMessage") => Ok("agentMessage"),
            Some("reasoning") => Ok("reasoning"),
            Some("userMessage") => Ok("userMessage"),
            _ => {
                self.protocol.tools_observed_and_rejected = true;
                bail!("app_tool_or_item_rejected")
            }
        }
    }
    fn notification(&mut self, v: &Value, limits: &Limits) -> Result<()> {
        let result = self.checked_notification(v, limits);
        if result.is_err() {
            self.protocol.failing_notification_category =
                Some(notification_category(v["method"].as_str()));
            if v["method"] == "error" {
                self.protocol.native_error_category = Some(native_error_category(
                    &v["params"]["error"]["codexErrorInfo"],
                ));
                self.protocol.native_error_will_retry = v["params"]["willRetry"].as_bool();
            }
        }
        result
    }
    fn checked_notification(&mut self, v: &Value, limits: &Limits) -> Result<()> {
        let method = field(v, "method")?.as_str().context("app_method_shape")?;
        let p = field(v, "params")?;
        if method == "thread/status/changed" {
            ensure!(
                matches!(
                    self.stage,
                    Stage::ThreadStarted
                        | Stage::SendTurn
                        | Stage::Turn
                        | Stage::Running
                        | Stage::Complete
                ),
                "app_notification_order"
            );
            same_id(
                field(p, "threadId")?,
                self.thread_id.as_deref().context("app_thread_missing")?,
            )?;
            let status = field(p, "status")?;
            if self.stage == Stage::Complete {
                ensure!(
                    field(status, "type")?.as_str() == Some("idle"),
                    "app_post_terminal_activity"
                );
            }
            match field(status, "type")?.as_str() {
                Some("idle") => {}
                Some("active") => ensure!(
                    field(status, "activeFlags")?
                        .as_array()
                        .is_some_and(Vec::is_empty),
                    "app_approval_or_input_observed"
                ),
                _ => bail!("app_thread_status_rejected"),
            }
            return Ok(());
        }
        if method == "thread/started" {
            ensure!(
                self.stage == Stage::ThreadStarted,
                "app_thread_started_order"
            );
            same_id(
                field(field(p, "thread")?, "id")?,
                self.thread_id.as_deref().context("app_thread_missing")?,
            )?;
            let turn = turn_frame(self.thread_id.as_deref().unwrap(), self.prompt, self.schema)?;
            self.queue(&[("turn/start", &turn)], Stage::SendTurn)?;
            return Ok(());
        }
        ensure!(self.stage == Stage::Running, "app_notification_order");
        if method == "turn/started" {
            ensure!(!self.started, "app_duplicate_turn");
            same_id(
                field(p, "threadId")?,
                self.thread_id.as_deref().context("app_thread_missing")?,
            )?;
            let t = field(p, "turn")?;
            same_id(
                field(t, "id")?,
                self.turn_id.as_deref().context("app_turn_missing")?,
            )?;
            ensure!(
                field(t, "status")?.as_str() == Some("inProgress"),
                "app_turn_status"
            );
            self.started = true;
            return Ok(());
        }
        ensure!(self.started, "app_turn_not_started");
        match method {
            "thread/tokenUsage/updated" => {
                same_id(
                    field(p, "threadId")?,
                    self.thread_id.as_deref().context("app_thread_missing")?,
                )?;
                same_id(
                    field(p, "turnId")?,
                    self.turn_id.as_deref().context("app_turn_missing")?,
                )?;
                let u = field(p, "tokenUsage")?;
                // Commit both projections only after complete validation. Do not sum total+last.
                let next = NativeUsage {
                    total: TokenUsage::parse(field(u, "total")?)?,
                    last: TokenUsage::parse(field(u, "last")?)?,
                };
                self.protocol.thread_usage = Some(next);
            }
            "rawResponse/completed" => {
                self.correlated(p)?;
                ensure!(self.response_id.is_none(), "app_duplicate_response_id");
                let id = opaque(field(p, "responseId")?)?.to_owned();
                let usage = match object(p)?.get("usage") {
                    Some(Value::Null) | None => None,
                    Some(v) => Some(TokenUsage::parse(v)?),
                };
                self.response_id = Some(id);
                self.protocol.raw_response_usage = usage;
                // Optional usageMetadata.amount has no proven currency unit. Do not expose as USD.
            }
            "item/started" | "item/completed" => {
                self.correlated(p)?;
                let item = field(p, "item")?;
                let id = opaque(field(item, "id")?)?.to_owned();
                let kind = self.item_kind(item)?;
                if method == "item/started" {
                    ensure!(
                        self.items.len() < ITEMS_MAX && !self.items.contains_key(&id),
                        "app_item_order"
                    );
                    self.items.insert(id, (kind, false));
                } else {
                    let entry = self.items.get_mut(&id).context("app_item_not_started")?;
                    ensure!(entry.0 == kind && !entry.1, "app_duplicate_or_changed_item");
                    entry.1 = true;
                    if kind == "agentMessage" {
                        ensure!(
                            object(item)?.get("delivery").is_none_or(Value::is_null)
                                && object(item)?.get("questions").is_none_or(
                                    |v| v.is_null() || v.as_array().is_some_and(Vec::is_empty)
                                ),
                            "app_async_or_question_rejected"
                        );
                        let phase = object(item)?.get("phase").and_then(Value::as_str);
                        ensure!(
                            matches!(phase, Some("commentary") | Some("final_answer")),
                            "app_agent_phase"
                        );
                        let text = field(item, "text")?.as_str().context("app_final_shape")?;
                        ensure!(text.len() <= limits.final_bytes, "app_final_limit");
                        if phase == Some("final_answer") {
                            ensure!(self.final_text.is_none(), "app_duplicate_final");
                            // Validate the exact original schema before retaining the final text.
                            let value = decode(text.as_bytes(), limits.final_bytes)?;
                            let verdict: Verdict = serde_json::from_value(value)
                                .map_err(|_| anyhow::anyhow!("app_final_shape"))?;
                            ensure!(verdict.reason.len() <= STDERR_MAX, "app_reason_limit");
                            self.final_item = Some(id);
                            self.final_text = Some(text.to_owned());
                        }
                    } else if kind == "userMessage" {
                        let content = field(item, "content")?
                            .as_array()
                            .context("app_user_input_shape")?;
                        ensure!(
                            content.len() == 1
                                && field(&content[0], "type")?.as_str() == Some("text")
                                && field(&content[0], "text")?.as_str() == Some(self.prompt),
                            "app_user_input_mismatch"
                        );
                    }
                }
            }
            "item/agentMessage/delta"
            | "item/reasoning/textDelta"
            | "item/reasoning/summaryTextDelta"
            | "item/reasoning/summaryPartAdded" => {
                self.correlated(p)?;
                let id = opaque(field(p, "itemId")?)?;
                let (kind, done) = self.items.get(id).context("app_item_not_started")?;
                ensure!(
                    !done
                        && ((method == "item/agentMessage/delta" && *kind == "agentMessage")
                            || (method != "item/agentMessage/delta" && *kind == "reasoning")),
                    "app_item_delta_kind"
                );
                if method != "item/reasoning/summaryPartAdded" {
                    field(p, "delta")?.as_str().context("app_delta_shape")?;
                }
                // Delta bytes are bounded by the admitted frame, never retained as a second history.
            }
            "turn/completed" => {
                same_id(
                    field(p, "threadId")?,
                    self.thread_id.as_deref().context("app_thread_missing")?,
                )?;
                let t = field(p, "turn")?;
                same_id(
                    field(t, "id")?,
                    self.turn_id.as_deref().context("app_turn_missing")?,
                )?;
                ensure!(
                    field(t, "status")?.as_str() == Some("completed")
                        && object(t)?.get("error").is_none_or(Value::is_null),
                    "app_turn_terminal_status"
                );
                ensure!(
                    self.final_text.is_some() && self.items.values().all(|(_, done)| *done),
                    "app_incomplete_items"
                );
                // Turn summaries may be notLoaded and are not proof of the complete item history.
                // If items are supplied, they cannot contradict observed IDs/types/final bytes.
                if let Some(value) = object(t)?.get("items") {
                    let items = value.as_array().context("app_summary_shape")?;
                    ensure!(items.len() <= ITEMS_MAX, "app_item_limit");
                    let mut seen = std::collections::BTreeSet::new();
                    for item in items {
                        let id = opaque(field(item, "id")?)?;
                        let kind = self.item_kind(item)?;
                        ensure!(
                            seen.insert(id) && self.items.get(id) == Some(&(kind, true)),
                            "app_summary_mismatch"
                        );
                        if Some(id) == self.final_item.as_deref() {
                            ensure!(
                                field(item, "text")?.as_str() == self.final_text.as_deref(),
                                "app_summary_final_mismatch"
                            );
                        }
                    }
                }
                self.stage = Stage::Complete;
            }
            _ => bail!("app_notification_rejected"),
        }
        Ok(())
    }
}

pub(super) fn dispatch(path: &Path, sha: &str, run: bool) -> Result<String> {
    let a = Admission::load(path, sha)?;
    let actual_recipe = recipe(&a)?;
    reserve_receipt(&a)?;
    if a.phase.is_some() {
        ensure!(
            actual_recipe == a.control.runtime.recipe_sha256,
            "app_recipe_mismatch"
        );
    }
    if !run {
        return String::from_utf8(json_bytes(&json!({
            "schema_version":"gptgrep.judge.app-server-preflight.v1","status":"preflight_no_effects",
            "control_sha256":a.control_snapshot.sha256,"request_manifest_sha256":a.prepared.manifest.sha256,
            "request_sha256":a.prepared.saved.sha256,"fixed_recipe_sha256":actual_recipe,
            "declared_recipe_sha256":a.control.runtime.recipe_sha256,
            "recipe_matches":actual_recipe==a.control.runtime.recipe_sha256,
            "mapping_source":MAPPING,"execution_policy":"unavailable","judge_status":"unavailable",
            "actual_judge_model":null,"actual_turn_id":null,"actual_response_id":null,"actual_usage":null,
            "actual_cost":null,"actual_provider_calls":null,"reward_emitted":false,"effect_count":0,
            "original_formatter_reconstruction":"not_replayed_by_app_server_preflight"
        }),a.control.limits.receipt_bytes)?).map_err(Into::into);
    }
    ensure!(
        actual_recipe == a.control.runtime.recipe_sha256,
        "app_recipe_mismatch"
    );
    let permit = authorize_app_server(&a)?;
    let outcome = execute(&a, permit)?;
    String::from_utf8(json_bytes(&outcome, a.control.limits.receipt_bytes)?).map_err(Into::into)
}

fn cleanup_until(child: &mut std::process::Child, deadline: Instant) -> bool {
    // SAFETY: this actor created the exact child's group. Do not renew the run deadline.
    let _ = unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL) };
    let _ = child.kill();
    loop {
        if child.try_wait().ok().flatten().is_some() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(2));
    }
}

struct Pipes<W, R, E> {
    input: Option<W>,
    stdout: R,
    stderr: E,
}
struct PumpFacts {
    stdout_bytes: usize,
    stderr_bytes: usize,
    stderr_hash: Sha256,
    stdout_eof: bool,
    stderr_eof: bool,
    leader_exit: Option<bool>,
}
impl PumpFacts {
    fn new() -> Self {
        Self {
            stdout_bytes: 0,
            stderr_bytes: 0,
            stderr_hash: Sha256::new(),
            stdout_eof: false,
            stderr_eof: false,
            leader_exit: None,
        }
    }
}
// Generic over borrowed or owned nonblocking streams. No process, home, clock or policy is
// discovered here; the actor supplies an already fixed deadline and owned-leader status probe.
fn pump<W: Write, R: Read, E: Read>(
    io: &mut Pipes<W, R, E>,
    session: &mut Session<'_>,
    limits: &Limits,
    deadline: Instant,
    exit: &mut dyn FnMut() -> Result<Option<bool>>,
    facts: &mut PumpFacts,
    original_clock: Option<&gptgrep_jev::RunClockAnchor>,
) -> Result<()> {
    let mut scratch = [0u8; 4096];
    let mut line = Vec::new();
    line.try_reserve_exact(limits.event_bytes)?;
    loop {
        if let Some(clock) = original_clock {
            clock.remaining()?;
        }
        ensure!(Instant::now() < deadline, "app_deadline");
        if session.stage == Stage::Complete {
            io.input = None;
        }
        if session.sent < session.outgoing.len() {
            let stdin = io.input.as_mut().context("app_stdin_closed")?;
            let end = session.outgoing.len().min(session.sent + scratch.len());
            match stdin.write(&session.outgoing[session.sent..end]) {
                Ok(0) => bail!("app_stdin_failure"),
                Ok(n) => {
                    ensure!(n <= end - session.sent, "app_stdin_failure");
                    session.sent += n;
                }
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                    ) => {}
                Err(_) => bail!("app_stdin_failure"),
            }
            session.written();
        }
        for (stream, eof, is_stdout) in [
            (&mut io.stdout as &mut dyn Read, &mut facts.stdout_eof, true),
            (
                &mut io.stderr as &mut dyn Read,
                &mut facts.stderr_eof,
                false,
            ),
        ] {
            if *eof {
                continue;
            }
            match stream.read(&mut scratch) {
                Ok(0) => *eof = true,
                Ok(n) if is_stdout => {
                    facts.stdout_bytes = facts
                        .stdout_bytes
                        .checked_add(n)
                        .context("app_stdout_limit")?;
                    ensure!(
                        facts.stdout_bytes <= limits.stdout_bytes,
                        "app_stdout_limit"
                    );
                    for b in &scratch[..n] {
                        if *b == b'\n' {
                            ensure!(!line.is_empty(), "app_empty_frame");
                            session.line(&line, limits)?;
                            line.clear();
                        } else {
                            ensure!(line.len() < limits.event_bytes, "app_frame_limit");
                            line.push(*b);
                        }
                    }
                }
                Ok(n) => {
                    facts.stderr_bytes = facts
                        .stderr_bytes
                        .checked_add(n)
                        .context("app_stderr_limit")?;
                    facts.stderr_hash.update(&scratch[..n]);
                    ensure!(
                        facts.stderr_bytes <= limits.stderr_bytes,
                        "app_stderr_limit"
                    );
                }
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                    ) => {}
                Err(_) => bail!("app_pipe_failure"),
            }
        }
        if facts.leader_exit.is_none() {
            facts.leader_exit = exit()?;
        }
        if facts.stdout_eof {
            ensure!(
                line.is_empty() && session.stage == Stage::Complete,
                "app_incomplete_terminal"
            );
        }
        if facts.stdout_eof && facts.stderr_eof && facts.leader_exit.is_some() {
            ensure!(facts.leader_exit == Some(true), "app_exit_failure");
            return Ok(());
        }
        thread::sleep(Duration::from_millis(2));
    }
}
fn report(a: &Admission, origin: &'static str) -> AppReport {
    let mut base = initial_report(a, origin);
    base.schema_version = "gptgrep.judge.app-server-observation.v1";
    AppReport {
        base,
        internal_failure_code: None,
        protocol: ProtocolObservations {
            mapping_source: MAPPING,
            configured_ack: None,
            thread_usage: None,
            raw_response_usage: None,
            rpc_error_code: None,
            failing_notification_category: None,
            native_error_category: None,
            native_error_will_retry: None,
            events_observed: 0,
            automatic_metadata_notifications: 0,
            tools_observed_and_rejected: false,
            tool_prevention_qualified: false,
            served_profile_qualified: false,
            outbound_frames: Vec::new(),
        },
    }
}
fn reserve_receipt(a: &Admission) -> Result<()> {
    let mut worst = serde_json::to_value(report(a, "source_only_no_effects"))?;
    worst["status"] = json!("typed_sequence_unpublished_unavailable");
    worst["failure_code"] = json!("app_publication_unknown_no_retry");
    worst["internal_failure_code"] = json!("x".repeat(STATIC_FAILURE_CODE_CAP));
    worst["raw_final_sha256"] = json!("f".repeat(64));
    for name in [
        "transport_thread_id",
        "native_turn_id",
        "transport_final_item_id",
        "actual_response_id",
    ] {
        worst["observations"][name] = json!("\\".repeat(ID_MAX));
    }
    worst["observations"]["stdout_bytes_observed"] = json!(usize::MAX);
    worst["observations"]["stderr_bytes_observed"] = json!(usize::MAX);
    worst["protocol"]["events_observed"] = json!(usize::MAX);
    worst["protocol"]["rpc_error_code"] = json!(i64::MIN);
    worst["protocol"]["failing_notification_category"] = json!("x".repeat(STATIC_FAILURE_CODE_CAP));
    worst["protocol"]["native_error_category"] = json!("x".repeat(STATIC_FAILURE_CODE_CAP));
    worst["protocol"]["native_error_will_retry"] = json!(false);
    worst["protocol"]["outbound_frames"]=json!(["initialize","initialized","thread/start","turn/start"].map(|method|json!({"method":method,"prepared_bytes":usize::MAX,"prepared_sha256":"f".repeat(64),"bytes_written":usize::MAX,"full_write_observed":false})));
    worst["protocol"]["configured_ack"] = json!({"model":"\\".repeat(ID_MAX),"model_provider":"\\".repeat(ID_MAX),"service_tier":"\\".repeat(ID_MAX)});
    let usage = json!({"totalTokens":i64::MAX,"inputTokens":i64::MAX,"cachedInputTokens":i64::MAX,"cacheWriteInputTokens":i64::MAX,"outputTokens":i64::MAX,"reasoningOutputTokens":i64::MAX});
    worst["protocol"]["thread_usage"] = json!({"total":usage,"last":usage});
    worst["protocol"]["raw_response_usage"] = usage;
    // A low receipt budget may intentionally reject before claim. Retaining verdict is optional
    // for a low budget, but unknown observations must fit even without a typed verdict.
    worst["verdict"] = if a.phase.is_some() {
        json!({"equivalent":true,"abstained":true,"reason":"\u{1}".repeat(a.control.limits.final_bytes)})
    } else {
        Value::Null
    };
    worst["protocol"]["automatic_metadata_notifications"] = json!(32);
    outcome_allowance(&worst, a.control.limits.receipt_bytes).context("app_receipt_allowance")
}
fn execute(a: &Admission, permit: ExecutionPermit) -> Result<AppOutcome> {
    use std::os::unix::process::CommandExt;
    ensure!(
        permit.control_sha256 == a.control_snapshot.sha256,
        "native_execution_policy_unavailable"
    );
    ensure!(
        recipe(a)? == a.control.runtime.recipe_sha256,
        "app_recipe_mismatch"
    );
    reserve_receipt(a)?;
    permit.check()?;
    a.prepared.reconstruct(
        a.control.output_root.parent().context("output_parent")?,
        permit.deadline,
    )?;
    let claim_root = consume(a, &permit)?;
    let claim_id = identity(&claim_root)?;
    let mut report = report(a, permit.observation_origin);
    let mut final_bytes = None;
    let result: Result<()> = (|| {
        permit.check()?;
        let staging = private_tempdir(
            a.control.output_root.parent().context("output_parent")?,
            ".judge-app-server-",
        )?;
        let cwd = staging.path().to_str().context("native_path_contract")?;
        let mut session = Session::new(
            &a.prepared.decoded.formatted.prompt,
            &a.prepared.decoded.formatted.verdict_schema,
            cwd,
        )?;
        let fixed = fixed_argv(a)?;
        // The opaque test-only permit substitutes generic Python fake argv. No caller route.
        let argv = if permit.argv.is_empty() {
            &fixed
        } else {
            &permit.argv
        };
        a.fresh()?;
        permit.check()?;
        let mut command = Command::new(&a.binary.path);
        command
            .args(argv)
            .env_clear()
            .env("PATH", &a.control.environment.path)
            .env("HOME", &a.control.environment.home)
            .env("CODEX_HOME", &a.control.environment.codex_home)
            .env("CODEX_INTERNAL_APP_SERVER_REMOTE_CONTROL_DISABLED", "1")
            .env("TMPDIR", &a.control.environment.tmpdir)
            .env("LANG", "C.UTF-8")
            .env("LC_ALL", "C.UTF-8")
            .current_dir(staging.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // SAFETY: only async-signal-safe process-group and umask operations in the child.
        unsafe {
            command.pre_exec(|| {
                if libc::setpgid(0, 0) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                libc::umask(0o077);
                Ok(())
            });
        }
        permit.check()?;
        let mut child = command.spawn()?;
        report.base.observations.wrapper_spawned = true;
        let mut facts = PumpFacts::new();
        let process_result: Result<()> = (|| {
            if a.phase.is_some() {
                permit.check()?;
                let mut event = private_file(
                    &claim_root,
                    &format!("{}.started.json", a.control.invocation_id),
                )?;
                event.write_all(&json_bytes(&json!({"schema_version":"gptgrep.judge.native-lifecycle.v1","state":"owned_child_spawn_observed","control_sha256":a.control_snapshot.sha256,"actual_provider_calls":null}),RECEIPT_CAP)?)?;
                event.sync_all()?;
                claim_root.sync_all()?;
            }
            let mut io = Pipes {
                input: Some(child.stdin.take().context("app_pipe")?),
                stdout: child.stdout.take().context("app_pipe")?,
                stderr: child.stderr.take().context("app_pipe")?,
            };
            nonblocking(io.input.as_ref().unwrap().as_raw_fd())?;
            nonblocking(io.stdout.as_raw_fd())?;
            nonblocking(io.stderr.as_raw_fd())?;
            pump(
                &mut io,
                &mut session,
                &a.control.limits,
                permit.deadline,
                &mut || Ok(child.try_wait()?.map(|s| s.success())),
                &mut facts,
                permit.original_clock.as_ref(),
            )
        })();
        report.base.observations.wrapper_leader_reaped = facts.leader_exit.is_some();
        report.base.observations.pipe_eof_observed = facts.stdout_eof && facts.stderr_eof;
        report.base.observations.stdout_bytes_observed = facts.stdout_bytes;
        report.base.observations.stderr_bytes_observed = facts.stderr_bytes;
        report.base.observations.stderr_sha256 = format!("{:x}", facts.stderr_hash.finalize());
        report.base.observations.terminal_sequence_observed = session.stage == Stage::Complete;
        report.base.observations.transport_thread_id = session.thread_id.take();
        report.base.observations.native_turn_id = session.turn_id.take();
        report.base.observations.transport_final_item_id = session.final_item.take();
        report.base.observations.actual_response_id = session.response_id.take();
        report.protocol = session.protocol;
        if process_result.is_err() {
            report.base.observations.wrapper_leader_reaped |=
                cleanup_until(&mut child, permit.deadline);
        }
        process_result?;
        let raw = session
            .final_text
            .context("app_final_missing")?
            .into_bytes();
        let verdict: Verdict = serde_json::from_value(decode(&raw, a.control.limits.final_bytes)?)
            .map_err(|_| anyhow::anyhow!("app_final_shape"))?;
        report.base.raw_final_sha256 = Some(digest(&raw));
        report.base.verdict = Some(verdict);
        final_bytes = Some(raw);
        a.fresh()?;
        ensure!(
            identity(&open_held(&a.control.invocation_root, true)?)? == claim_id,
            "native_claim_root_changed"
        );
        ensure!(Instant::now() < permit.deadline, "app_deadline");
        Ok(())
    })();
    report.base.status = if result.is_ok() {
        if permit.observation_origin == "native_app_server_v2" {
            "completed_actual"
        } else {
            "typed_structural_unqualified"
        }
    } else if report.base.observations.wrapper_spawned {
        "completion_unknown_quarantined"
    } else {
        "consumed_failed_no_spawn_observed"
    };
    if let Err(error) = &result {
        report.internal_failure_code = Some(static_failure_code(error));
        report.base.failure_code = Some("app_unavailable_no_retry");
        report.base.verdict = None;
        final_bytes = None;
    }
    if a.phase.is_some() {
        let recorded = (|| -> Result<()> {
            if result.is_ok() {
                permit.check()?;
            }
            let mut event = private_file(
                &claim_root,
                &format!("{}.finished.json", a.control.invocation_id),
            )?;
            event.write_all(&json_bytes(&json!({"schema_version":"gptgrep.judge.native-lifecycle.v1","state":if result.is_ok(){"owned_terminal_sequence_and_pipe_eof_observed"}else{"completion_unknown_no_retry"},"control_sha256":a.control_snapshot.sha256,"owned_leader_reaped":report.base.observations.wrapper_leader_reaped,"actual_provider_calls":null}),RECEIPT_CAP)?)?;
            event.sync_all()?;
            claim_root.sync_all()?;
            Ok(())
        })();
        if recorded.is_err() {
            report.base.status = "completion_unknown_quarantined";
            report.base.failure_code = Some("app_lifecycle_publication_unknown_no_retry");
            report.base.verdict = None;
            final_bytes = None;
        }
    }
    if report.base.verdict.is_some() && permit.observation_origin == "native_app_server_v2" {
        report.base.judge_status = "completed";
        if let Some(verdict) = &mut report.base.verdict {
            verdict.reason.clear();
        }
    }
    let receipt = match outcome_allowance(&report, a.control.limits.receipt_bytes)
        .and_then(|()| json_bytes(&report, a.control.limits.receipt_bytes))
    {
        Ok(b) => b,
        Err(_) => {
            report.base.verdict = None;
            final_bytes = None;
            report.base.status = "completion_unknown_quarantined";
            report.base.failure_code = Some("app_receipt_limit_no_retry");
            json_bytes(&report, a.control.limits.receipt_bytes)?
        }
    };
    let mut outcome = AppOutcome {
        schema_version: "gptgrep.judge.app-server-result.v1",
        publication_sha256: None,
        publication_status: "not_published",
        output_may_exist: false,
        no_retry: true,
        report,
    };
    let fresh = || -> Result<()> {
        permit.check()?;
        a.fresh()?;
        ensure!(
            identity(&open_held(&a.control.invocation_root, true)?)? == claim_id,
            "native_claim_root_changed"
        );
        permit.check()?;
        Ok(())
    };
    if fresh().is_err() {
        outcome.report.base.status = "completion_unknown_quarantined";
        outcome.report.base.failure_code = Some("app_stale_or_deadline_no_retry");
        outcome.report.base.verdict = None;
        return Ok(outcome);
    }
    let mut snapshots = a.prepared.snapshots().to_vec();
    snapshots.push(&a.control_snapshot);
    if let Some(phase) = &a.phase {
        snapshots.push(&phase.phase);
        snapshots.push(&phase.origin);
    }
    let mut files = vec![("app-server-observation.json", receipt.as_slice())];
    if let Some(bytes) = final_bytes.as_ref() {
        files.push(("native-final.json", bytes.as_slice()));
    }
    match publish_checked(&a.control.output_root, &files, &snapshots, fresh) {
        Ok(sha) => {
            outcome.publication_sha256 = Some(sha);
            outcome.publication_status = "published";
        }
        Err(e) => {
            outcome.output_may_exist = e
                .chain()
                .any(|e| e.to_string() == "publication_sync_unknown_no_retry_output_may_exist");
            outcome.publication_status = if outcome.output_may_exist {
                "unknown_after_rename_no_retry"
            } else {
                "failed_before_rename_no_retry"
            };
            outcome.report.base.verdict = None;
            outcome.report.base.failure_code = Some("app_publication_unknown_no_retry");
        }
    }
    Ok(outcome)
}

#[cfg(test)]
#[path = "judge_app_server_tests.rs"]
mod tests;
