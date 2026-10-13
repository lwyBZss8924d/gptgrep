use crate::jev_accounting::Accounting;
use anyhow::{Result, anyhow, ensure};
use gptgrep_core::{Hit, NodeCoverage, SearchOptions};
use gptgrep_jev::JevClient;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

pub(crate) const MAX_TOOL_BYTES: usize = 16 * 1024;
const MAX_TEXT_BYTES: usize = 6144;
const READ_DEFAULT: usize = 4096;
const CATALOG_DEFAULT: usize = 20;
const CATALOG_MAX: usize = 30;
const TREE_DEFAULT: usize = 24;
const TREE_MAX: usize = 40;
const SEARCH_MAX: usize = 3;

#[derive(Debug)]
struct ArgumentError(String);
impl std::fmt::Display for ArgumentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for ArgumentError {}
fn argument_error(message: impl Into<String>) -> anyhow::Error {
    ArgumentError(message.into()).into()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Citation {
    pub node_id: String,
    pub path: String,
    pub citation: String,
    pub source_sha256: String,
    pub excerpt_sha256: String,
    pub line_start: usize,
    pub line_end: usize,
    pub page_start: u32,
    pub page_end: u32,
    pub byte_start: usize,
    pub byte_end: usize,
    pub node_offset: usize,
    pub next_offset: Option<usize>,
    /// Coverage of this issued excerpt only; prior reads may cover other bytes.
    #[serde(default)]
    pub node_coverage: Option<NodeCoverage>,
    pub column_start: usize,
    pub coordinate_system: String,
    pub text_truncated: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolReceipt {
    pub call_id: String,
    pub tool: String,
    pub arguments: Value,
    pub generation: String,
    pub success: bool,
    pub output_sha256: String,
    pub evidence: Vec<Citation>,
    #[serde(default)]
    pub required_initial: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub search: Option<crate::SearchTelemetry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_budget: Option<crate::ToolBudgetDiagnostics>,
}

pub(crate) struct Evidence {
    root: PathBuf,
    catalog: Value,
    pub generation: String,
    known_nodes: BTreeSet<String>,
    issued: BTreeMap<String, Vec<Citation>>,
    pub receipts: Vec<ToolReceipt>,
    selected_document: Option<(String, String)>,
    document: Option<String>,
    jev_model: Option<String>,
    decisions_execution: Option<gptgrep_jev::ExecutionContext>,
    client: Option<JevClient>,
    accounting: Option<Accounting>,
    pub initial_payload: Option<Value>,
    initial_in_progress: bool,
    last_search: Option<crate::SearchTelemetry>,
    last_search_fatal: bool,
    current_call_id: String,
    evidence_roles: bool,
    navigation: Option<crate::navigation::BoundNavigation>,
}

impl Evidence {
    pub fn open(root: &Path, node_id: Option<&str>) -> Result<Self> {
        let catalog = gptgrep_core::catalog(root)?;
        let generation = catalog["generation"]
            .as_str()
            .ok_or_else(|| anyhow!("Index generation unavailable"))?
            .to_owned();
        let mut known_nodes = BTreeSet::new();
        let mut selected_document = None;
        if let Some(id) = node_id {
            let selected = gptgrep_core::read_node(root, id, 4)?;
            selected_document = Some((selected.path, selected.title));
            known_nodes.insert(id.to_owned());
        }
        Ok(Self {
            root: root.to_owned(),
            catalog,
            generation,
            known_nodes,
            issued: BTreeMap::new(),
            receipts: vec![],
            selected_document,
            document: None,
            jev_model: None,
            decisions_execution: None,
            client: None,
            accounting: None,
            initial_payload: None,
            initial_in_progress: false,
            last_search: None,
            last_search_fatal: false,
            current_call_id: String::new(),
            evidence_roles: false,
            navigation: None,
        })
    }

    pub fn configure(
        &mut self,
        config: &crate::HostConfig,
        accounting: Accounting,
        client: Option<JevClient>,
    ) -> Result<()> {
        self.evidence_roles = config
            .query_plan
            .as_ref()
            .is_some_and(|plan| plan.evidence_roles);
        self.accounting = Some(accounting);
        self.decisions_execution = config.decisions_execution.clone();
        self.client = client.map(|client| match &self.decisions_execution {
            Some(context) => client.with_execution_context(context.clone()),
            None => client,
        });
        self.jev_model = config.jev_model.clone();
        let document = match (&self.selected_document, &config.document) {
            (Some((selected, _)), Some(requested)) if selected != requested => {
                return Err(anyhow!("host_document_conflict"));
            }
            (Some((selected, _)), _) => Some(selected.clone()),
            (None, document) => document.clone(),
        };
        if let Some(document) = &document {
            let docs = self.catalog["documents"]
                .as_array_mut()
                .ok_or_else(|| anyhow!("Invalid catalog"))?;
            docs.retain(|doc| doc["path"] == *document);
            ensure!(!docs.is_empty(), "host_document_scope_unavailable");
        }
        self.document = document;
        self.ensure_generation()
    }

    pub async fn bootstrap(&mut self, question: &str) -> Result<()> {
        ensure!(
            self.initial_payload.is_none(),
            "host_jev_initial_already_completed"
        );
        let query = match &self.selected_document {
            Some((path, title)) => format!(
                "Summarize the selected document {path}, section {}. {question}",
                short(title, 256)
            ),
            None => question.to_owned(),
        };
        self.initial_in_progress = true;
        let result = self
            .call(
                "host-initial-jev",
                "gptgrep_search",
                json!({"query":query,"mode":"hybrid","limit":SEARCH_MAX}),
            )
            .await;
        self.initial_in_progress = false;
        let payload = result?;
        ensure!(payload["success"] == true, "host_jev_seed_delivery_failed");
        let search = self
            .last_search
            .as_ref()
            .ok_or_else(|| anyhow!("host_jev_initial_missing"))?;
        let coverage = search
            .coverage
            .as_ref()
            .ok_or_else(|| anyhow!("host_jev_initial_coverage_missing"))?;
        if coverage.indexed_files == 0 {
            return Err(anyhow!("host_jev_empty_corpus"));
        }
        ensure!(
            coverage.reranked_candidates > 0 && search.metrics.jev_requests > 0,
            "host_jev_no_candidates"
        );
        self.initial_payload = Some(serde_json::from_str(
            payload["contentItems"][0]["text"]
                .as_str()
                .ok_or_else(|| anyhow!("host_jev_seed_invalid"))?,
        )?);
        Ok(())
    }

    pub fn prepare_query_plan(
        &mut self,
        question: &str,
    ) -> Result<crate::query_plan::PlannerInput> {
        ensure!(self.selected_document.is_none(), "host_query_plan_ask_only");
        self.ensure_generation()?;
        self.ensure_jev_client()?;
        let mut input = crate::query_plan::planner_input(
            &self.root,
            &self.catalog,
            &self.generation,
            self.document.as_deref(),
            question,
        )?;
        if let Some(packet) = self.navigation_packet()? {
            input.state["navigation"] = packet.clone();
            input.instructions.push('\n');
            input.instructions.push_str(crate::navigation::GUIDANCE);
        }
        Ok(input)
    }

    pub async fn prepare_navigation(
        &mut self,
        question: &str,
        config: &crate::NavigationConfig,
        deadline: tokio::time::Instant,
    ) -> Result<()> {
        ensure!(
            self.selected_document.is_none() && self.navigation.is_none(),
            "host_navigation_ask_only"
        );
        self.ensure_generation()?;
        self.ensure_jev_client()?;
        let document = self
            .document
            .as_deref()
            .ok_or_else(|| anyhow!("host_navigation_document_required"))?;
        let accounting = self
            .accounting
            .as_ref()
            .ok_or_else(|| anyhow!("host_navigation_accounting_required"))?;
        // Navigation scans keep their declared per-batch submission budget:
        // the bounded transient retry of retrieval calls is not applied.
        let client = self
            .client
            .clone()
            .expect("initialized client")
            .without_transient_retry();
        self.navigation = Some(
            crate::navigation::run(
                crate::navigation::NavigationRequest {
                    root: &self.root,
                    question,
                    generation: &self.generation,
                    document,
                    config,
                    deadline,
                },
                &client,
                accounting,
            )
            .await?,
        );
        Ok(())
    }

    pub fn navigation_packet(&self) -> Result<Option<&Value>> {
        if let Some(navigation) = &self.navigation {
            navigation.verify(&self.root)?;
        }
        Ok(self
            .navigation
            .as_ref()
            .map(|navigation| &navigation.packet))
    }

    pub fn reader_state(&self, question: &str, node_id: Option<&str>) -> Result<Value> {
        let mut state = json!({"question":question,"selected_node_id":node_id,"snapshot_generation":self.generation,
            "initial_retrieval":self.initial_payload,"document_scope":self.document_scope(),"jev_required":true});
        if let Some(packet) = self.navigation_packet()? {
            state["navigation"] = packet.clone();
        }
        Ok(state)
    }

    pub fn navigation_guidance(&self) -> Option<&'static str> {
        self.navigation
            .as_ref()
            .map(|_| crate::navigation::GUIDANCE)
    }

    fn ensure_jev_client(&mut self) -> Result<()> {
        if self.client.is_none() {
            // Retrieval routing and reranking admit one bounded retry of a
            // transient provider status; each attempt is separately accounted.
            let client = JevClient::from_env(self.jev_model.as_deref())
                .map(|client| client.with_transient_retry(gptgrep_jev::TransientRetry::standard()))
                .map_err(|error| {
                    anyhow!(crate::jev_accounting::InitializationError {
                        cause: error.to_string()
                    })
                })?;
            self.client = Some(match &self.decisions_execution {
                Some(context) => client.with_execution_context(context.clone()),
                None => client,
            });
        }
        Ok(())
    }

    pub async fn bootstrap_planned(
        &mut self,
        question: &str,
        alternatives: &[String],
    ) -> Result<()> {
        ensure!(
            self.initial_payload.is_none(),
            "host_jev_initial_already_completed"
        );
        ensure!(self.selected_document.is_none(), "host_query_plan_ask_only");
        self.ensure_generation()?;
        self.ensure_jev_client()?;
        let accounting = self
            .accounting
            .clone()
            .ok_or_else(|| anyhow!("host_query_plan_accounting_required"))?;
        let call_id = "host-initial-jev";
        let index =
            accounting.start_search(call_id, question, "hybrid", self.document.as_deref(), true)?;
        let mut args = json!({"query":question,"alternate_queries":alternatives,"mode":"hybrid","limit":SEARCH_MAX,"strategy":"luna_queries_v1"});
        if self.evidence_roles {
            args["evidence_roles"] = json!(true);
        }
        let prior_nodes = self.known_nodes.clone();
        let prior_issued = self.issued.clone();
        let prior_receipts = self.receipts.len();
        let result: Result<()> = async {
            let options = SearchOptions {
                mode: "hybrid".into(),
                limit: SEARCH_MAX,
                document: self.document.clone(),
                model: self.jev_model.clone(),
                ..Default::default()
            };
            let observer = |event: &gptgrep_core::PlannedSearchEvent| accounting.plan_event(index, event);
            let mut found = gptgrep_core::search_planned_with_policy_and_observer(
                &self.root,
                question,
                alternatives,
                &options,
                &self.generation,
                self.client.as_ref().expect("initialized client"),
                if self.evidence_roles { gptgrep_core::PlannedScoringPolicy::EvidenceRoles } else { gptgrep_core::PlannedScoringPolicy::Relevance },
                &observer,
            ).await?;
            self.ensure_generation()?;
            ensure!(found.generation == self.generation, "host_generation_changed");
            ensure!(found.document_scope == self.document, "host_document_scope_changed");
            ensure!(found.query == question, "host_query_plan_question_changed");
            ensure!(found.coverage.indexed_files > 0, "host_jev_empty_corpus");
            ensure!(found.coverage.reranked_candidates > 0 && found.metrics.jev_requests > 0, "host_jev_no_candidates");
            ensure!(found.evidence_roles.is_some() == self.evidence_roles, "host_evidence_role_policy_mismatch");
            if let Some(diagnostics) = &found.evidence_roles {
                crate::evidence_roles::validate_policy(diagnostics, &self.generation, question)?;
            }
            let mut search = accounting.planned_success(index, &found)?;
            let original_hits = found.hits.len();
            let assessed_hits = found.hits.clone();
            let (value, payload) = loop {
                let mut value = serde_json::to_value(&found)?;
                // Full operation/provenance receipts remain in the private ledger/report.
                // The reader receives coverage counts and only delivered source windows.
                value.as_object_mut().expect("report object").remove("operations");
                value.as_object_mut().expect("report object").remove("evidence_roles");
                if let Some(coverage) = value["coverage"].as_object_mut() {
                    coverage.remove("selected_spans");
                }
                value["host_delivery"] = json!({"omitted_hits":original_hits-found.hits.len()});
                if let Some(diagnostics) = &found.evidence_roles {
                    for (index, hit) in found.hits.iter().enumerate() {
                        value["hits"][index] = crate::evidence_roles::delivered_hit(diagnostics, &self.generation, &assessed_hits[index], hit)?;
                    }
                    value["host_delivery"]["delivered_set_sufficiency"] = json!("unassessed");
                }
                let payload = json!({"contentItems":[{"type":"inputText","text":value.to_string()}],"success":true});
                if serde_json::to_vec(&payload)?.len() <= MAX_TOOL_BYTES - 256 {
                    break (value, payload);
                }
                ensure!(!found.hits.is_empty(), "host_jev_seed_delivery_failed");
                found.hits.pop();
                // The reader's fallback count describes only the hits it receives.
                if found.coverage.below_threshold_fallback > 0 {
                    found.coverage.below_threshold_fallback = found
                        .hits
                        .iter()
                        .filter(|hit| hit.is_below_threshold_fallback())
                        .count();
                }
            };
            ensure!(original_hits == 0 || !found.hits.is_empty(), "host_jev_seed_delivery_failed");
            if let Some(diagnostics) = &mut found.evidence_roles {
                // Packing currently drops trailing whole hits. Keep each assessed
                // original paired explicitly with its retained window or omission.
                for (index, assessed) in assessed_hits.iter().enumerate() {
                    diagnostics.record_delivery(assessed, found.hits.get(index))?;
                }
                diagnostics.delivery_packet_sha256 = Some(hash(&serde_json::to_vec(&payload)?));
                diagnostics.validate_bound()?;
                search = accounting.role_delivery(index, diagnostics)?;
            }
            let mut evidence = vec![];
            for hit in &found.hits {
                self.known_nodes.insert(hit.node_id.clone());
                self.issue(hit, &mut evidence)?;
            }
            search.delivered_hits = evidence.len();
            search.output_truncated = found.hits.len() < original_hits;
            search = accounting.delivery(&search)?;
            let receipt = ToolReceipt {
                call_id: call_id.into(),
                tool: "gptgrep_search".into(),
                arguments: args.clone(),
                generation: self.generation.clone(),
                success: true,
                output_sha256: hash(&serde_json::to_vec(&payload)?),
                evidence,
                required_initial: true,
                search: Some(search.clone()),
                tool_budget: None,
            };
            accounting.receipt(&receipt)?;
            self.receipts.push(receipt);
            self.last_search = Some(search);
            self.initial_payload = Some(value);
            Ok(())
        }.await;
        if let Err(error) = result {
            self.known_nodes = prior_nodes;
            self.issued = prior_issued;
            self.receipts.truncate(prior_receipts);
            self.initial_payload = None;
            self.last_search = Some(accounting.failure(index, &error)?);
            let payload = json!({"success":false,"error":"host_query_plan_retrieval_failed"});
            let receipt = ToolReceipt {
                call_id: call_id.into(),
                tool: "gptgrep_search".into(),
                arguments: args,
                generation: self.generation.clone(),
                success: false,
                output_sha256: hash(&serde_json::to_vec(&payload)?),
                evidence: vec![],
                required_initial: true,
                search: self.last_search.clone(),
                tool_budget: None,
            };
            accounting.receipt(&receipt)?;
            self.receipts.push(receipt);
            return Err(error);
        }
        Ok(())
    }

    fn ensure_generation(&self) -> Result<()> {
        let catalog =
            gptgrep_core::catalog(&self.root).map_err(|_| anyhow!("host_index_unavailable"))?;
        ensure!(
            catalog["generation"] == self.generation,
            "host_generation_changed"
        );
        if let Some(navigation) = &self.navigation {
            navigation.verify(&self.root)?;
        }
        Ok(())
    }
    pub fn document_scope(&self) -> Option<&str> {
        self.document.as_deref()
    }
    pub fn evidence_role_guidance(&self) -> Option<&'static str> {
        self.evidence_roles
            .then_some(crate::evidence_roles::READER_GUIDANCE)
    }

    pub async fn call(&mut self, call_id: &str, tool: &str, args: Value) -> Result<Value> {
        self.ensure_generation()?;
        let prior_nodes = self.known_nodes.clone();
        let prior_issued = self.issued.clone();
        let mut evidence = vec![];
        self.last_search = None;
        self.last_search_fatal = false;
        self.current_call_id = call_id.to_owned();
        let output = self.run(tool, &args, &mut evidence).await;
        self.ensure_generation()?;
        let mut fatal = None;
        let (value, mut success) = match output {
            Ok(value) => (value, true),
            Err(error) => {
                let (code, message) = if let Some(error) = error.downcast_ref::<ArgumentError>() {
                    ("invalid_arguments", error.0.as_str())
                } else {
                    (
                        "evidence_unavailable",
                        "The requested evidence is unavailable or stale. Use the current catalog/tree IDs; source changes require reindexing outside this read-only workflow.",
                    )
                };
                let value = json!({"error":code,"message":message});
                if self.last_search_fatal {
                    fatal = Some(error);
                }
                (value, false)
            }
        };
        let text =
            serde_json::to_string(&value).map_err(|_| anyhow!("host_tool_result_encoding"))?;
        let mut payload =
            json!({"contentItems":[{"type":"inputText","text":text}],"success":success});
        if payload_bytes(&payload)?.len() > MAX_TOOL_BYTES {
            success = false;
            let error = json!({"error":"tool_output_limit","message":format!("Serialized result exceeds {MAX_TOOL_BYTES} bytes. Retry with a smaller limit, or a smaller max_bytes for read. No evidence from this response was delivered.")});
            payload = json!({"contentItems":[{"type":"inputText","text":error.to_string()}],"success":false});
        }
        if !success {
            self.known_nodes = prior_nodes;
            self.issued = prior_issued;
            evidence.clear();
        }
        if let Some(search) = &mut self.last_search {
            search.delivered_hits = evidence.len();
            search.output_truncated |= !success;
        }
        if let (Some(accounting), Some(search)) = (&self.accounting, &self.last_search) {
            accounting.delivery(search)?;
        }
        self.record_receipt(ToolReceipt {
            call_id: call_id.to_owned(),
            tool: tool.to_owned(),
            arguments: args,
            generation: self.generation.clone(),
            success,
            output_sha256: hash(&payload_bytes(&payload)?),
            evidence,
            required_initial: self.initial_in_progress,
            search: self.last_search.clone(),
            tool_budget: None,
        })?;
        if let Some(error) = fatal {
            return Err(error);
        }
        Ok(payload)
    }

    fn record_receipt(&mut self, receipt: ToolReceipt) -> Result<()> {
        self.receipts.push(receipt);
        if let Some(accounting) = &self.accounting {
            accounting.receipt(self.receipts.last().expect("just pushed"))?;
        }
        Ok(())
    }

    /// Validate the advertised shape without reading sources or invoking Jev.
    /// Budget exhaustion must not turn an unsupported or malformed request into
    /// a recoverable tool result. Under-budget argument handling is unchanged.
    pub fn validate_budget_request(&self, tool: &str, args: &Value) -> Result<()> {
        let specifications = tools();
        let schema = specifications[0]["tools"]
            .as_array()
            .and_then(|tools| tools.iter().find(|spec| spec["name"] == tool))
            .map(|spec| &spec["inputSchema"])
            .ok_or_else(|| anyhow!("host_tool_budget_unknown_tool"))?;
        ensure!(
            jsonschema::is_valid(schema, args)
                && args.as_object().is_some_and(|args| args
                    .values()
                    .all(|value| !value.is_number() || value.as_u64().is_some()))
                && args["query"]
                    .as_str()
                    .is_none_or(|query| query.len() <= 2048),
            "host_tool_budget_invalid_arguments"
        );
        Ok(())
    }

    pub fn deny_budget(
        &mut self,
        call_id: &str,
        tool: &str,
        args: Value,
        budget: crate::ToolBudgetDiagnostics,
    ) -> Result<Value> {
        let value = json!({
            "error":"tool_budget_exhausted",
            "message":"No tool calls remain. Do not request more tools. Finalize now using only evidence already issued in this turn, including initial_retrieval. Preserve the required answer schema and issued citation IDs; if evidence is insufficient, state that and set insufficient_evidence=true.",
            "tool_budget":budget
        });
        let payload =
            json!({"contentItems":[{"type":"inputText","text":value.to_string()}],"success":false});
        self.record_receipt(ToolReceipt {
            call_id: call_id.into(),
            tool: tool.into(),
            arguments: args,
            generation: self.generation.clone(),
            success: false,
            output_sha256: hash(&payload_bytes(&payload)?),
            evidence: vec![],
            required_initial: false,
            search: None,
            tool_budget: Some(budget),
        })?;
        Ok(payload)
    }

    async fn run(
        &mut self,
        tool: &str,
        args: &Value,
        evidence: &mut Vec<Citation>,
    ) -> Result<Value> {
        let object = args
            .as_object()
            .ok_or_else(|| argument_error("Arguments must be a JSON object."))?;
        match tool {
            "gptgrep_catalog" => {
                fields(object, &["offset", "limit"])?;
                let (offset, limit) = window(args, CATALOG_DEFAULT, CATALOG_MAX)?;
                let docs = self.catalog["documents"]
                    .as_array()
                    .ok_or_else(|| anyhow!("Invalid catalog"))?;
                let rows: Vec<_> = docs.iter().skip(offset).take(limit).map(|doc| json!({
                    "id":doc["id"],"path":doc["path"],"title":short(doc["title"].as_str().unwrap_or(""),256),
                    "pages":doc["pages"],"nodes":doc["nodes"],"source_sha256":doc["source_sha256"]
                })).collect();
                Ok(
                    json!({"generation":self.generation,"document_scope":self.document,"documents":rows,"total":docs.len(),"next_offset":next(offset,limit,docs.len())}),
                )
            }
            "gptgrep_tree" => {
                fields(object, &["path", "offset", "limit"])?;
                let path = args["path"].as_str().ok_or_else(|| {
                    argument_error(
                        "path must be a relative document path returned by gptgrep_catalog.",
                    )
                })?;
                if !self.catalog["documents"]
                    .as_array()
                    .is_some_and(|docs| docs.iter().any(|doc| doc["path"] == path))
                {
                    return Err(argument_error(
                        "Unknown document path. Select path from gptgrep_catalog.",
                    ));
                }
                let tree = gptgrep_core::tree(&self.root, Path::new(path))?;
                ensure!(tree["generation"] == self.generation, "Changed generation");
                let doc = &tree["document"];
                let id = doc["id"]
                    .as_str()
                    .ok_or_else(|| anyhow!("Invalid document"))?;
                let nodes = doc["nodes"]
                    .as_array()
                    .ok_or_else(|| anyhow!("Invalid tree"))?;
                let (offset, limit) = window(args, TREE_DEFAULT, TREE_MAX)?;
                let mut rows = vec![];
                let mut descriptor_bytes = 0;
                for node in nodes.iter().skip(offset).take(limit) {
                    let local_id = node["id"].as_str().ok_or_else(|| anyhow!("Invalid node"))?;
                    let node_id = format!("{id}:{local_id}");
                    let items = node["key_items"].as_array();
                    let key_items: Vec<_> = items
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_str)
                        .take(4)
                        .map(|item| short(item, 200))
                        .collect();
                    let truncated = items.is_some_and(|items| {
                        items.len() > 4
                            || items
                                .iter()
                                .filter_map(Value::as_str)
                                .any(|item| item.len() > 200)
                    });
                    let row = json!({"node_id":node_id,
                        "parent_id":node["parent_id"].as_str().map(|parent|format!("{id}:{parent}")),
                        "title":short(node["title"].as_str().unwrap_or(""),256),
                        "key_items":key_items,"key_items_truncated":truncated,
                        "line_start":node["line_start"],"line_end":node["line_end"],
                        "page_start":node["page_start"],"page_end":node["page_end"]});
                    let row_bytes = serde_json::to_vec(&row)?.len();
                    if descriptor_bytes + row_bytes > MAX_TOOL_BYTES / 2 {
                        break;
                    }
                    descriptor_bytes += row_bytes;
                    self.known_nodes.insert(node_id.clone());
                    rows.push(row);
                }
                let returned = rows.len();
                Ok(
                    json!({"generation":self.generation,"document_scope":self.document,"path":path,"source_sha256":doc["source_sha256"],"nodes":rows,"total":nodes.len(),"next_offset":next(offset,returned,nodes.len())}),
                )
            }
            "gptgrep_read" => {
                fields(object, &["node_id", "max_bytes", "offset_bytes"])?;
                let id = args["node_id"].as_str().ok_or_else(|| {
                    argument_error(
                        "node_id must be a string returned by gptgrep_tree or gptgrep_search.",
                    )
                })?;
                if !self.known_nodes.contains(id) {
                    return Err(argument_error(
                        "node_id was not issued. First use gptgrep_tree or gptgrep_search.",
                    ));
                }
                let max_bytes = integer(args, "max_bytes", READ_DEFAULT)?;
                if !(1..=MAX_TEXT_BYTES).contains(&max_bytes) {
                    return Err(argument_error(format!(
                        "max_bytes must be an integer in 1..{MAX_TEXT_BYTES}; omit it for default {READ_DEFAULT}."
                    )));
                }
                let offset_bytes = integer(args, "offset_bytes", 0)?;
                let hit = gptgrep_core::read_node_window(&self.root, id, max_bytes, offset_bytes)?;
                self.issue(&hit, evidence)?;
                Ok(
                    json!({"generation":self.generation,"document_scope":self.document,"node_offset":hit.node_offset,"next_offset":hit.next_offset,"evidence":hit}),
                )
            }
            "gptgrep_search" => self.run_search(args, evidence).await,
            _ => Err(anyhow!("Tool is not allowed")),
        }
    }

    async fn run_search(&mut self, args: &Value, evidence: &mut Vec<Citation>) -> Result<Value> {
        let object = args
            .as_object()
            .ok_or_else(|| argument_error("Arguments must be a JSON object."))?;
        fields(object, &["query", "mode", "limit", "document"])?;
        let query = args["query"]
            .as_str()
            .ok_or_else(|| argument_error("query must be a string of 1..8192 UTF-8 bytes."))?;
        let query_limit = if self.initial_in_progress { 8192 } else { 2048 };
        if query.is_empty() || query.len() > query_limit {
            return Err(argument_error(format!(
                "query must contain 1..{query_limit} UTF-8 bytes."
            )));
        }
        let mode = match args.get("mode") {
            Some(value) => value.as_str().ok_or_else(|| {
                argument_error("mode must be hybrid, semantic, regex or lexical; default hybrid.")
            })?,
            None => "hybrid",
        };
        if !matches!(mode, "hybrid" | "semantic" | "regex" | "lexical") {
            return Err(argument_error(
                "mode must be hybrid, semantic, regex or lexical; default hybrid.",
            ));
        }
        let limit = integer(args, "limit", SEARCH_MAX)?;
        if !(1..=SEARCH_MAX).contains(&limit) {
            return Err(argument_error(format!(
                "limit must be an integer in 1..{SEARCH_MAX}; omit it for default {SEARCH_MAX}."
            )));
        }
        if self.accounting.is_some() && !self.initial_in_progress && self.initial_payload.is_none()
        {
            return Err(anyhow!("host_jev_initial_required"));
        }
        let requested = args
            .get("document")
            .filter(|value| !value.is_null())
            .map(|value| {
                value
                    .as_str()
                    .ok_or_else(|| argument_error("document must be a catalog-issued path."))
            })
            .transpose()?;
        if let (Some(scope), Some(requested)) = (&self.document, requested)
            && scope != requested
        {
            return Err(argument_error(
                "document cannot widen or replace the caller's document scope.",
            ));
        }
        let document = self
            .document
            .clone()
            .or_else(|| requested.map(str::to_owned));
        if let Some(document) = &document
            && !self.catalog["documents"]
                .as_array()
                .is_some_and(|docs| docs.iter().any(|doc| doc["path"] == *document))
        {
            return Err(argument_error(
                "document must be a path returned by the current catalog.",
            ));
        }
        let options = SearchOptions {
            mode: mode.into(),
            limit,
            context: 0,
            model: self.jev_model.clone(),
            document: document.clone(),
            ..Default::default()
        };
        let index = if let Some(accounting) = &self.accounting {
            Some(accounting.start_search(
                &self.current_call_id,
                query,
                mode,
                document.as_deref(),
                self.initial_in_progress,
            )?)
        } else {
            None
        };
        let assisted = matches!(mode, "hybrid" | "semantic");
        let result = if assisted {
            self.last_search_fatal = true;
            if let Err(error) = self.ensure_jev_client() {
                if let (Some(accounting), Some(index)) = (&self.accounting, index) {
                    self.last_search = Some(accounting.failure(index, &error)?);
                }
                return Err(error);
            }
            let client = self.client.as_ref().expect("initialized client");
            if let (Some(accounting), Some(index)) = (&self.accounting, index) {
                let observer = |progress: &gptgrep_core::JevSearchProgress| {
                    accounting.progress(index, progress)
                };
                gptgrep_core::search_with_client_and_observer(
                    &self.root, query, &options, client, &observer,
                )
                .await
            } else {
                gptgrep_core::search_with_client(&self.root, query, &options, client).await
            }
        } else {
            gptgrep_core::search(&self.root, query, &options).await
        };
        let mut found = match result {
            Ok(found) => found,
            Err(error) => {
                if let (Some(accounting), Some(index)) = (&self.accounting, index) {
                    self.last_search = Some(accounting.failure(index, &error)?);
                }
                return Err(coded_search_error(error));
            }
        };
        if let (Some(accounting), Some(index)) = (&self.accounting, index) {
            self.last_search = Some(accounting.success(index, &found, 0, false)?);
        }
        ensure!(
            found.generation == self.generation,
            "host_generation_changed"
        );
        ensure!(
            found.document_scope == document,
            "host_document_scope_changed"
        );
        let original_hits = found.hits.len();
        if self.initial_in_progress {
            loop {
                let value = serde_json::to_value(&found)
                    .map_err(|_| anyhow!("host_tool_result_encoding"))?;
                let payload = json!({"contentItems":[{"type":"inputText","text":value.to_string()}],"success":true});
                if payload_bytes(&payload)?.len() <= MAX_TOOL_BYTES - 256 {
                    break;
                }
                ensure!(!found.hits.is_empty(), "host_jev_seed_delivery_failed");
                found.hits.pop();
                // The reader's fallback count describes only the hits it receives.
                if found.coverage.below_threshold_fallback > 0 {
                    found.coverage.below_threshold_fallback = found
                        .hits
                        .iter()
                        .filter(|hit| hit.is_below_threshold_fallback())
                        .count();
                }
            }
            ensure!(
                original_hits == 0 || !found.hits.is_empty(),
                "host_jev_seed_delivery_failed"
            );
        }
        for hit in &found.hits {
            self.known_nodes.insert(hit.node_id.clone());
            self.issue(hit, evidence)?;
        }
        if let Some(search) = &mut self.last_search {
            search.delivered_hits = evidence.len();
            search.output_truncated = found.hits.len() < original_hits;
        }
        self.last_search_fatal = false;
        let mut value =
            serde_json::to_value(found).map_err(|_| anyhow!("host_tool_result_encoding"))?;
        value["host_delivery"] = json!({"omitted_hits":original_hits-evidence.len()});
        Ok(value)
    }

    fn issue(&mut self, hit: &Hit, evidence: &mut Vec<Citation>) -> Result<()> {
        // An EOF response advances no evidence and must not become a citable empty span.
        if hit.text.is_empty() {
            return Ok(());
        }
        let node_offset = hit
            .node_offset
            .ok_or_else(|| anyhow!("host_evidence_interval_uncontained"))?;
        let citation = Citation {
            node_id: hit.node_id.clone(),
            path: hit.path.clone(),
            citation: hit.citation.clone(),
            source_sha256: hit.source_sha256.clone(),
            excerpt_sha256: hash(hit.text.as_bytes()),
            line_start: hit.line_start,
            line_end: hit.line_end,
            page_start: hit.page_start,
            page_end: hit.page_end,
            byte_start: hit.byte_start,
            byte_end: hit.byte_end,
            node_offset,
            next_offset: hit.next_offset,
            node_coverage: hit.node_coverage,
            column_start: hit.column_start,
            coordinate_system: hit.coordinate_system.clone(),
            text_truncated: hit.text_truncated,
        };
        let previous = self.issued.entry(hit.node_id.clone()).or_default();
        if !previous.iter().any(|prior| {
            prior.excerpt_sha256 == citation.excerpt_sha256
                && prior.byte_start == citation.byte_start
        }) {
            previous.push(citation.clone());
        }
        evidence.push(citation);
        Ok(())
    }

    pub fn finish(&self, raw: &str) -> Result<(String, Vec<Citation>, bool)> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Answer {
            answer: String,
            citations: Vec<String>,
            insufficient_evidence: bool,
        }
        ensure!(raw.len() <= 16 * 1024, "host_answer_too_large");
        // Only the classification of a rejected answer is kept, never its text.
        let answer: Answer = serde_json::from_str(raw).map_err(|error| {
            if error.is_data() {
                anyhow!("host_answer_schema_mismatch")
            } else {
                anyhow!("host_answer_invalid_json")
            }
        })?;
        ensure!(
            !answer.answer.trim().is_empty() && answer.answer.len() <= 8192,
            "host_answer_length_invalid"
        );
        ensure!(answer.citations.len() <= 24, "host_answer_citation_limit");
        ensure!(
            answer.insufficient_evidence || !answer.citations.is_empty(),
            "host_answer_citations_missing"
        );
        self.ensure_generation()?;
        let mut seen = BTreeSet::new();
        if !self.receipts.iter().any(|receipt| {
            receipt.success && matches!(receipt.tool.as_str(), "gptgrep_search" | "gptgrep_read")
        }) {
            return Err(crate::HostCapabilityError.into());
        }
        let mut citations = vec![];
        for id in answer.citations {
            ensure!(seen.insert(id.clone()), "host_answer_citation_repeated");
            let issued = self
                .issued
                .get(&id)
                .ok_or_else(|| anyhow!("host_answer_citation_unissued"))?;
            for item in issued {
                let bytes = item
                    .byte_end
                    .checked_sub(item.byte_start)
                    .filter(|bytes| (1..=MAX_TEXT_BYTES).contains(bytes))
                    .ok_or_else(|| anyhow!("host_answer_citation_window_invalid"))?;
                let fresh =
                    gptgrep_core::read_node_window(&self.root, &id, bytes, item.node_offset)
                        .map_err(|_| anyhow!("host_answer_citation_window_unreadable"))?;
                ensure!(
                    fresh.source_fresh && fresh.source_sha256 == item.source_sha256,
                    "host_answer_citation_source_changed"
                );
                ensure!(
                    fresh.node_offset == Some(item.node_offset)
                        && fresh.byte_start == item.byte_start
                        && fresh.byte_end == item.byte_end
                        && hash(fresh.text.as_bytes()) == item.excerpt_sha256
                        && fresh.line_start == item.line_start
                        && fresh.line_end == item.line_end
                        && fresh.page_start == item.page_start
                        && fresh.page_end == item.page_end
                        && fresh.column_start == item.column_start
                        && fresh.coordinate_system == item.coordinate_system,
                    "host_answer_citation_window_changed"
                );
            }
            citations.extend(issued.iter().cloned());
        }
        self.ensure_generation()?;
        Ok((answer.answer, citations, answer.insufficient_evidence))
    }
}

