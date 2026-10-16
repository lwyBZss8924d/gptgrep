//! Emit source-bound profile metadata without starting a model or reading credentials.
use anyhow::{Result, ensure};
use gptgrep_host::{
    NativeAgentRuntimeProfile, OcrRuntimeProfile,
    parse_agent_driver::{DocumentNativeExecution, document_native_execution_bindings},
};
use serde::Deserialize;
use serde_json::json;
use std::io::{Read, Write};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    source_revision: String,
    protocol_schema_sha256: String,
    codex_binary_sha256: String,
    document: DocumentNativeExecution,
    ocr: NativeAgentRuntimeProfile,
    document_max_seconds: u64,
}

fn main() -> Result<()> {
    let mut input = Vec::new();
    std::io::stdin().take(1_048_577).read_to_end(&mut input)?;
    ensure!(input.len() <= 1_048_576, "profile_input_limit");
    let request: Request = serde_json::from_slice(&input)?;
    ensure!(
        (1..=86_400).contains(&request.document_max_seconds),
        "profile_duration"
    );
    let bindings = document_native_execution_bindings(&request.document)?;
    let ocr = OcrRuntimeProfile::with_native_agent(
        request.source_revision.clone(),
        request.protocol_schema_sha256.clone(),
        request.codex_binary_sha256.clone(),
        request.ocr,
    )?;
    let document_profile = json!({
        "schema":"gptgrep.parse-agent.document-runtime-profile.v1",
        "source_revision":request.source_revision,
        "protocol_schema_sha256":request.protocol_schema_sha256,
        "codex_binary_sha256":request.codex_binary_sha256,
        "descriptor_sha256":bindings.descriptor_sha256,
        "permissions_config_sha256":bindings.permissions_config_sha256,
        "permissions_template_sha256":bindings.permissions_template_sha256,
        "tools_sha256":bindings.tools_sha256,
        "instructions_sha256":bindings.instructions_sha256,
        "standalone_app_server":true,"native_document_guard":null,
        "max_seconds":request.document_max_seconds,
    });
    let output = json!({
        "schema":"gptgrep.native-execution-profiles.v1",
        "document_bindings":bindings,
        "document_model_descriptor_json":serde_json::to_string(&bindings.model_descriptor)?,
        "document_permissions_config_json":serde_json::to_string(&bindings.permissions_config)?,
        "document_runtime_profile":document_profile,
        "ocr_runtime_profile_sha256":ocr.sha256()?,
        "ocr_runtime_profile":ocr,
    });
    serde_json::to_writer(std::io::stdout().lock(), &output)?;
    std::io::stdout().write_all(b"\n")?;
    Ok(())
}
