use anyhow::{Result, anyhow};
use serde_json::{Value, json};
use std::{
    fs::{File, OpenOptions},
    io::Write,
    path::Path,
};
pub(crate) struct Trace {
    file: File,
    bytes: usize,
    events: usize,
    closed: bool,
}
impl Trace {
    pub fn open(path: Option<&Path>) -> Result<Option<Self>> {
        let Some(path) = path else { return Ok(None) };
        if let Some(parent) = path.parent().filter(|path| !path.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)
                .map_err(|_| anyhow!("host_trace_directory_unavailable"))?;
        }
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options
            .open(path)
            .map_err(|_| anyhow!("host_trace_file_unavailable"))?;
        Ok(Some(Self {
            file,
            bytes: 0,
            events: 0,
            closed: false,
        }))
    }
    pub fn record(&mut self, direction: &str, message: &Value) -> Result<()> {
        if self.closed {
            return Ok(());
        }
        let params = &message["params"];
        let mut tools = vec![];
        if direction == "send" && message["method"] == "thread/start" {
            for spec in params["dynamicTools"].as_array().into_iter().flatten() {
                if spec["type"] == "namespace" {
                    for function in spec["tools"].as_array().into_iter().flatten() {
                        if let (Some(namespace), Some(name)) =
                            (label(&spec["name"]), label(&function["name"]))
                        {
                            tools.push(format!("{namespace}.{name}"));
                        }
                    }
                } else if let Some(name) = label(&spec["name"]) {
                    tools.push(name);
                }
            }
        }
        let id = message
            .get("id")
            .filter(|id| id.is_u64() || label(id).is_some())
            .cloned();
        let error = match message["method"].as_str() {
            Some("error") => Some(crate::codex_error::ParsedError::parse(&params["error"])),
            Some("turn/completed") if params["turn"]["status"] == "failed" => Some(
                crate::codex_error::ParsedError::parse(&params["turn"]["error"]),
            ),
            _ => None,
        };
        let entry = json!({
            "direction":direction,"method":label(&message["method"]),"id":id,
            "thread_id":label(&params["threadId"]).or_else(||label(&message["result"]["thread"]["id"])),
            "turn_id":label(&params["turnId"]).or_else(||label(&params["turn"]["id"])).or_else(||label(&message["result"]["turn"]["id"])),
            "tool":label(&params["tool"]).or_else(||label(&params["item"]["tool"])),
            "namespace":label(&params["namespace"]).or_else(||label(&params["item"]["namespace"])),
            "advertised_tools":tools,
            "codex_error_info":error.as_ref().and_then(|error|error.info),
            "will_retry":if message["method"] == "error" { params["willRetry"].as_bool() } else { None },
            "error_diagnostics":error.as_ref().and_then(|error|error.diagnostics.as_ref()),
            "http_status_code":error.and_then(|error|error.http_status_code)
        });
        let mut line = serde_json::to_vec(&entry).map_err(|_| anyhow!("host_trace_encoding"))?;
        line.push(b'\n');
        if self.events >= 256 || self.bytes + line.len() > 64 * 1024 - 32 {
            self.closed = true;
            self.file
                .write_all(b"{\"trace_truncated\":true}\n")
                .map_err(|_| anyhow!("host_trace_write_failed"))?;
        } else {
            self.file
                .write_all(&line)
                .map_err(|_| anyhow!("host_trace_write_failed"))?;
            self.events += 1;
            self.bytes += line.len();
        }
        self.file
            .flush()
            .map_err(|_| anyhow!("host_trace_flush_failed"))?;
        Ok(())
    }
}
fn label(value: &Value) -> Option<String> {
    value
        .as_str()
        .filter(|text| !text.is_empty() && text.len() <= 256 && !text.chars().any(char::is_control))
        .map(str::to_owned)
}