/// Public failure telemetry is a fixed code, never the model's answer or source text.
/// The cause codes predate the stable `host_answer_*` error codes and are kept.
pub(crate) fn citation_validation_code(error: &anyhow::Error) -> &'static str {
    match error.to_string().as_str() {
        "host_answer_too_large" => "final_response_byte_limit",
        "host_answer_invalid_json" | "host_answer_schema_mismatch" => "final_response_schema",
        "host_answer_length_invalid" => "answer_length",
        "host_answer_citation_limit" => "citation_count",
        "host_answer_citations_missing" => "missing_citations",
        "host_answer_citation_repeated" => "duplicate_citation",
        "host_answer_citation_unissued" => "unissued_citation",
        "host_answer_citation_window_invalid" => "invalid_issued_window",
        "host_answer_citation_window_unreadable" => "unreadable_cited_window",
        "host_answer_citation_source_changed" => "stale_cited_source",
        "host_answer_citation_window_changed" => "changed_cited_window",
        "host_generation_changed" => "generation_changed",
        "host_index_unavailable" => "index_unavailable",
        _ if error.downcast_ref::<crate::HostCapabilityError>().is_some() => "no_evidence_tools",
        _ => "unclassified_citation_validation",
    }
}

/// Host-built tool payloads; a serializer failure keeps no text.
fn payload_bytes(payload: &Value) -> Result<Vec<u8>> {
    serde_json::to_vec(payload).map_err(|_| anyhow!("host_tool_result_encoding"))
}

/// A failed search keeps typed Jev failures and host codes for accounting and
/// the ask code; any other cause, such as local index text, becomes one code.
fn coded_search_error(error: anyhow::Error) -> anyhow::Error {
    let typed = error.is::<gptgrep_core::JevSearchError>()
        || error.is::<gptgrep_core::PlannedSearchError>()
        || error.is::<crate::jev_accounting::InitializationError>();
    if typed || crate::model_attempts::is_host_code(&error.to_string()) {
        error
    } else {
        anyhow!("host_search_failed")
    }
}

fn integer(value: &Value, key: &str, default: usize) -> Result<usize> {
    value
        .get(key)
        .map(|v| {
            v.as_u64()
                .and_then(|v| usize::try_from(v).ok())
                .ok_or_else(|| {
                    argument_error(format!(
                        "{key} must be a non-negative integer; omit it for default {default}."
                    ))
                })
        })
        .unwrap_or(Ok(default))
}
fn window(value: &Value, default: usize, max: usize) -> Result<(usize, usize)> {
    let offset = integer(value, "offset", 0)?;
    let limit = integer(value, "limit", default)?;
    if !(1..=max).contains(&limit) {
        return Err(argument_error(format!(
            "limit must be an integer in 1..{max}; omit it for default {default}. Use next_offset for another page."
        )));
    }
    Ok((offset, limit))
}
fn next(offset: usize, limit: usize, total: usize) -> Option<usize> {
    offset.checked_add(limit).filter(|next| *next < total)
}
fn fields(object: &serde_json::Map<String, Value>, allowed: &[&str]) -> Result<()> {
    if !object.keys().all(|key| allowed.contains(&key.as_str())) {
        return Err(argument_error(format!(
            "Allowed arguments: {}.",
            allowed.join(", ")
        )));
    }
    Ok(())
}
fn short(text: &str, max: usize) -> String {
    let mut end = text.len().min(max);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}
pub(crate) fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

pub(crate) fn tools() -> Value {
    let coverage = "Each excerpt's node_coverage describes that window only, not prior reads: complete means it covers the entire node; unread_before_bytes and unread_after_bytes count node bytes outside this excerpt. null means the excerpt is not contained in one verified node. text_truncated=false does not imply complete node coverage.";
    let specs = [
        (
            "gptgrep_catalog",
            format!(
                "List indexed document paths and titles. Omit optional arguments for offset=0, limit={CATALOG_DEFAULT}. limit must be 1..{CATALOG_MAX}; use next_offset for pagination."
            ),
            json!({"offset":{"type":"integer","minimum":0,"default":0,"description":"Zero-based offset; default 0. Use the returned next_offset for another page."},"limit":{"type":"integer","minimum":1,"maximum":CATALOG_MAX,"default":CATALOG_DEFAULT,"description":format!("Number of documents: 1..{CATALOG_MAX}, default {CATALOG_DEFAULT}. Do not request a larger limit.")}}),
            vec![],
        ),
        (
            "gptgrep_tree",
            format!(
                "Expand a catalog-issued document path into verified tree nodes and key_items. Default offset=0, limit={TREE_DEFAULT}; limit must be 1..{TREE_MAX}. Use returned node_id for read and next_offset for pagination."
            ),
            json!({"path":{"type":"string","description":"Exact relative path returned by gptgrep_catalog."},"offset":{"type":"integer","minimum":0,"default":0,"description":"Zero-based offset; default 0 or returned next_offset."},"limit":{"type":"integer","minimum":1,"maximum":TREE_MAX,"default":TREE_DEFAULT,"description":format!("Number of nodes: 1..{TREE_MAX}, default {TREE_DEFAULT}. Additional pages use next_offset.")}}),
            vec!["path"],
        ),
        (
            "gptgrep_read",
            format!(
                "Read a node_id returned by tree/search. Omit max_bytes for {READ_DEFAULT}; max_bytes must be 1..{MAX_TEXT_BYTES}. offset_bytes defaults to 0. Continue with offset_bytes=next_offset until next_offset is null; preserve the same node_id. Offsets count canonical node UTF-8 bytes, not characters. EOF can leave a prefix outside this excerpt; prior reads may already cover it. If tool_output_limit occurs, retry the same offset with fewer bytes. {coverage}"
            ),
            json!({"node_id":{"type":"string","description":"Exact node_id returned by gptgrep_tree or gptgrep_search, or the selected summary node."},"max_bytes":{"type":"integer","minimum":1,"maximum":MAX_TEXT_BYTES,"default":READ_DEFAULT,"description":format!("UTF-8 excerpt byte budget: 1..{MAX_TEXT_BYTES}, default {READ_DEFAULT}. Values such as 10000 are invalid; omit this argument for the default.")},"offset_bytes":{"type":"integer","minimum":0,"default":0,"description":"Byte offset relative to this node's canonical text; default 0. For continuation use the returned next_offset exactly. To expand a search hit, its node_offset identifies that excerpt's start. Must be a UTF-8 boundary. A null next_offset means EOF."}}),
            vec!["node_id"],
        ),
        (
            "gptgrep_search",
            format!(
                "Search with Jev document routing and evidence reranking. Default mode=hybrid; semantic omits the lexical lane. Regex/lexical are explicit local refinements after the host's mandatory initial Jev pass. limit must be 1..{SEARCH_MAX}, default {SEARCH_MAX}. For more evidence, reformulate the query or inspect/read the document tree. {coverage}"
            ),
            json!({"query":{"type":"string","minLength":1,"maxLength":2048,"description":"Runtime search question, words or regex, 1..2048 UTF-8 bytes."},"mode":{"type":"string","enum":["hybrid","semantic","regex","lexical"],"default":"hybrid","description":"hybrid (default): lexical candidates plus Jev routing/reranking. semantic: Jev semantic lane. regex or lexical: explicit local refinement after required initial Jev work."},"document":{"type":"string","description":"Optional exact relative document path from the current catalog. Omission keeps the caller's scope. This cannot widen or replace a caller-supplied document scope."},"limit":{"type":"integer","minimum":1,"maximum":SEARCH_MAX,"default":SEARCH_MAX,"description":format!("Result count: 1..{SEARCH_MAX}, default {SEARCH_MAX}. This bounded evidence tool does not accept larger result counts.")}}),
            vec!["query"],
        ),
    ];
    let functions:Vec<_>=specs.into_iter().map(|(name,description,properties,required)|json!({
        "type":"function","name":name,"description":description,
        "inputSchema":{"type":"object","properties":properties,"required":required,"additionalProperties":false}
    })).collect();
    json!([{"type":"namespace","name":"gptgrep","description":"Direct bounded local document retrieval functions.","tools":functions}])
}

pub(crate) fn limits_guidance() -> String {
    format!(
        "Omit optional size arguments to use defaults: catalog limit={CATALOG_DEFAULT} (max {CATALOG_MAX}), tree limit={TREE_DEFAULT} (max {TREE_MAX}), search limit={SEARCH_MAX} (max {SEARCH_MAX}), read max_bytes={READ_DEFAULT} (max {MAX_TEXT_BYTES}), offset_bytes=0. Continue long reads with the returned next_offset; stop when it is null. Use pagination or another focused search instead of exceeding size limits."
    )
}

pub(crate) fn allowed(tool: &str) -> bool {
    matches!(
        tool,
        "gptgrep_catalog" | "gptgrep_tree" | "gptgrep_read" | "gptgrep_search"
    )
}

#[cfg(test)]
mod citation_code_tests {
    use super::citation_validation_code;

    #[test]
    fn citation_failure_telemetry_is_bounded_and_redacted() {
        assert_eq!(
            citation_validation_code(&anyhow::anyhow!("host_answer_citation_unissued")),
            "unissued_citation"
        );
        for code in ["host_answer_invalid_json", "host_answer_schema_mismatch"] {
            assert_eq!(
                citation_validation_code(&anyhow::anyhow!(code)),
                "final_response_schema"
            );
        }
        assert_eq!(
            citation_validation_code(&anyhow::anyhow!("Unrecognized source detail: private text")),
            "unclassified_citation_validation"
        );
    }
}
