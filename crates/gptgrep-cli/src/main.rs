use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand, ValueEnum};
use gptgrep_core::SearchOptions;
use serde_json::{Value, json};
use std::io::{self, Read, Write};
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(
    name = "gptgrep",
    version,
    about = "Jev-powered document routing and reranking, structural trees and exact grep",
    after_help = "Exit codes: 0 success/matches, 1 no matches, 2 error or excluded stale evidence.\nNo network is used by regex/lexical search. hybrid/semantic explicitly sends bounded evidence to Jev."
)]
struct Cli {
    #[arg(long, global = true, help = "Emit one machine-readable JSON object")]
    json: bool,
    #[arg(
        long,
        global = true,
        help = "Print the versioned CLI contract without executing a command"
    )]
    schema: bool,
    #[arg(long, global = true, help = "Print a compact agent usage manifest")]
    llms: bool,
    #[arg(
        long,
        global = true,
        value_name = "FILE",
        help = "Join an existing shared Decisions budget contract for paid operations"
    )]
    decisions_budget: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Clone, ValueEnum)]
enum Mode {
    Regex,
    Lexical,
    Hybrid,
    Semantic,
}

#[derive(Debug, Clone, ValueEnum)]
enum IndexOcr {
    Off,
    Auto,
}

#[derive(Debug, Args)]
struct RichIndexArgs {
    #[arg(
        long,
        help = "Explicitly publish source-bound v2 PDF/PNG/JPEG artifacts; default index retains v1"
    )]
    rich: bool,
    #[arg(
        long,
        value_enum,
        help = "Select v2 ingestion: off is local; auto requires the qualified native OCR runtime"
    )]
    ocr: Option<IndexOcr>,
    #[arg(
        long,
        help = "Prepare all native pages and OCR routing without model calls or CURRENT publication"
    )]
    plan_only: bool,
    #[arg(
        long,
        help = "Reuse only verified completed OCR results; unknown/failed tasks cannot be retried"
    )]
    resume: bool,
    #[arg(
        long,
        help = "Explicit immutable OCR run identity, required for live OCR"
    )]
    ocr_run_id: Option<String>,
    #[arg(
        long,
        help = "Operator-owned qualified OcrRuntimeProfile JSON; never an authentication file"
    )]
    ocr_runtime_profile: Option<PathBuf>,
    #[arg(
        long,
        default_value = "codex",
        help = "Pinned official Codex binary from the OCR runtime profile"
    )]
    ocr_codex_bin: String,
    #[arg(
        long,
        help = "Existing independent authenticated OCR home; no login or credential reads"
    )]
    ocr_codex_home: Option<PathBuf>,
    #[arg(long, default_value_t = 24)]
    ocr_max_pages: usize,
    #[arg(
        long,
        default_value_t = 1800,
        help = "One original OCR run deadline; resume adopts it unchanged"
    )]
    ocr_timeout_seconds: u64,
    #[arg(long, default_value_t = 25)]
    page_window: usize,
    #[arg(
        long,
        help = "Require the frozen rich-ingest plan digest before live execution"
    )]
    expected_plan_sha256: Option<String>,
}

#[derive(Debug, Args)]
struct HostArgs {
    #[arg(long, default_value = "codex")]
    codex_bin: String,
    #[arg(
        long,
        help = "Existing Codex account home; defaults to CODEX_HOME or ~/.codex"
    )]
    codex_home: Option<PathBuf>,
    #[arg(
        long,
        help = "Canonical existing native state directory with an existing logs child; account home stays unchanged"
    )]
    codex_state_dir: Option<PathBuf>,
    #[arg(long, default_value = "gpt-6-luna")]
    model: String,
    #[arg(long, default_value = "max")]
    reasoning_effort: String,
    #[arg(
        long,
        default_value = "fast",
        help = "Codex service tier; fast requests priority service"
    )]
    service_tier: String,
    #[arg(long, default_value_t = 180)]
    timeout: u64,
    #[arg(long, default_value_t = 12)]
    max_tool_calls: usize,
    #[arg(
        long,
        default_value_t = 262144,
        help = "Structured completion input byte budget, at most 1048576"
    )]
    max_input_bytes: usize,
    #[arg(
        long,
        help = "Create a private bounded protocol-method trace; contents/configuration are omitted"
    )]
    protocol_trace: Option<PathBuf>,
}

impl HostArgs {
    fn config(self) -> gptgrep_host::HostConfig {
        gptgrep_host::HostConfig {
            codex_bin: self.codex_bin,
            codex_home: self
                .codex_home
                .unwrap_or_else(|| gptgrep_host::HostConfig::default().codex_home),
            codex_state_dir: self.codex_state_dir,
            model: self.model,
            jev_model: None,
            decisions_execution: None,
            document: None,
            reasoning_effort: self.reasoning_effort,
            service_tier: self.service_tier,
            timeout_secs: self.timeout,
            max_tool_calls: self.max_tool_calls,
            max_input_bytes: self.max_input_bytes,
            trace_path: self.protocol_trace,
            query_plan: None,
            navigation: None,
            source_continuation: false,
        }
    }
}

/// Bounds for the explicit v2 text/visual workflow. Legacy options keep their
/// original defaults and are never silently repurposed for this workflow.
#[derive(Debug, Args)]
struct MultimodalArgs {
    #[arg(
        long,
        help = "Route and rerank source-bound v2 text and images; requires a rich generation"
    )]
    multimodal: bool,
    #[arg(long, requires = "multimodal", value_parser = clap::value_parser!(u64).range(1..=900))]
    multimodal_timeout: Option<u64>,
    #[arg(long, requires = "multimodal")]
    window_bytes: Option<usize>,
    #[arg(long, requires = "multimodal")]
    max_visual_candidates: Option<usize>,
    #[arg(long, requires = "multimodal")]
    max_concurrent_batches: Option<usize>,
    #[arg(long, requires = "multimodal")]
    max_output_bytes: Option<usize>,
    #[arg(long, requires = "multimodal")]
    max_metadata_bytes: Option<usize>,
    #[arg(long, requires = "multimodal")]
    max_image_bytes: Option<u64>,
    #[arg(long, requires = "multimodal")]
    max_image_pixels: Option<u64>,
}

impl MultimodalArgs {
    fn options(
        &self,
        query: &str,
        document: Option<String>,
        min_score: f64,
        shared: bool,
    ) -> Result<gptgrep_core::RichMultimodalSearchOptions> {
        let mut options = gptgrep_core::RichMultimodalSearchOptions {
            document,
            min_score,
            require_shared_ledger: shared,
            ..Default::default()
        };
        if let Some(value) = self.window_bytes {
            options.window_bytes = value;
        }
        if let Some(value) = self.max_visual_candidates {
            options.max_visual_candidates = value;
        }
        if let Some(value) = self.max_concurrent_batches {
            options.max_concurrent_batches = value;
        }
        if let Some(value) = self.max_output_bytes {
            options.max_output_bytes = value;
        }
        if let Some(value) = self.max_metadata_bytes {
            options.max_metadata_bytes = value;
        }
        if let Some(value) = self.max_image_bytes {
            options.resources.image_file_bytes = value;
        }
        if let Some(value) = self.max_image_pixels {
            options.resources.image_pixels = value;
        }
        if query.trim().is_empty()
            || query.len() > 8192
            || !(4..=2048).contains(&options.window_bytes)
            || !(1..=16).contains(&options.max_visual_candidates)
            || !(1..=4).contains(&options.max_concurrent_batches)
            || !(1..=1048576).contains(&options.max_output_bytes)
            || !(1..=524288).contains(&options.max_metadata_bytes)
            || !min_score.is_finite()
            || !(0.0..=1.0).contains(&min_score)
            || options
                .document
                .as_ref()
                .is_some_and(|d| d.is_empty() || d.len() > 8192)
        {
            bail!("multimodal query or candidate/output options exceed their bounds");
        }
        options.resources.validate()?;
        Ok(options)
    }
}

#[derive(Debug, Args)]
struct EnrichHostArgs {
    #[arg(long, default_value = "codex")]
    codex_bin: String,
    #[arg(
        long,
        help = "Existing Codex account home; defaults to CODEX_HOME or ~/.codex"
    )]
    codex_home: Option<PathBuf>,
    #[arg(
        long,
        help = "Canonical existing native state directory with an existing logs child; account home stays unchanged"
    )]
    codex_state_dir: Option<PathBuf>,
    #[arg(long, default_value = "gpt-6-luna")]
    builder_model: String,
    #[arg(long, default_value = "max")]
    reasoning_effort: String,
    #[arg(long, default_value = "fast")]
    service_tier: String,
    #[arg(long, default_value_t = 180)]
    timeout: u64,
    #[arg(long, default_value_t = 262144)]
    max_input_bytes: usize,
}

impl EnrichHostArgs {
    fn config(self) -> gptgrep_host::HostConfig {
        let mut config = gptgrep_host::HostConfig::default();
        config.codex_bin = self.codex_bin;
        config.codex_home = self.codex_home.unwrap_or(config.codex_home);
        config.codex_state_dir = self.codex_state_dir;
        config.model = self.builder_model;
        config.reasoning_effort = self.reasoning_effort;
        config.service_tier = self.service_tier;
        config.timeout_secs = self.timeout;
        config.max_input_bytes = self.max_input_bytes;
        config
    }
}

#[derive(Debug, Subcommand)]
enum OcrCommand {
    /// Explicit authenticated foreground loopback service; native CLI only.
    Serve {
        #[arg(
            long,
            value_name = "FILE",
            help = "Bounded operator-owned gptgrep.ocr.serve.v1 control snapshot"
        )]
        config: PathBuf,
        #[arg(
            long,
            default_value = "GPTGREP_OCR_SERVE_TOKEN",
            help = "Selected bearer token environment variable name; value is never printed or inherited by OCR children"
        )]
        token_env: String,
        #[arg(
            long,
            help = "Create only the supplied original-anchor ledger pair; existing state must match and is never reset"
        )]
        initialize_ledger: bool,
        #[arg(
            long,
            help = "Require this exact SHA256 of the explicitly selected control snapshot"
        )]
        expected_control_sha256: Option<String>,
    },
}

#[derive(Debug)]
struct OcrServeError(&'static str);
impl std::fmt::Display for OcrServeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}
impl std::error::Error for OcrServeError {}

#[derive(Debug, Subcommand)]
enum Command {
    /// Explicit foreground OCR; ordinary search never starts it.
    Ocr {
        #[command(subcommand)]
        command: OcrCommand,
    },
    /// Rerank exact source-bound v2 references; JSON output, no document routing.
    Rerank {
        #[arg(long, default_value = ".")]
        root: PathBuf,
        #[arg(long, default_value = "-")]
        input: PathBuf,
        #[arg(long)]
        model: Option<String>,
        #[arg(long, default_value_t = 180, value_parser = clap::value_parser!(u64).range(1..=900))]
        multimodal_timeout: u64,
    },
    /// Run bounded local operations against one retained generation; JSON output.
    Batch {
        #[arg(long, default_value = ".")]
        root: PathBuf,
        #[arg(long, default_value = "-")]
        input: PathBuf,
        #[arg(
            long,
            help = "Use v2 artifact operations and explicit paid search_multimodal"
        )]
        rich: bool,
        #[arg(
            long,
            requires = "rich",
            help = "Decisions model for explicit paid rich batch operations"
        )]
        model: Option<String>,
        #[arg(long, requires = "rich", default_value_t = 180, value_parser = clap::value_parser!(u64).range(1..=900))]
        multimodal_timeout: u64,
    },
    /// Parse documents and publish a complete immutable search generation.
    #[command(alias = "init")]
    Index {
        #[arg(default_value = ".")]
        root: PathBuf,
        #[arg(long, default_value_t = 100000)]
        max_files: usize,
        #[arg(
            long,
            help = "Apply PageIndex scan-cost merging to native paginated documents"
        )]
        optimize_merge: bool,
        #[command(flatten)]
        rich: RichIndexArgs,
    },
    /// Build source-bound, non-citable navigation hints over a published index.
    Enrich {
        #[arg(default_value = ".")]
        root: PathBuf,
        #[arg(long, help = "Absolute private append-only builder ledger path")]
        ledger_path: PathBuf,
        #[arg(
            long,
            help = "Resume only validated completed windows in the same ledger"
        )]
        resume: bool,
        #[arg(
            long,
            requires = "resume",
            conflicts_with = "plan_only",
            help = "Explicitly rebuild the window of this finished failed call as a NEW builder sample; at most two admissions per ledger, each adds one builder and one Jev allowance"
        )]
        rebuild_failed_window: Option<usize>,
        #[arg(
            long,
            default_value_t = 0,
            help = "Declared whole-ledger extra same-body Jev submissions (0..2) after typed communication failures; separate attempts with 250/500ms backoff; immutable on resume"
        )]
        #[arg(value_parser = parse_support_retries)]
        support_retries: usize,
        #[arg(long, help = "Require the zero-model plan digest before any live call")]
        expected_plan_sha256: Option<String>,
        #[arg(
            long,
            help = "Compute the complete raw-window plan without model calls"
        )]
        plan_only: bool,
        #[arg(
            long,
            requires = "plan_only",
            help = "Write the full immutable plan once"
        )]
        plan_output: Option<PathBuf>,
        #[arg(long, default_value_t = 8192)]
        window_bytes: usize,
        #[arg(long, default_value_t = 4)]
        max_hints_per_window: usize,
        #[arg(long, help = "Positive cumulative Codex builder-call cap")]
        max_builder_calls: usize,
        #[arg(long, help = "Positive cumulative Jev support-call cap")]
        max_jev_calls: usize,
        #[arg(long, default_value_t = 64 * 1024 * 1024)]
        max_ledger_bytes: u64,
        #[arg(long, default_value_t = 65536)]
        max_windows_per_run: usize,
        #[arg(long, default_value_t = 900)]
        deadline_seconds: u64,
        #[arg(
            long,
            help = "Jev Decisions model; default openai/gpt-6-luna-decisions"
        )]
        jev_model: Option<String>,
        #[command(flatten)]
        host: EnrichHostArgs,
    },
    /// Find evidence with Jev hybrid routing/reranking; explicit regex is local.
    Search {
        query: String,
        #[arg(default_value = ".")]
        root: PathBuf,
        #[arg(long, value_enum, default_value = "hybrid")]
        mode: Mode,
        #[arg(
            long,
            help = "Restrict retrieval to one exact indexed source-relative path"
        )]
        document: Option<String>,
        #[arg(short = 'n', long, default_value_t = 20, conflicts_with = "multimodal")]
        limit: usize,
        #[arg(short = 'C', long, default_value_t = 0, conflicts_with = "multimodal")]
        context: usize,
        #[arg(short = 'i', long, conflicts_with = "multimodal")]
        ignore_case: bool,
        #[arg(short = 'F', long, conflicts_with = "multimodal")]
        fixed_strings: bool,
        #[arg(long, default_value_t = 24, conflicts_with = "multimodal")]
        max_candidates: usize,
        #[arg(long, default_value_t = 32, conflicts_with = "multimodal")]
        routing_docs: usize,
        #[arg(
            long,
            help = "OpenRouter Decisions model; default openai/gpt-6-luna-decisions"
        )]
        model: Option<String>,
        #[arg(
            long,
            default_value_t = 0.5,
            help = "Model relevance rubric floor (0..1); not calibrated confidence"
        )]
        min_score: f64,
        #[command(flatten)]
        visual: MultimodalArgs,
    },
    /// Inspect document tree nodes and their source coordinates.
    Tree {
        file: PathBuf,
        #[arg(long, default_value = ".")]
        root: PathBuf,
    },
    /// Retrieve a selected document_id:node_id from a search/tree result.
    Read {
        node_id: String,
        #[arg(long, default_value = ".")]
        root: PathBuf,
        #[arg(long, default_value_t = 8192)]
        max_bytes: usize,
        #[arg(
            long,
            default_value_t = 0,
            help = "Byte offset within the node; copy next_offset to continue"
        )]
        offset: usize,
    },
    /// List indexed documents and identifiers.
    Files {
        #[arg(default_value = ".")]
        root: PathBuf,
    },
    /// Verify freshness of existing indexed source files.
    Status {
        #[arg(default_value = ".")]
        root: PathBuf,
    },
    /// Parse one source document without creating a search index.
    Parse { file: PathBuf },
    /// Submit an explicit typed Jev state/questions JSON file.
    Judge {
        #[arg(long)]
        input: PathBuf,
        #[arg(long)]
        model: Option<String>,
    },
    /// Run a bounded local Codex retrieval workflow and validate returned citations.
    Ask {
        question: String,
        #[arg(default_value = ".")]
        root: PathBuf,
        #[arg(
            long,
            help = "Jev Decisions model; default openai/gpt-6-luna-decisions"
        )]
        jev_model: Option<String>,
        #[arg(
            long,
            help = "Restrict retrieval to one exact indexed source-relative path"
        )]
        document: Option<String>,
        #[arg(
            long,
            help = "Experimental: one extra Luna query-planning turn before bounded parallel retrieval; Jev remains required"
        )]
        experimental_query_plan: bool,
        #[arg(long, requires_all = ["experimental_query_plan", "document"], help = "Experimental: at node EOF, verify unresolved claims through scoped tree/read continuation within existing budgets")]
        experimental_source_continuation: bool,
        #[arg(
            long,
            requires_all = ["experimental_query_plan", "document"],
            help = "Require one exact completed navigation overlay for scoped planned retrieval"
        )]
        navigation_overlay_sha256: Option<String>,
        #[arg(long, requires = "navigation_overlay_sha256")]
        navigation_max_jev_calls: Option<usize>,
        #[arg(long, requires = "navigation_overlay_sha256")]
        navigation_max_request_bytes: Option<usize>,
        #[arg(
            long,
            requires = "experimental_query_plan",
            help = "Model for the optional query planner; defaults to gpt-6-luna"
        )]
        planner_model: Option<String>,
        #[arg(
            long,
            requires = "experimental_query_plan",
            help = "Experimental: add source-bound Jev evidence-role judgments to planned retrieval"
        )]
        experimental_evidence_roles: bool,
        #[command(flatten)]
        visual: MultimodalArgs,
        #[arg(long, requires = "multimodal")]
        max_citations: Option<usize>,
        #[arg(long, requires = "multimodal")]
        min_score: Option<f64>,
        #[command(flatten)]
        host: HostArgs,
    },
    /// Summarize one verified node through the local Codex host.
    Summarize {
        node_id: String,
        #[arg(long, default_value = ".")]
        root: PathBuf,
        #[arg(
            long,
            help = "Jev Decisions model; default openai/gpt-6-luna-decisions"
        )]
        jev_model: Option<String>,
        #[arg(
            long,
            help = "Must match the selected node's indexed source-relative path"
        )]
        document: Option<String>,
        #[command(flatten)]
        host: HostArgs,
    },
    /// Local Codex JSON completion for explicit summary/indexing workflow adapters.
    HostComplete {
        #[arg(long)]
        input: PathBuf,
        #[command(flatten)]
        host: HostArgs,
    },
    /// Inspect local capabilities; never prints credentials or calls a model.
    Doctor,
    /// Print the stable CLI contract.
    Schema,
}

fn parse_support_retries(value: &str) -> std::result::Result<usize, String> {
    value
        .parse::<usize>()
        .ok()
        .filter(|value| *value <= 2)
        .ok_or_else(|| "support retries must be 0..2".into())
}

fn contract() -> Value {
    let mut contract = json!({
        "schema_version":"gptgrep.cli.v1", "name":"gptgrep", "version":env!("CARGO_PKG_VERSION"),
        "transport":"argv/stdout", "mcp":false, "default_output":"text", "machine_output":"--json",
        "exit_codes":{"0":"success or matches","1":"no matches","2":"error, stale evidence, or incomplete enrichment"},
        "commands":{
            "batch":{"usage":"gptgrep batch --root ROOT --input FILE_OR_DASH --json","effect":"local generation-pinned search/catalog/tree/read operations","network":false,"request_schema":"gptgrep.batch.request.v1","output_schema":"gptgrep.batch.response.v1","limits":{"input_bytes":1048576,"operations":64,"output_bytes":1048576},"output_order":"input operation order","model_operations":"hybrid/semantic return output_budget_insufficient before any provider work in this foundation","continuation":"deterministic cursors bind generation, scope and projection; no model continuation","output":"always JSON; exit2 for any item or global failure"},
            "index":{"usage":"gptgrep index ROOT [--ocr auto|off] [--rich] [--plan-only] --json","effect":"default legacy v1; explicit rich/ocr prepares source-bound v2 candidates and atomically publishes only complete documents","network":"none for plan-only/off; explicit auto uses qualified native OCR only for required source pages","options":{"ocr":{"enum":["off","auto"],"default":"legacy index when absent"},"plan-only":{"type":"boolean","effect":"local native parse/render; zero model calls and no CURRENT change"},"resume":{"type":"boolean","effect":"same immutable run; verified result reuse, no failed/unknown task retries or deadline renewal"},"ocr-run-id":{"type":"string","required":"live auto"},"ocr-runtime-profile":{"type":"path","required":"live auto; accepted image qualification required"},"ocr-codex-home":{"type":"path","required":"explicit independent existing home for live auto"},"ocr-max-pages":{"type":"integer","default":24,"minimum":1,"maximum":24},"ocr-timeout-seconds":{"type":"integer","default":1800,"minimum":1,"maximum":86400},"page-window":{"type":"integer","default":25,"minimum":1,"maximum":25}},"output_schema":"gptgrep.rich-ingest.v1 for explicit rich/ocr/plan-only; legacy gptgrep.v1 otherwise","limitations":"rich UTF-8 text/Markdown, PDF/PNG/baseline JPEG; Office requires a working LibreOffice converter; other image variants remain unsupported; v2 consumption requires rich reader"},
            "enrich":{"usage":"gptgrep enrich ROOT --ledger-path ABSOLUTE_PATH --max-builder-calls N --max-jev-calls N [--plan-only] [--expected-plan-sha256 SHA] --json","effect":"explicit raw-only Codex/Jev navigation-overlay build after deterministic index","model_assisted":true,"plan_only_model_calls":0,"defaults":{"builder_model":"gpt-6-luna","reasoning_effort":"max","service_tier":"fast","jev_model":gptgrep_jev::DEFAULT_MODEL,"window_bytes":8192,"max_hints_per_window":4},"output":"complete publishes a separate source-bound navigation overlay; incomplete/failed leaves the current index unchanged and exits 2","hint_authority":"non-citable navigation only; not full PageIndex Flash parity","resume":"same absolute ledger and bound plan; pending or failed calls are not automatically retried"},
            "search":{"usage":"gptgrep search QUERY ROOT --mode regex|lexical|hybrid|semantic --json","options":{"mode":{"type":"string","default":"hybrid","enum":["hybrid","semantic","regex","lexical"]},"document":{"type":"string","description":"Exact indexed source-relative path; scope applies before candidate limits"},"limit":{"type":"integer","default":20,"minimum":1,"maximum":1000},"context":{"type":"integer","default":0,"maximum":100},"max-candidates":{"type":"integer","default":24,"maximum":24},"routing-docs":{"type":"integer","default":32,"maximum":32},"ignore-case":{"type":"boolean"},"fixed-strings":{"type":"boolean"},"model":{"type":"string","default":gptgrep_jev::DEFAULT_MODEL},"min-score":{"type":"number","minimum":0,"maximum":1,"default":0.5}},"network":"required Jev for default hybrid and semantic; explicit regex/lexical primitives are local","credentials":"OPENROUTER_API_KEY environment only","output_schema":"gptgrep.v1"},
            "tree":{"usage":"gptgrep tree FILE --root ROOT --json","effect":"read"},
            "read":{"usage":"gptgrep read DOCUMENT_ID:NODE_ID --root ROOT --max-bytes 8192 --offset BYTES --json","effect":"read"},
            "files":{"usage":"gptgrep files ROOT --json","effect":"read"},
            "status":{"usage":"gptgrep status ROOT --json","scope":"existing indexed files; index discovers new files"},
            "parse":{"usage":"gptgrep parse FILE --json","effect":"local parser"},
            "judge":{"usage":"gptgrep judge --input FILE [--model MODEL] --json","input":{"state":"JSON","questions":"Choice/Noul/Score map"},"effect":"explicit remote Decisions call"},
            "ask":{"usage":"gptgrep ask QUESTION ROOT [--codex-home HOME] [--codex-bin codex] [--experimental-query-plan] [--planner-model MODEL] [--navigation-overlay-sha256 SHA --navigation-max-jev-calls N --navigation-max-request-bytes BYTES] [--experimental-evidence-roles] --json","effect":"required Jev routing/reranking followed by bounded local Codex reasoning","options":{"jev-model":{"type":"string","default":gptgrep_jev::DEFAULT_MODEL},"document":{"type":"string"},"navigation-overlay-sha256":{"type":"string","requires":["experimental-query-plan","document"],"effect":"require exact completed source-bound overlay and full Jev hint scan before planned retrieval","report_schema":"gptgrep.navigation-query.v1","citation_authority":false},"navigation-max-jev-calls":{"type":"integer","requires":"navigation-overlay-sha256","minimum":1,"maximum":128,"default":32},"navigation-max-request-bytes":{"type":"integer","requires":"navigation-overlay-sha256","minimum":1,"maximum":8388608,"default":2097152},"experimental-query-plan":{"type":"boolean","default":false,"effect":"one extra no-tools Luna planner turn; up to two alternate retrieval queries; bounded parallel routes and final Jev reranking against the original question","limits":{"alternate_queries":2,"routing_concurrency":2,"union_candidates":24,"planner_timeout_seconds":45},"accounting":"model_attempts reports planner and reader separately; legacy usage is reader-only; overall timeout is shared"},"planner-model":{"type":"string","default":"gpt-6-luna","requires":"experimental-query-plan","effect":"select the query-planner Codex model independently from the final reader"},"experimental-evidence-roles":{"type":"boolean","default":false,"requires":"experimental-query-plan","effect":"one additional Choice per union candidate in the same mandatory Jev decision; source-bound role ordering and continuation hints","limits":{"union_candidates":24,"final_questions":48,"complete_request_bytes":65536},"sufficiency":"unassessed","accounting":"extra judgments and tokens are measured within the existing request; discarded-candidate metadata stays private and is not citable"}},"defaults":{"model":"gpt-6-luna","reasoning_effort":"max","service_tier":"fast","timeout_seconds":180,"max_tool_calls":12}},
            "summarize":{"usage":"gptgrep summarize DOCUMENT_ID:NODE_ID --root ROOT [host options] --json","effect":"required Jev retrieval in the selected document, then model-written summary with issued evidence citations"},
            "host-complete":{"usage":"gptgrep host-complete --input FILE_OR_DASH [host options] --json","input":{"instructions":"string","state":"JSON","schema":"JSON Schema object"},"effect":"explicit schema-validated local Codex completion; no citation assertion","defaults":{"max_input_bytes":262144,"service_tier":"fast"},"hard_max_input_bytes":1048576},
            "doctor":{"usage":"gptgrep doctor --json","effect":"local capability probe"}
        },
        "search_output":{"fields":["schema_version","query","mode","document_scope","root","generation","index_used","source_fresh","minimum_relevance_score","hits","coverage","metrics","warnings"],"hit_fields":["path","node_id","title","line_start","line_end","page_start","page_end","match_line","match_column","byte_start","byte_end","column_start","node_offset","next_offset","node_coverage","coordinate_system","text","text_truncated","score","confidence","literal_anchor","source_sha256","source_fresh","citation"]},
        "bounds":{"source_file_bytes":67108864,"jev_candidates":24,"semantic_routing_documents":32},
        "limitations":["No full PageIndex Flash parity claim", "OCR requires explicit rich ingestion and an accepted native runtime profile", "Office conversion requires LibreOffice", "Semantic routing and evidence snippets have explicit budgets", "Generative synthesis requires explicit local Codex host mode", "Relevance floor is operational policy, not calibrated confidence; hybrid exact-token anchors are retained"]
    });
    contract["commands"]["ocr"] = json!({
        "usage":"gptgrep ocr serve --config CONTROL_JSON [--token-env ENV_NAME] [--expected-control-sha256 SHA] [--initialize-ledger]",
        "effect":"explicit foreground authenticated loopback HTTP service; qualified native OCR on admitted requests",
        "native_cli_only":true,"control_schema":"gptgrep.ocr.serve.v1","phase_schema":"gptgrep.ocr.service-phase.v1",
        "catalog_schema":"gptgrep.ocr.task-catalog.v1","default_bind":"127.0.0.1:8828",
        "routes":{"/v1/ocr/tasks":"exact catalog-bound OcrTask; paths restored only from server custody",
                  "/ocr":"separate PNG multipart image-only compatibility; no generation authority"},
        "limits":{"control_bytes":16384,"phase_bytes":16384,"task_bytes":131072,"catalog_bytes":8388608,
                  "compatibility_png_bytes":1048576,"multipart_overhead_bytes":16384,"response_bytes":262144,
                  "connections":8,"native_concurrency":2,"phase_admissions_max":1024,"page_seconds_max":180},
        "defaults":{"token_env":"GPTGREP_OCR_SERVE_TOKEN","native_model":"gpt-6-luna","reasoning_effort":"max","service_tier":"fast"},
        "qualification":"separate exact full-task and compatibility-mode/runtime policy qualification is required; requested identity is not observed served identity",
        "initialize_ledger":"create/join exact original-anchor namespace; never replace, reset, renew or retry unknown requests",
        "compatibility_confidence":"actual model-reported uncalibrated numeric confidence; whole-image region only; fresh mode qualification required",
        "output":"bounded JSON preparation/stopped lifecycle; errors always structured, token and private paths omitted",
        "shutdown":"stop admissions and await owned native stop/reap/drain; unknown remote outcome stays charged/quarantined",
        "limitations":"listener/native/image/compatibility qualification and OS/RSS/cancellation guarantees remain pending"
    });
    contract["commands"]["enrich"]["options"] = json!({"rebuild-failed-window":{
        "type":"integer","requires":"resume","conflicts_with":"plan-only",
        "meaning":"Last finished failed builder/support call ID; rebuild its uncommitted raw window as a NEW builder sample and new support request",
        "maximum_admissions_per_ledger":2,"additional_allowance_per_admission":{"builder_calls":1,"jev_calls":1},
        "preserves":"Completed windows, original binding/caps, all prior attempts and unknown billing; append-only lineage",
        "ineligible":"Pending calls, successful uncommitted calls, validated complete windows, changed source/profile, known validation/policy failures",
        "legacy_failure_classification":"legacy_unclassified; elapsed time is not used to infer a cause"
    }});
    contract["commands"]["enrich"]["options"]["support-retries"] = json!({
        "type":"integer","default":0,"minimum":0,"maximum":2,"scope":"extra same-request Jev submissions across the entire bound ledger",
        "eligible":"typed timeout, transport, or HTTP 408/429/5xx only; never auth, content/validation, policy or unclassified failures",
        "backoff_ms":[250,500],"deadline":"original invocation deadline; never reset",
        "allowance":"declared additive Jev-only allowance; original call caps retained; each extra submission durably reserved and finished",
        "binding":"part of new plans; resume must retain the same value; legacy plans remain zero",
        "billing":"failed attempts and observed/unknown provider usage remain recorded; no inference of zero billing"
    });
    contract["commands"]["batch"]["limits"]["minimum_output_bytes"] =
        json!("4096 + 1024 * operation_count; includes complete response, no trailing newline");
    contract["commands"]["batch"]["invalid_request_diagnostics"] = json!({
        "max_bytes":4096,
        "scope":"invalid declarations and input I/O errors use the batch response schema; their cap is separate from an unadmitted requested output allowance"
    });
    contract["commands"]["batch"]["rich"] = json!({
        "usage":"gptgrep batch --rich --root ROOT --input FILE_OR_DASH",
        "request_schema":"gptgrep.batch.rich.request.v1",
        "output_schema":"gptgrep.batch.rich.response.v1",
        "operations":["inspect_documents","inspect_pages","inspect_native_metadata","read_artifact","search_artifacts","inspect_visual","read_visual","search_multimodal","rerank"],
        "generation":"CURRENT is resolved once; an exact generation and manifest SHA may be supplied",
        "limits":{"input_bytes":1048576,"operations":64,"output_bytes":1048576,"minimum_output_bytes":"4096 + 1536 * operation_count"},
        "inspection":"inspect_documents/inspect_pages remain indexed_metadata_only; no citation authority",
        "inspect_native_metadata":{
            "artifact_id":"page NativeMetadata ID returned by inspect_pages; document-wide metadata and text IDs are rejected",
            "fields":gptgrep_core::NATIVE_METADATA_SCALAR_FIELDS,
            "collections":{"object_summaries":gptgrep_core::NATIVE_OBJECT_FIELDS,"path_segments":gptgrep_core::NATIVE_SEGMENT_FIELDS,"image_geometry":gptgrep_core::NATIVE_IMAGE_FIELDS,"native_characters":gptgrep_core::NATIVE_CHARACTER_FIELDS,"native_words":gptgrep_core::NATIVE_WORD_FIELDS},
            "item_fields":"closed selected fields for chosen collection; empty selects all row fields",
            "object_id":"required only for path_segments; stable object ID returned by object_summaries",
            "page_size":{"minimum":1,"maximum":1000,"default":100},
            "max_output_bytes":{"minimum":1,"maximum":1048576,"default":1048576},
            "cursor":"coverage.cursor binds exact generation, artifact SHA, source/prepared/profile/recipe, document/page, collection/object, normalized fields, page_size/max_output_bytes and row ordinal",
            "output":"gptgrep.native_metadata.inspection.v1; mandatory identity, selected whole scalar objects and rows; metadata_only=true, citation_authority=false; freshly verifies source/prepared/artifact",
            "admission":"4MiB maximum artifact; aggregate snapshot and owned response leases within256MiB; complete next row, scalars and cursor must fit reserved item share before JSON allocation; first-unfit is output_budget_insufficient",
            "compatibility":"v7 complete graphics; v6 characters/words; unavailable geometry is explicit rebuild_required; no legacy silent upgrade",
            "model_required":false
        },
        "search_continuation":"generation/scope/query/options-bound cursor over a bounded max_hits window; inspect search_ceiling_reached and exhausted",
        "read_metrics":"partial reader API bytes; excludes final-wire delivery and complete physical I/O",
        "inspect_visual":{"scope":"document_ids/first_page/last_page; default all fixed-pin documents","query":"opaque query bound into continuation; no model scoring","max_candidates":"1..256, default16","cursor":"native visual continuation returned in result.next_cursor"},
        "read_visual":{"rendition_id":"ID returned by inspect_visual","output":"complete candidate and image_base64; no offset, byte window or cursor; never truncates image bytes"},
        "visual_continuation":"binds exact generation, scope, query, max_candidates, stored resource budget and derived item output reservation; retain batch output budget, operation count and reserved share across continuation",
        "visual_admission":"complete image/base64 and enclosing item must fit its fixed reservation before rendition byte reads; no borrowing from other operations; strict_cache_only refuses visual operations because no rendition cache exists",
        "visual_authority":"local observations only; actual_image_delivery=false and citation_authority=false; no native/model delivery or citation proof",
        "network":"only explicit search_multimodal and rerank invoke OpenRouter Decisions; local operations remain offline",
        "model_submissions":"local:0; paid:null when exact batch attribution is unavailable; scheduled_logical_attempts and shared context snapshots are separate",
        "search_multimodal":{"query":"nonempty UTF8 up to8192bytes","document":"optional exact source-relative path","window_bytes":"4..2048, default1024","max_visual_candidates":"1..16, default8","max_concurrent_batches":"1..4, default4","min_score":"0..1, default0.5","effect":"mandatory document routing then union text/independent visual rerank; no paid cursor or local fallback","admission":"all item output and worst-case call shares prepared before first submit; fixed shares never borrowed; one retained reader/context/original deadline","telemetry":"complete hit identities; bounded numeric usage projection retains full usage SHA, cost is lossy finite_f64 and is not an invoice","failure":"required paid failure retains partial observations and stops later paid operations; independent local items may continue"},
        "rerank":{"query":"nonempty UTF8 up to8192bytes","candidates":"1..64 unique {id,evidence}; exact pinned native/OCR text <=2048bytes or up to16 visual source refs","max_concurrent_batches":"1..4, default4","effect":"rerank this supplied pool only; no document routing, caller content, dependency, cursor or reader citation rights","admission":"whole output and call share prepared before any batch submit"},
        "paid_budget":{"max_decisions_submissions":"0..200, default8; whole batch worst-case call allowance also bounded by parent context","retained_preparation_bytes":268435456,"output":"same1MiB total, fixed per-item complete reservations; insufficient paid share fails before submit"},
        "output":"always JSON without a trailing newline; exit2 for global or item failure/not_executed"
    });
    contract["commands"]["rerank"] = json!({
        "usage":"gptgrep rerank --root ROOT --input FILE_OR_DASH --json",
        "request_schema":"gptgrep.rich_rerank.request.v1","output_schema":"gptgrep.rich_rerank.response.v1",
        "input":"query and1..64unique {id,evidence} resolved from one CURRENT/exact rich generation; no caller text, image bytes or URLs",
        "limits":{"input_bytes":1048576,"output_bytes":1048576,"text_window_bytes":2048,"visual_candidates":16,"max_concurrent_batches":4},
        "network":"explicit Luna Decisions; same original finite context; no routing/retry/fallback",
        "output":"always bounded JSON; all supplied refs scored or typed failure; no inference cursor/initial routing pass/citation rights",
        "options":{"model":"supported Luna Decisions profile","multimodal-timeout":"1..900seconds, default180; narrows existing deadline"}
    });
    contract["commands"]["ask"]["options"]["experimental-source-continuation"] = json!({
        "type":"boolean","default":false,"requires":["experimental-query-plan","document"],
        "effect":"Reader guidance: node EOF is not evidence completeness; verify unresolved claims/referents using scoped verified tree/read nodes",
        "budgets":"existing tool-call, deadline and output budgets; unresolved requirements at exhaustion yield insufficiency",
        "citation_authority":"only issued source spans; no inferred unseen text or scope expansion",
        "report_schema":"gptgrep.source-continuation.v1","additional_model_calls":0
    });
    contract["global_options"] = json!({"decisions-budget": {
        "type":"file", "optional":true, "schema":"gptgrep.decisions-budget-contract.v1",
        "control_bytes":16384, "effect":"join existing shared original-clock ledger for search hybrid/semantic, judge, ask, summarize and non-plan enrich",
        "initialization":false, "missing_files":"error", "without_file":"single-invocation bounded context",
        "local_commands":"regex/lexical, schema, doctor and enrich --plan-only do not open it"
    }});
    contract["decisions"] = json!({"default_model":gptgrep_jev::DEFAULT_MODEL,
        "legacy_model":gptgrep_jev::LEGACY_MODEL, "legacy_images":false,
        "model_admission":"exact requested identifier or the observed canonical mapping for Luna Decisions and legacy Jev; no other aliases or fallback",
        "typed_wire":"flat literal text plus PNG/JPEG image_url data URLs; local source bindings excluded",
        "limits":{"text_envelope_bytes":gptgrep_jev::MAX_REQUEST_BYTES,"body_bytes":gptgrep_jev::MAX_MULTIMODAL_REQUEST_BYTES,
            "images":gptgrep_jev::MAX_IMAGES,"image_file_bytes":gptgrep_jev::MAX_IMAGE_BYTES,"response_bytes":gptgrep_jev::MAX_RESPONSE_BYTES}});
    contract["commands"]["judge"]["input"] = json!({"one_of":[
        {"state":"JSON","questions":"Choice/Noul/Score map"},
        {"input":{"text":[{"source":{"source_id":"string","unit_id":"string","source_sha256":"sha256"},"text":"literal string"}],
            "images":[{"source":{"source_id":"string","unit_id":"string","source_sha256":"sha256"},"path":"local PNG/JPEG path"}]},
        "questions":"Choice/Noul/Score map"}],"control_bytes":65536});
    let multimodal = json!({"type":"boolean","default":false,
        "generation":"v2 only; CURRENT resolved once and retained",
        "routing":"mandatory document routing plus text/independent visual union reranking",
        "decisions_model":gptgrep_jev::DEFAULT_MODEL,
        "limits":{"query_utf8_bytes":8192,"text_candidates":24,"routing_documents":8,
            "visual_candidates":16,"concurrent_batches":4,"images_per_request":4,
            "request_image_pixels":24000000,"request_bytes":8388608},
        "coverage":"bounded candidate pool; inspect omitted pages/artifacts and truncation; not exhaustive semantic recall",
        "search_schema":"gptgrep.rich_multimodal_search.v1","ask_schema":"gptgrep.rich_multimodal.host.v1",
        "output":"one complete typed JSON report, including without --json",
        "qualification":"implementation with offline mock checks; actual model image influence/native runtime qualification pending"});
    for command in ["search", "ask"] {
        contract["commands"][command]["options"]["multimodal"] = multimodal.clone();
        for (name, minimum, maximum, default) in [
            ("multimodal-timeout", 1, 900, 180),
            ("window-bytes", 4, 2048, 1024),
            ("max-visual-candidates", 1, 16, 8),
            ("max-concurrent-batches", 1, 4, 4),
            ("max-output-bytes", 1, 1048576, 1048576),
            ("max-metadata-bytes", 1, 524288, 524288),
            ("max-image-bytes", 1, 1048576, 1048576),
            ("max-image-pixels", 1, 8000000, 8000000),
        ] {
            contract["commands"][command]["options"][name] = json!({"type":"integer","minimum":minimum,"maximum":maximum,"default":default,"requires":"multimodal"});
        }
    }
    contract["commands"]["search"]["options"]["multimodal"]["conflicts"] = json!([
        "mode other than hybrid",
        "limit",
        "context",
        "ignore-case",
        "fixed-strings",
        "max-candidates",
        "routing-docs"
    ]);
    contract["commands"]["search"]["options"]["multimodal"]["citation_authority"] = json!(false);
    contract["commands"]["ask"]["options"]["multimodal"]["conflicts"] = json!([
        "experimental-query-plan",
        "experimental-source-continuation",
        "experimental-evidence-roles",
        "navigation options",
        "planner-model",
        "native profile other than gpt-6-luna/max/fast"
    ]);
    contract["commands"]["ask"]["options"]["multimodal"]["citation_authority"] = json!(
        "only a complete actual reader RPC write+flush and fresh source/coordinate binding; Decisions image exposure grants none"
    );
    contract["commands"]["ask"]["options"]["max-citations"] =
        json!({"type":"integer","minimum":1,"maximum":8,"default":8,"requires":"multimodal"});
    contract["commands"]["ask"]["options"]["min-score"] =
        json!({"type":"number","minimum":0,"maximum":1,"default":0.5,"requires":"multimodal"});
    contract["commands"]["ask"]["options"]["multimodal"]["host_bounds"] = json!({"rerank_report_bytes":524288,"candidate_metadata_bytes":32768,"bootstrap_candidate_metadata_bytes":524288,"image_result_frame_bytes":1048576,"complete_output_reserved_before_calls":true});
    for command in ["ask", "summarize", "host-complete", "enrich"] {
        contract["commands"][command]["options"]["codex-state-dir"] = json!({
            "type":"path","default":null,
            "validation":"canonical absolute existing directory; its logs child must also be an existing canonical directory; symlinks and missing/non-directory paths are rejected before spawn",
            "effect":"explicit sqlite_home=DIR and log_dir=DIR/logs; codex-home authentication/profile routing is unchanged",
            "omitted":"original sqlite_home binding to codex-home and original log configuration; no implicit directory creation or fallback"
        });
    }
    contract
}

fn llms() -> &'static str {
    "# GPTgrep\n\nLocal vectorless retrieval helper for agents. No MCP server.\n\n1. gptgrep index ./docs --json\n1r. gptgrep index ./documents --ocr auto --plan-only --json\n1n. gptgrep index ./native-documents --ocr off --json\n1a. gptgrep enrich ./docs --ledger-path /path/to/private/enrich.jsonl --max-builder-calls 100 --max-jev-calls 100 --plan-only --json\n2. gptgrep search 'concept in natural language' ./docs --json\n3. gptgrep search 'pattern' ./docs --mode regex --json\n4. gptgrep tree manual.pdf --root ./docs --json\n5. gptgrep read DOCUMENT_ID:NODE_ID --root ./docs --json\n6. gptgrep batch --root ./docs --input operations.json --json\n7. gptgrep batch --rich --root ./docs --input rich-operations.json\n\nBatch accepts gptgrep.batch.request.v1 JSON, pins one generation and preserves operation order. Local search/catalog/tree/read operations share bounded output reservations. Batch hybrid/semantic are currently rejected before provider work; inspect each item status and coverage.\n\nFor a separately published v2 generation, batch --rich accepts gptgrep.batch.rich.request.v1 with inspect_documents, inspect_pages, inspect_native_metadata, read_artifact, search_artifacts, inspect_visual, read_visual and explicit search_multimodal. Document/page enumeration returns indexed metadata only. inspect_native_metadata takes a page metadata artifact_id, closed fields and item_fields, collection, optional path object_id, page_size, max_output_bytes and coverage.cursor. It returns verified structural geometry/characters/words with mandatory source and artifact identities; it is metadata_only and never citation authority. Object summaries and selected path segments paginate independently; old geometry is rebuild_required and an indivisible first row that cannot fit is output_budget_insufficient. Text read/search preserve typed native/OCR evidence; their cursors bind generation and operation options. Search pages cover a bounded max_hits window; inspect truncation and exhaustion. Opt-in inspect_visual takes scope (document_ids, first_page, last_page), query, max_candidates and its own cursor; use result.next_cursor and retain the batch output budget and per-item reservation when continuing. read_visual takes rendition_id and returns complete image_base64 plus pinned candidate metadata; no image byte windows, truncation or cursor. Both visual operations reuse the generation resource budget and report actual_image_delivery=false and citation_authority=false. They establish local observations, not model delivery or citation rights. The seven deterministic operations remain local and perform zero model submissions. search_multimodal requires mandatory document routing then source-bound text/visual reranking on the same finite Decisions context. All complete item output shares and worst-case call shares are prepared before the first submit. budget.max_decisions_submissions defaults to8 (hard cap200); no operation borrows a later share. Paid results have no inference cursor, preserve partial attempt receipts, and grant no reader citation rights. Required paid failure stops later paid items while independent local items may continue. model_submissions is null when exact batch attribution is unavailable; scheduled attempts and shared context snapshots are separate. Numeric cost telemetry is a labelled lossy finite_f64 projection with full usage SHA; missing cost remains null. Explicit rerank --root ROOT --input FILE_OR_DASH accepts gptgrep.rich_rerank.request.v1 with query and unique {id,evidence} candidates. It resolves exact pinned native/OCR text windows and visual refs, scores only that supplied pool using the same rubric, and creates neither routing-pass nor citation rights. The equivalent rich batch rerank operation shares the same prepaid output/call reservations. Caller text/image bytes/URLs, implicit dependencies and inference cursors are rejected.\n\nJev routing/reranking is required by default search, ask and summarize. OPENROUTER_API_KEY must be supplied for model-assisted runs; failures never silently downgrade to local retrieval. Explicit regex/lexical and enrich --plan-only remain offline. Enrich is an explicit Codex/Jev step that creates non-citable navigation hints after local indexing. Use --document RELATIVE_PATH to scope before candidate budgets. The default Decisions model is openai/gpt-6-luna-decisions; native Codex defaults separately to gpt-6-luna/max/fast. Hybrid/semantic and judge send bounded data to OpenRouter Decisions. Judge input accepts state/questions or typed input.text/input.images with explicit source bindings and local PNG/JPEG paths. --decisions-budget FILE joins an existing original-clock shared budget; it never initializes or resets a ledger. Without the file, paid calls share their single-invocation bounded client context. Explicit --model typesafe/jev-1.13 remains text-only. Inspect coverage, source_fresh, coordinate_system and text_truncated before citing evidence. The main agent performs reasoning and controls follow-up reads. Use search QUERY ROOT --multimodal or ask QUESTION ROOT --multimodal on a published v2 rich generation. Both retain one generation and one finite Decisions context, perform mandatory document routing, and rerank a union of text and independent visual candidates. Scope with --document PATH; bound --window-bytes, --max-visual-candidates, --max-concurrent-batches, --max-image-bytes, --max-image-pixels, --max-metadata-bytes and --max-output-bytes. --multimodal-timeout limits the same invocation and never extends an existing shared deadline. Search returns gptgrep.rich_multimodal_search.v1 JSON with model-exposure-only evidence and no citation rights. Ask uses native gpt-6-luna/max/fast, accepts --max-citations 1..8, and issues typed text/image citations only after actual complete reader delivery and freshness checks. Inspect coverage/truncation and actual observations; implementation and offline mocks do not qualify actual image influence/native runtime. No legacy generation, regex/lexical, grep-window or planning fallback is allowed. Foreground OCR is native-CLI-only: gptgrep ocr serve --config CONTROL_JSON --token-env GPTGREP_OCR_SERVE_TOKEN. It uses explicit server-held task custody, qualified native gpt-6-luna/max/fast, a finite original-anchor phase and authenticated loopback only. The optional --initialize-ledger creates or joins the exact namespace without reset or renewal. /ocr is separately qualified PNG multipart compatibility with model-reported uncalibrated confidence and no generation authority. Ordinary queries never start OCR; listener/native qualification remains pending. Rich ingestion supports lossless UTF-8 text/Markdown, PDF, PNG and restricted baseline JPEG. Office uses one bounded LibreOffice conversion; a working converter is required and real Office runtime qualification remains pending. The full JPEG family remains unsupported. New files require reindexing. --schema provides the command contract.\n"
}

fn emit(value: &Value) -> Result<()> {
    let mut stdout = io::stdout().lock();
    serde_json::to_writer(&mut stdout, value)?;
    writeln!(stdout)?;
    Ok(())
}

fn program_available(name: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|paths| std::env::split_paths(&paths).any(|p| p.join(name).is_file()))
}

fn decisions_context(
    path: Option<&std::path::Path>,
) -> Result<Option<gptgrep_jev::ExecutionContext>> {
    path.map(gptgrep_jev::ExecutionContext::from_budget_file)
        .transpose()
}

fn multimodal_client(
    reader: &gptgrep_core::RichGenerationReader,
    model: Option<&str>,
    budget: Option<&std::path::Path>,
    timeout: u64,
) -> Result<gptgrep_jev::JevClient> {
    let selected = model.unwrap_or(gptgrep_jev::DEFAULT_MODEL);
    if !matches!(
        selected,
        gptgrep_jev::DEFAULT_MODEL | "openai/gpt-6-luna-decisions-20261006"
    ) {
        bail!("multimodal retrieval requires the supported Luna Decisions image profile");
    }
    if !(1..=900).contains(&timeout) {
        bail!("multimodal timeout must be 1 to 900 seconds");
    }
    let execution = match decisions_context(budget)? {
        Some(context) => context,
        None => gptgrep_jev::ExecutionContext::new(
            "gptgrep.cli.multimodal.invocation",
            &reader.pin().manifest_sha256,
            std::time::SystemTime::now() + std::time::Duration::from_secs(timeout),
            gptgrep_jev::MAX_SUBMISSIONS,
        )?,
    };
    // The existing shared deadline is never extended. Search's explicit
    // invocation cap also narrows shared contexts, without changing their ledger.
    let execution = execution
        .with_deadline(std::time::Instant::now() + std::time::Duration::from_secs(timeout))?;
    Ok(gptgrep_jev::JevClient::from_env(Some(selected))?.with_execution_context(execution))
}

fn require_keys(value: &Value, allowed: &[&str]) -> Result<()> {
    let object = value
        .as_object()
        .context("judge input requires an object")?;
    if object.keys().any(|key| !allowed.contains(&key.as_str())) {
        bail!("judge input contains an unknown field");
    }
    Ok(())
}

/// Admit either the original state/questions contract or typed local image evidence.
fn prepare_judge(
    client: &gptgrep_jev::JevClient,
    data: &Value,
) -> Result<gptgrep_jev::PreparedDecision> {
    let questions = data
        .get("questions")
        .context("judge input requires questions")?
        .clone();
    if let Some(input) = data.get("input") {
        require_keys(data, &["input", "questions"])?;
        require_keys(input, &["text", "images"])?;
        let mut evidence = gptgrep_jev::DecisionInput::default();
        if let Some(text) = input.get("text") {
            let items = text
                .as_array()
                .context("judge text evidence requires an array")?;
            for item in items {
                require_keys(item, &["source", "text"])?;
                evidence.text.push(gptgrep_jev::TextEvidence {
                    source: serde_json::from_value(
                        item.get("source")
                            .context("judge evidence requires source")?
                            .clone(),
                    )?,
                    text: item
                        .get("text")
                        .and_then(Value::as_str)
                        .context("judge evidence requires literal text")?
                        .into(),
                });
            }
        }
        if let Some(images) = input.get("images") {
            let items = images
                .as_array()
                .context("judge image evidence requires an array")?;
            if items.len() > gptgrep_jev::MAX_IMAGES {
                bail!("judge image count exceeds the limit");
            }
            for item in items {
                require_keys(item, &["source", "path"])?;
                let source = serde_json::from_value(
                    item.get("source")
                        .context("judge evidence requires source")?
                        .clone(),
                )?;
                let path = item
                    .get("path")
                    .and_then(Value::as_str)
                    .context("judge image evidence requires path")?;
                evidence.images.push(gptgrep_jev::ImageEvidence::from_file(
                    source,
                    std::path::Path::new(path),
                )?);
            }
        }
        client.prepare_input(evidence, questions, gptgrep_jev::MAX_RESPONSE_BYTES)
    } else {
        let state = data
            .get("state")
            .context("judge input requires state or input")?
            .clone();
        client.prepare_decision(state, questions)
    }
}

/// One fixed host root per task; all executors share the existing two-page
/// context, original clock and persistent ledger. No alternate model route.
struct RichOcrExecutor {
    pages: std::collections::BTreeMap<String, gptgrep_host::OcrHostExecutor>,
}
impl gptgrep_parse::OcrExecutor for RichOcrExecutor {
    fn execute<'a>(
        &'a self,
        task: gptgrep_parse::OcrTask,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<gptgrep_parse::OcrPageResult>> + Send + 'a>,
    > {
        Box::pin(async move {
            let page = self
                .pages
                .get(&task.task_sha256)
                .context("unplanned rich OCR task")?;
            gptgrep_parse::OcrExecutor::execute(page, task).await
        })
    }
}

async fn run_rich_index(
    root: &std::path::Path,
    max_files: usize,
    args: RichIndexArgs,
) -> Result<i32> {
    let mode = if matches!(args.ocr, Some(IndexOcr::Auto)) {
        gptgrep_parse::OcrMode::Auto
    } else {
        gptgrep_parse::OcrMode::Off
    };
    if args.resume && args.ocr_run_id.is_none() {
        bail!("--resume requires --ocr-run-id");
    }
    let profile = if let Some(path) = &args.ocr_runtime_profile {
        let bytes = gptgrep_core::read_rich_runtime_descriptor(path)?;
        let profile: gptgrep_host::OcrRuntimeProfile = serde_json::from_slice(&bytes)
            .map_err(|_| anyhow::anyhow!("invalid OCR runtime profile"))?;
        profile.validate()?;
        Some(profile)
    } else {
        None
    };
    let mut options = gptgrep_core::RichIngestOptions {
        max_files,
        mode,
        max_ocr_pages: args.ocr_max_pages,
        run_id: args.ocr_run_id.clone(),
        resume: args.resume,
        runtime_profile_sha256: profile.as_ref().map(|p| p.sha256()).transpose()?,
        ..Default::default()
    };
    options.native.page_window = args.page_window;
    let ingest = gptgrep_core::prepare_rich_ingest(root, options).await?;
    if args
        .expected_plan_sha256
        .as_ref()
        .is_some_and(|sha| sha != &ingest.report().plan_sha256)
    {
        bail!("rich ingest plan digest differs from --expected-plan-sha256");
    }
    if args.plan_only || !ingest.report().blockers.is_empty() {
        emit(&serde_json::to_value(ingest.report())?)?;
        return Ok(if ingest.report().blockers.is_empty() {
            0
        } else {
            2
        });
    }
    if mode == gptgrep_parse::OcrMode::Auto && ingest.needs_ocr_executor() {
        let profile = profile
            .context("live OCR requires --ocr-runtime-profile with accepted image qualification")?;
        if profile.accepted_image_qualification_sha256.is_none() {
            bail!("ocr_runtime_image_qualification_required");
        }
        let home = args
            .ocr_codex_home
            .context("live OCR requires an explicit independent --ocr-codex-home")?;
        let state = ingest.state_dir()?;
        let clock = ingest.ocr_clock(args.ocr_timeout_seconds)?;
        let context = gptgrep_host::OcrExecutionContext::with_clock_anchor(
            args.ocr_run_id
                .as_deref()
                .context("live OCR run ID required")?,
            &ingest.report().plan_sha256,
            clock,
            args.ocr_max_pages,
        )?
        .with_ledger(state.join("budget.json"))?;
        let raw = state.join("raw");
        if raw.is_symlink() {
            bail!("OCR raw result directory cannot be a symlink");
        }
        std::fs::create_dir_all(&raw)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&raw, std::fs::Permissions::from_mode(0o700))?;
        }
        let mut pages = std::collections::BTreeMap::new();
        for (hash, prepared_root) in ingest.ocr_staging_roots() {
            pages.insert(
                hash,
                gptgrep_host::OcrHostExecutor::new(
                    gptgrep_host::OcrHostConfig {
                        codex_bin: args.ocr_codex_bin.clone(),
                        codex_home: home.clone(),
                        prepared_root,
                        raw_results_dir: raw.clone(),
                        runtime_profile: profile.clone(),
                        transport: gptgrep_host::OcrImageTransport::DynamicImages,
                    },
                    context.clone(),
                )?,
            );
        }
        let executor = RichOcrExecutor { pages };
        let report = ingest.publish(Some(&executor)).await;
        let ok = report.status == "published" && report.failure_code.is_none();
        emit(&serde_json::to_value(report)?)?;
        Ok(if ok { 0 } else { 2 })
    } else {
        let report = ingest.publish(None).await;
        let ok = report.status == "published" && report.failure_code.is_none();
        emit(&serde_json::to_value(report)?)?;
        Ok(if ok { 0 } else { 2 })
    }
}

async fn run(cli: Cli) -> Result<i32> {
    if cli.schema {
        emit(&contract())?;
        return Ok(0);
    }
    if cli.llms {
        print!("{}", llms());
        return Ok(0);
    }
    let Some(command) = cli.command else {
        print!("{}", llms());
        return Ok(0);
    };
    match command {
        Command::Ocr { command } => {
            if cli.decisions_budget.is_some() {
                return Err(OcrServeError("ocr_serve_incompatible_decisions_budget").into());
            }
            return run_ocr_serve(command).await;
        }
        Command::Rerank {
            root,
            input,
            model,
            multimodal_timeout,
        } => {
            let mut bytes = Vec::with_capacity(8192);
            let read_result = (|| -> io::Result<()> {
                if input.as_os_str() == "-" {
                    io::stdin().lock().take(1_048_577).read_to_end(&mut bytes)?;
                } else {
                    std::fs::File::open(&input)?
                        .take(1_048_577)
                        .read_to_end(&mut bytes)?;
                }
                Ok(())
            })();
            if read_result.is_err() {
                serde_json::to_writer(
                    io::stdout().lock(),
                    &gptgrep_core::rich_rerank_input_io_failure(),
                )?;
                return Ok(2);
            }
            let request = match gptgrep_core::parse_rich_rerank_request(&bytes) {
                Ok(request) => request,
                Err(e) => {
                    serde_json::to_writer(io::stdout().lock(), &e)?;
                    return Ok(2);
                }
            };
            let reader = match gptgrep_core::open_rich_rerank_reader(&root, &request) {
                Ok(reader) => reader,
                Err(e) => {
                    serde_json::to_writer(io::stdout().lock(), &e)?;
                    return Ok(2);
                }
            };
            let client = match multimodal_client(
                &reader,
                model.as_deref(),
                cli.decisions_budget.as_deref(),
                multimodal_timeout,
            ) {
                Ok(client) => client,
                Err(_) => {
                    serde_json::to_writer(
                        io::stdout().lock(),
                        &gptgrep_core::rich_rerank_context_failure(reader.pin()),
                    )?;
                    return Ok(2);
                }
            };
            let prepared = match gptgrep_core::prepare_rich_rerank(reader, client, request) {
                Ok(prepared) => prepared,
                Err(e) => {
                    serde_json::to_writer(io::stdout().lock(), &e)?;
                    return Ok(2);
                }
            };
            return match prepared.execute().await {
                Ok(report) => {
                    serde_json::to_writer(io::stdout().lock(), &report)?;
                    Ok(0)
                }
                Err(e) => {
                    serde_json::to_writer(io::stdout().lock(), &e)?;
                    Ok(2)
                }
            };
        }
        Command::Batch {
            root,
            input,
            rich,
            model,
            multimodal_timeout,
        } => {
            let mut bytes = Vec::with_capacity(8192);
            let cap = if rich {
                gptgrep_core::rich_batch::MAX_RICH_BATCH_INPUT_BYTES
            } else {
                gptgrep_core::batch::MAX_BATCH_INPUT_BYTES
            };
            let read_result = (|| -> io::Result<()> {
                if input.as_os_str() == "-" {
                    io::stdin()
                        .lock()
                        .take(cap as u64 + 1)
                        .read_to_end(&mut bytes)?;
                } else {
                    std::fs::File::open(&input)?
                        .take(cap as u64 + 1)
                        .read_to_end(&mut bytes)?;
                }
                Ok(())
            })();
            if read_result.is_err() {
                if rich {
                    serde_json::to_writer(
                        io::stdout().lock(),
                        &gptgrep_core::rich_batch::rich_batch_input_io_failure(),
                    )?;
                } else {
                    serde_json::to_writer(
                        io::stdout().lock(),
                        &gptgrep_core::batch::input_io_failure(),
                    )?;
                }
                return Ok(2);
            }
            if rich {
                let request = match gptgrep_core::rich_batch::parse_rich_batch_request(&bytes) {
                    Ok(request) => request,
                    Err(error) => {
                        serde_json::to_writer(io::stdout().lock(), &error)?;
                        return Ok(2);
                    }
                };
                let result = if request.requires_model() {
                    match gptgrep_core::rich_batch::open_rich_batch_reader(&root, &request) {
                        Err(e) => Err(e),
                        Ok(reader) => {
                            // Only the explicit paid operation reaches credential lookup.
                            // Errors remain bounded batch diagnostics, never raw provider/config text.
                            let client = multimodal_client(
                                &reader,
                                model.as_deref(),
                                cli.decisions_budget.as_deref(),
                                multimodal_timeout,
                            )
                            .ok();
                            gptgrep_core::rich_batch::dispatch_rich_batch_with_reader_and_client(
                                &reader, request, client,
                            )
                            .await
                        }
                    }
                } else {
                    gptgrep_core::rich_batch::dispatch_rich_batch(&root, request)
                };
                return match result {
                    Ok(response) => {
                        let failed =
                            response.coverage.failed > 0 || response.coverage.not_executed > 0;
                        serde_json::to_writer(io::stdout().lock(), &response)?;
                        Ok(if failed { 2 } else { 0 })
                    }
                    Err(error) => {
                        serde_json::to_writer(io::stdout().lock(), &error)?;
                        Ok(2)
                    }
                };
            }
            let request = match gptgrep_core::batch::parse_batch_request(&bytes) {
                Ok(request) => request,
                Err(error) => {
                    serde_json::to_writer(io::stdout().lock(), &error)?;
                    return Ok(2);
                }
            };
            match gptgrep_core::batch::dispatch_batch(&root, request, None).await {
                Ok(response) => {
                    let failed = response.coverage.failed > 0 || response.coverage.not_executed > 0;
                    serde_json::to_writer(io::stdout().lock(), &response)?;
                    if failed {
                        return Ok(2);
                    }
                }
                Err(error) => {
                    serde_json::to_writer(io::stdout().lock(), &error)?;
                    return Ok(2);
                }
            }
        }
        Command::Index {
            root,
            max_files,
            optimize_merge,
            rich,
        } => {
            if rich.rich || rich.ocr.is_some() || rich.plan_only {
                if optimize_merge {
                    bail!(
                        "rich ingestion rebuilds complete projection; --optimize-merge is legacy-only"
                    );
                }
                return run_rich_index(&root, max_files, rich).await;
            }
            if rich.resume
                || rich.ocr_run_id.is_some()
                || rich.ocr_runtime_profile.is_some()
                || rich.ocr_codex_home.is_some()
            {
                bail!("OCR options require explicit --ocr auto or --rich");
            }
            let report = gptgrep_core::index_with_options(&root, max_files, optimize_merge).await?;
            if cli.json {
                emit(&serde_json::to_value(&report)?)?;
            } else {
                println!(
                    "Indexed {} documents ({} bytes) in {} ms; generation {}",
                    report.indexed_files, report.source_bytes, report.elapsed_ms, report.generation
                );
            }
        }
        Command::Enrich {
            root,
            ledger_path,
            resume,
            rebuild_failed_window,
            support_retries,
            expected_plan_sha256,
            plan_only,
            plan_output,
            window_bytes,
            max_hints_per_window,
            max_builder_calls,
            max_jev_calls,
            max_ledger_bytes,
            max_windows_per_run,
            deadline_seconds,
            jev_model,
            host,
        } => {
            let mut host = host.config();
            host.jev_model = jev_model;
            if !plan_only {
                host.decisions_execution = decisions_context(cli.decisions_budget.as_deref())?;
            }
            let config = gptgrep_host::EnrichConfig {
                host,
                ledger_path,
                resume,
                rebuild_failed_window,
                support_retries,
                expected_plan_sha256,
                window_bytes,
                max_hints_per_window,
                max_builder_calls,
                max_jev_calls,
                max_ledger_bytes,
                max_windows_per_run,
                timeout_secs: deadline_seconds,
            };
            if plan_only {
                let plan = gptgrep_host::plan_enrichment(&root, &config)?;
                if config
                    .expected_plan_sha256
                    .as_ref()
                    .is_some_and(|expected| expected != &plan.plan_sha256)
                {
                    bail!("Enrichment plan digest differs from --expected-plan-sha256");
                }
                if let Some(path) = &plan_output {
                    let mut options = std::fs::OpenOptions::new();
                    options.write(true).create_new(true);
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::OpenOptionsExt;
                        options.mode(0o600);
                    }
                    let mut file = options
                        .open(path)
                        .context("create immutable enrichment plan")?;
                    serde_json::to_writer_pretty(&mut file, &plan)?;
                    file.write_all(b"\n")?;
                    file.sync_all()?;
                }
                let worst_case_within_caps = plan.worst_case_builder_calls
                    <= config.max_builder_calls
                    && plan.worst_case_jev_calls <= config.max_jev_calls;
                let compact = json!({"schema_version":"gptgrep.enrich-plan.v1",
                    "status":"plan_prepared","plan_sha256":plan.plan_sha256,
                    "generation":plan.binding.source.generation,
                    "manifest_sha256":plan.binding.source.manifest_sha256,
                    "unit_count":plan.unit_count,
                    "worst_case_builder_calls":plan.worst_case_builder_calls,
                    "worst_case_jev_calls":plan.worst_case_jev_calls,
                    "worst_case_within_declared_caps":worst_case_within_caps,
                    "plan_output":plan_output,"new_model_calls":0});
                if cli.json {
                    emit(&compact)?;
                } else {
                    println!(
                        "Planned {} raw windows; SHA-256 {} (zero model calls; worst-case caps {})",
                        plan.unit_count,
                        plan.plan_sha256,
                        if worst_case_within_caps {
                            "sufficient"
                        } else {
                            "insufficient"
                        }
                    );
                }
            } else {
                let report = gptgrep_host::enrich(&root, &config).await?;
                if cli.json {
                    emit(&serde_json::to_value(&report)?)?;
                } else {
                    println!(
                        "Enrichment {}: {} windows complete, {} reused; plan {}",
                        report.status,
                        report.windows_completed,
                        report.windows_reused,
                        report.plan_sha256
                    );
                }
                return Ok(if report.status == "complete" { 0 } else { 2 });
            }
        }
        Command::Search {
            query,
            root,
            mode,
            document,
            limit,
            context,
            ignore_case,
            fixed_strings,
            max_candidates,
            routing_docs,
            model,
            min_score,
            visual,
        } => {
            if visual.multimodal {
                if !matches!(mode, Mode::Hybrid) {
                    bail!(
                        "--multimodal uses mandatory rich routing/reranking; --mode must be hybrid"
                    );
                }
                let options =
                    visual.options(&query, document, min_score, cli.decisions_budget.is_some())?;
                let reader = gptgrep_core::RichGenerationReader::open(&root)?;
                let client = multimodal_client(
                    &reader,
                    model.as_deref(),
                    cli.decisions_budget.as_deref(),
                    visual.multimodal_timeout.unwrap_or(180),
                )?;
                let prepared =
                    gptgrep_core::prepare_rich_multimodal_search(reader, client, &query, &options)?;
                let report = prepared.execute().await?.into_report();
                let code = if report.hits.is_empty() { 1 } else { 0 };
                // A typed JSON report is required to preserve image identities,
                // coverage, accounting and exposure-only citation status.
                emit(&serde_json::to_value(report)?)?;
                return Ok(code);
            }
            if (fixed_strings || context > 0 || ignore_case) && !matches!(mode, Mode::Regex) {
                bail!("--fixed-strings, --ignore-case and --context require --mode regex");
            }
            let options = SearchOptions {
                mode: format!("{mode:?}").to_lowercase(),
                document,
                limit,
                context,
                case_insensitive: ignore_case,
                literal: fixed_strings,
                max_candidates,
                routing_docs,
                model,
                min_score,
            };
            let report = if matches!(mode, Mode::Hybrid | Mode::Semantic) {
                if let Some(context) = decisions_context(cli.decisions_budget.as_deref())? {
                    let client = gptgrep_jev::JevClient::from_env(options.model.as_deref())?
                        .with_execution_context(context);
                    gptgrep_core::search_with_client(&root, &query, &options, &client).await?
                } else {
                    gptgrep_core::search(&root, &query, &options).await?
                }
            } else {
                gptgrep_core::search(&root, &query, &options).await?
            };
            if cli.json {
                emit(&serde_json::to_value(&report)?)?;
            } else {
                let mut stdout = io::stdout().lock();
                for h in &report.hits {
                    for (offset, line) in h.text.lines().enumerate() {
                        writeln!(stdout, "{}:{}:{}", h.path, h.line_start + offset, line)?;
                    }
                }
                for warning in &report.warnings {
                    eprintln!("warning: {warning}");
                }
                if report.coverage.truncated {
                    eprintln!(
                        "warning: result or candidate budget reached; inspect --json coverage"
                    );
                }
            }
            if !report.coverage.stale_files.is_empty() {
                return Ok(2);
            }
            return Ok(if report.hits.is_empty() { 1 } else { 0 });
        }
        Command::Tree { file, root } => {
            emit(&gptgrep_core::tree(&root, &file)?)?;
        }
        Command::Read {
            node_id,
            root,
            max_bytes,
            offset,
        } => {
            let mut result = serde_json::to_value(gptgrep_core::read_node_window(
                &root, &node_id, max_bytes, offset,
            )?)?;
            result["schema_version"] = json!("gptgrep.read.v1");
            emit(&result)?;
        }
        Command::Files { root } => {
            emit(&gptgrep_core::catalog(&root)?)?;
        }
        Command::Status { root } => {
            let result = gptgrep_core::status(&root)?;
            let fresh = result["source_fresh"].as_bool().unwrap_or(false);
            emit(&result)?;
            if !fresh {
                return Ok(2);
            }
        }
        Command::Parse { file } => {
            let mut result = serde_json::to_value(gptgrep_parse::parse_path(&file).await?)?;
            result["schema_version"] = json!("gptgrep.document.v1");
            emit(&result)?;
        }
        Command::Judge { input, model } => {
            let mut bytes = Vec::new();
            std::fs::File::open(input)?
                .take(gptgrep_jev::MAX_REQUEST_BYTES as u64 + 1)
                .read_to_end(&mut bytes)?;
            if bytes.len() > gptgrep_jev::MAX_REQUEST_BYTES {
                bail!("judge input exceeds 64 KiB limit");
            }
            let data = gptgrep_jev::parse_unique_json(&bytes)?;
            let client = gptgrep_jev::JevClient::from_env(model.as_deref())?;
            let client = match decisions_context(cli.decisions_budget.as_deref())? {
                Some(context) => client.with_execution_context(context),
                None => client,
            };
            let prepared = prepare_judge(&client, &data)?;
            emit(&serde_json::to_value(
                client.submit_prepared(prepared).await?,
            )?)?;
        }

        Command::Doctor => {
            emit(
                &json!({"schema_version":"gptgrep.doctor.v1","version":env!("CARGO_PKG_VERSION"),
                "native_index":"embedded tgrep-core","vector_database":false,"mcp":false,
                "jev":{"required_for":["default search","ask","summarize"],"configured":std::env::var_os("OPENROUTER_API_KEY").is_some(),"default_model":gptgrep_jev::DEFAULT_MODEL,"endpoint":"https://openrouter.ai/api/alpha/decisions","network_checked":false},
                "parsing":{"plaintext":true,"liteparse":"embedded Rust library; PDFium required at runtime","ocr":false,"libreoffice_available":program_available("libreoffice") || program_available("soffice")},
                "full_pageindex_flash_parity":false}),
            )?;
        }
        Command::Ask {
            question,
            root,
            host,
            jev_model,
            document,
            experimental_query_plan,
            experimental_source_continuation,
            navigation_overlay_sha256,
            navigation_max_jev_calls,
            navigation_max_request_bytes,
            planner_model,
            experimental_evidence_roles,
            visual,
            max_citations,
            min_score,
        } => {
            if visual.multimodal {
                if experimental_query_plan
                    || experimental_source_continuation
                    || experimental_evidence_roles
                    || navigation_overlay_sha256.is_some()
                    || navigation_max_jev_calls.is_some()
                    || navigation_max_request_bytes.is_some()
                    || planner_model.is_some()
                {
                    bail!(
                        "--multimodal cannot combine with legacy query planning, navigation or source continuation"
                    );
                }
                if host.model != "gpt-6-luna"
                    || host.reasoning_effort != "max"
                    || host.service_tier != "fast"
                {
                    bail!("--multimodal ask requires the native gpt-6-luna/max/fast profile");
                }
                let citations = max_citations.unwrap_or(8);
                if !(1..=8).contains(&citations) {
                    bail!("multimodal citations must be 1 to 8");
                }
                let retrieval = visual.options(
                    &question,
                    document.clone(),
                    min_score.unwrap_or(0.5),
                    cli.decisions_budget.is_some(),
                )?;
                let timeout = visual
                    .multimodal_timeout
                    .unwrap_or(host.timeout)
                    .min(host.timeout);
                let reader = gptgrep_core::RichGenerationReader::open(&root)?;
                let client = multimodal_client(
                    &reader,
                    jev_model.as_deref(),
                    cli.decisions_budget.as_deref(),
                    timeout,
                )?;
                let mut config = host.config();
                config.document = document;
                config.timeout_secs = timeout;
                // The explicit client already owns the selected model/context.
                // The host retains and consumes its genuine initial pass once.
                let options = gptgrep_host::RichMultimodalHostOptions {
                    max_output_bytes: retrieval.max_output_bytes,
                    max_citations: citations,
                    retrieval,
                };
                let prepared = gptgrep_host::prepare_rich_multimodal_host(
                    reader, client, &question, &config, &options,
                )?;
                let report = prepared.execute(&|_| Ok(())).await?;
                emit(&serde_json::to_value(report)?)?;
                return Ok(0);
            }
            let mut config = host.config();
            config.jev_model = jev_model;
            config.decisions_execution = decisions_context(cli.decisions_budget.as_deref())?;
            config.document = document;
            config.source_continuation = experimental_source_continuation;
            config.navigation = navigation_overlay_sha256.map(|expected_overlay_sha256| {
                gptgrep_host::NavigationConfig {
                    expected_overlay_sha256,
                    max_jev_calls: navigation_max_jev_calls.unwrap_or(32),
                    max_jev_request_bytes: navigation_max_request_bytes.unwrap_or(2 * 1024 * 1024),
                }
            });
            config.query_plan = experimental_query_plan.then(|| gptgrep_host::QueryPlanConfig {
                evidence_roles: experimental_evidence_roles,
                planner_model: planner_model.unwrap_or_else(|| "gpt-6-luna".into()),
                ..gptgrep_host::QueryPlanConfig::default()
            });
            emit(&serde_json::to_value(
                gptgrep_host::ask(&root, &question, &config).await?,
            )?)?;
        }
        Command::Summarize {
            node_id,
            root,
            host,
            jev_model,
            document,
        } => {
            let mut config = host.config();
            config.jev_model = jev_model;
            config.decisions_execution = decisions_context(cli.decisions_budget.as_deref())?;
            config.document = document;
            emit(&serde_json::to_value(
                gptgrep_host::summarize(&root, &node_id, &config).await?,
            )?)?;
        }
        Command::HostComplete { input, host } => {
            use std::io::Read;
            let config = host.config();
            let limit = config.max_input_bytes.min(1_048_576);
            let mut bytes = Vec::new();
            if input.as_os_str() == "-" {
                io::stdin().take(limit as u64 + 1).read_to_end(&mut bytes)?;
            } else {
                std::fs::File::open(input)?
                    .take(limit as u64 + 1)
                    .read_to_end(&mut bytes)?;
            }
            if bytes.len() > limit {
                bail!(gptgrep_host::CompletionError::InputLimit);
            }
            let data: Value = serde_json::from_slice(&bytes)?;
            let instructions = data["instructions"]
                .as_str()
                .context("host-complete input requires instructions")?;
            let state = data
                .get("state")
                .context("host-complete input requires state")?
                .clone();
            let schema = data
                .get("schema")
                .context("host-complete input requires schema")?
                .clone();
            emit(&serde_json::to_value(
                gptgrep_host::complete_json(instructions, state, schema, &config).await?,
            )?)?;
        }
        Command::Schema => emit(&contract())?,
    }
    Ok(0)
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let as_json = cli.json
        || matches!(&cli.command, Some(Command::Ocr { .. }))
        || matches!(
            &cli.command,
            Some(
                Command::Search {
                    visual: MultimodalArgs {
                        multimodal: true,
                        ..
                    },
                    ..
                } | Command::Ask {
                    visual: MultimodalArgs {
                        multimodal: true,
                        ..
                    },
                    ..
                }
            )
        );
    let host_operation = matches!(
        cli.command,
        Some(Command::Ask { .. } | Command::Summarize { .. } | Command::HostComplete { .. })
    );
    let result = if host_operation {
        let mut operation = Box::pin(run(cli));
        tokio::select! {
            result = &mut operation => Some(result),
            _ = shutdown_signal() => None,
        }
    } else {
        Some(run(cli).await)
    };
    let Some(result) = result else {
        // Dropping the host future runs its owned-process-group cancellation guard.
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        if as_json {
            let _ = emit(
                &json!({"schema_version":"gptgrep.error.v1","ok":false,"code":"host_interrupted","error":"Host interrupted; owned process cleanup was requested."}),
            );
        } else {
            eprintln!("gptgrep: host interrupted");
        }
        std::process::exit(130);
    };
    match result {
        Ok(code) => std::process::exit(code),
        Err(error) => {
            if error
                .downcast_ref::<io::Error>()
                .is_some_and(|e| e.kind() == io::ErrorKind::BrokenPipe)
            {
                std::process::exit(0);
            }
            if as_json {
                let code = error
                    .downcast_ref::<OcrServeError>()
                    .map(|e| e.0)
                    .unwrap_or_else(|| {
                        error
                            .downcast_ref::<gptgrep_host::CompletionError>()
                            .map(|e| e.code())
                            .unwrap_or_else(|| {
                                if let Some(partial) =
                                    error.downcast_ref::<gptgrep_host::HostRetrievalError>()
                                {
                                    partial.code.as_str()
                                } else if let Some(protocol) =
                                    error.downcast_ref::<gptgrep_host::HostProtocolError>()
                                {
                                    protocol.code()
                                } else if let Some(partial) =
                                    error.downcast_ref::<gptgrep_host::RichMultimodalHostError>()
                                {
                                    partial.code
                                } else if let Some(partial) =
                                    error.downcast_ref::<gptgrep_core::RichMultimodalSearchError>()
                                {
                                    partial.code
                                } else if error.is::<gptgrep_core::JevSearchError>() {
                                    "jev_search_failed"
                                } else if error.is::<gptgrep_host::HostCapabilityError>() {
                                    "host_no_evidence_tools"
                                } else {
                                    "gptgrep_error"
                                }
                            })
                    });
                let diagnostic = if error.is::<OcrServeError>() {
                    "Foreground OCR failed; token values, private paths and native diagnostics are omitted.".into()
                } else {
                    format!("{error:#}")
                };
                let mut report = json!({"schema_version":"gptgrep.error.v1","ok":false,"code":code,"error":diagnostic});
                if let Some(partial) = error.downcast_ref::<gptgrep_core::JevSearchError>() {
                    report["retrieval"] = serde_json::to_value(partial).unwrap_or(Value::Null);
                }
                if let Some(partial) = error.downcast_ref::<gptgrep_host::HostRetrievalError>() {
                    report["host_retrieval"] = serde_json::to_value(partial).unwrap_or(Value::Null);
                }
                if let Some(protocol) = error.downcast_ref::<gptgrep_host::HostProtocolError>() {
                    report["host_protocol"] = serde_json::to_value(protocol).unwrap_or(Value::Null);
                }
                if let Some(partial) =
                    error.downcast_ref::<gptgrep_core::RichMultimodalSearchError>()
                {
                    report["multimodal_retrieval"] =
                        serde_json::to_value(partial).unwrap_or(Value::Null);
                }
                if let Some(partial) = error.downcast_ref::<gptgrep_host::RichMultimodalHostError>()
                {
                    report["multimodal_host"] =
                        serde_json::to_value(partial).unwrap_or(Value::Null);
                }
                let _ = emit(&report);
            } else {
                eprintln!("gptgrep: {error:#}");
            }
            std::process::exit(2);
        }
    }
}

async fn run_ocr_serve(command: OcrCommand) -> Result<i32> {
    let OcrCommand::Serve {
        config,
        token_env,
        initialize_ledger,
        expected_control_sha256,
    } = command;
    gptgrep_host::validate_token_env_name(&token_env)
        .map_err(|_| OcrServeError("ocr_serve_token_env_invalid"))?;
    if expected_control_sha256.as_ref().is_some_and(|sha| {
        sha.len() != 64
            || !sha
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    }) {
        return Err(OcrServeError("ocr_serve_control_digest_invalid").into());
    }
    let control =
        tokio::task::spawn_blocking(move || gptgrep_host::ForegroundOcrControl::load(&config))
            .await
            .map_err(|_| OcrServeError("ocr_serve_control_unavailable"))?
            .map_err(|_| OcrServeError("ocr_serve_control_invalid"))?;
    let digest = control.sha256().to_owned();
    if expected_control_sha256
        .as_ref()
        .is_some_and(|expected| expected != &digest)
    {
        return Err(OcrServeError("ocr_serve_control_digest_mismatch").into());
    }
    let bind = control.bind();
    let token =
        std::env::var(&token_env).map_err(|_| OcrServeError("ocr_serve_token_unavailable"))?;
    if !(32..=256).contains(&token.len()) || !token.bytes().all(|b| b.is_ascii_graphic()) {
        return Err(OcrServeError("ocr_serve_token_invalid").into());
    }
    let service = tokio::task::spawn_blocking(move || {
        control.prepare(token.as_bytes(), &token_env, initialize_ledger)
    })
    .await
    .map_err(|_| OcrServeError("ocr_serve_prepare_unavailable"))?
    .map_err(|_| OcrServeError("ocr_serve_prepare_failed"))?;
    emit(
        &json!({"schema_version":"gptgrep.ocr.serve.lifecycle.v1","status":"prepared","control_sha256":digest,
        "bind":bind.to_string(),"listener_ready":false,"native_model_calls":0}),
    )?;
    let shutdown = gptgrep_host::OcrCancellation::default();
    let signal = shutdown.clone();
    let signal_task = tokio::spawn(async move {
        shutdown_signal().await;
        signal.cancel();
    });
    let result = service.serve(shutdown).await;
    signal_task.abort();
    let _ = signal_task.await;
    result.map_err(|_| OcrServeError("ocr_serve_execution_failed"))?;
    emit(
        &json!({"schema_version":"gptgrep.ocr.serve.lifecycle.v1","status":"stopped","owned_cleanup_awaited":true,
        "provider_usage":null,"remote_completion":"unavailable_unless_observed"}),
    )?;
    Ok(0)
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        if let Ok(mut termination) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            tokio::select! { _ = tokio::signal::ctrl_c() => {}, _ = termination.recv() => {} }
            return;
        }
    }
    let _ = tokio::signal::ctrl_c().await;
}

#[cfg(test)]
mod query_plan_cli_tests {
    use super::*;

    #[test]
    fn production_defaults_budget_option_and_typed_judge_frontdoor_are_reachable() {
        let cli = Cli::try_parse_from([
            "gptgrep",
            "search",
            "ordinary query",
            ".",
            "--decisions-budget",
            "budget.json",
        ])
        .unwrap();
        assert_eq!(cli.decisions_budget, Some(PathBuf::from("budget.json")));
        let contract = contract();
        assert_eq!(
            contract["decisions"]["default_model"],
            gptgrep_jev::DEFAULT_MODEL
        );
        assert_eq!(
            contract["commands"]["ask"]["defaults"]["model"],
            "gpt-6-luna"
        );
        let directory = tempfile::tempdir().unwrap();
        let image = directory.path().join("evidence.png");
        // Fixed 1x1 PNG fixture; generic source text contains no evaluator input.
        std::fs::write(
            &image,
            [
                137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 1, 0, 0, 0,
                1, 8, 6, 0, 0, 0, 31, 21, 196, 137, 0, 0, 0, 13, 73, 68, 65, 84, 120, 156, 99, 248,
                207, 192, 240, 31, 0, 5, 0, 1, 255, 137, 153, 61, 29, 0, 0, 0, 0, 73, 69, 78, 68,
                174, 66, 96, 130,
            ],
        )
        .unwrap();
        let source = json!({"source_id":"source","unit_id":"page","source_sha256":"a".repeat(64)});
        let data = json!({"input":{"text":[{"source":{"source_id":"source","unit_id":"caption","source_sha256":"a".repeat(64)},"text":"Observe this image."}],
            "images":[{"source":source,"path":image}]},"questions":{"visible":{"type":"noul","instructions":"Is the image visible?"}}});
        let client = gptgrep_jev::JevClient::new("ordinary-mock-only", None).unwrap();
        let prepared = prepare_judge(&client, &data).unwrap();
        let wire: Value = serde_json::from_slice(prepared.canonical_body()).unwrap();
        assert!(wire["state"][0].is_string());
        assert_eq!(wire["state"][1]["type"], "image_url");
        assert!(wire["state"][1].get("source").is_none());
        assert_eq!(
            client
                .execution_context()
                .snapshot()
                .unwrap()
                .logical_reservations,
            0
        );
        let legacy =
            gptgrep_jev::JevClient::new("ordinary-mock-only", Some(gptgrep_jev::LEGACY_MODEL))
                .unwrap();
        assert!(prepare_judge(&legacy, &data).is_err());
        let mut unknown = data.clone();
        unknown["state"] = json!("ambiguous");
        assert!(prepare_judge(&client, &unknown).is_err());
        assert!(
            prepare_judge(
                &client,
                &json!({"state":"original structured state","questions":data["questions"]})
            )
            .is_ok()
        );
    }
    #[test]
    fn planning_is_explicit_and_limited_to_ask() {
        let plain =
            Cli::try_parse_from(["gptgrep", "ask", "find the rule", "."]).expect("ordinary ask");
        assert!(matches!(
            plain.command,
            Some(Command::Ask {
                experimental_query_plan: false,
                ..
            })
        ));
        let planned = Cli::try_parse_from([
            "gptgrep",
            "ask",
            "find the rule",
            ".",
            "--experimental-query-plan",
        ])
        .expect("explicit planned ask");
        assert!(matches!(
            planned.command,
            Some(Command::Ask {
                experimental_query_plan: true,
                ..
            })
        ));
        for arguments in [
            vec![
                "gptgrep",
                "summarize",
                "doc:node",
                "--experimental-query-plan",
            ],
            vec!["gptgrep", "host-complete", "-", "--experimental-query-plan"],
            vec![
                "gptgrep",
                "search",
                "rule",
                ".",
                "--experimental-query-plan",
            ],
        ] {
            assert!(Cli::try_parse_from(arguments).is_err());
        }
        let schema = contract();
        assert_eq!(
            schema["commands"]["ask"]["options"]["experimental-query-plan"]["default"],
            false
        );
        assert_eq!(
            schema["commands"]["ask"]["options"]["planner-model"]["default"],
            "gpt-6-luna"
        );
        assert!(
            Cli::try_parse_from([
                "gptgrep",
                "ask",
                "find the rule",
                ".",
                "--planner-model",
                "gpt-5.6-luna"
            ])
            .is_err()
        );
        let control = Cli::try_parse_from([
            "gptgrep",
            "ask",
            "find the rule",
            ".",
            "--experimental-query-plan",
            "--planner-model",
            "gpt-5.6-luna",
        ])
        .expect("explicit model control");
        assert!(
            matches!(control.command, Some(Command::Ask {planner_model: Some(model), ..}) if model == "gpt-5.6-luna")
        );
    }

    #[test]
    fn evidence_roles_require_explicit_ask_planning() {
        let ordinary =
            Cli::try_parse_from(["gptgrep", "ask", "find the rule", "."]).expect("ordinary ask");
        assert!(matches!(
            ordinary.command,
            Some(Command::Ask {
                experimental_evidence_roles: false,
                ..
            })
        ));
        let selected = Cli::try_parse_from([
            "gptgrep",
            "ask",
            "find the rule",
            ".",
            "--experimental-query-plan",
            "--experimental-evidence-roles",
        ])
        .expect("explicit planned evidence-role selection");
        assert!(matches!(
            selected.command,
            Some(Command::Ask {
                experimental_query_plan: true,
                experimental_evidence_roles: true,
                ..
            })
        ));
        for arguments in [
            vec![
                "gptgrep",
                "ask",
                "find the rule",
                ".",
                "--experimental-evidence-roles",
            ],
            vec![
                "gptgrep",
                "search",
                "rule",
                ".",
                "--experimental-evidence-roles",
            ],
            vec![
                "gptgrep",
                "summarize",
                "doc:node",
                "--experimental-evidence-roles",
            ],
            vec![
                "gptgrep",
                "host-complete",
                "-",
                "--experimental-evidence-roles",
            ],
        ] {
            assert!(Cli::try_parse_from(arguments).is_err());
        }
        let schema = contract();
        assert_eq!(
            schema["commands"]["ask"]["options"]["experimental-evidence-roles"]["default"],
            false
        );
        assert_eq!(
            schema["commands"]["ask"]["options"]["experimental-evidence-roles"]["requires"],
            "experimental-query-plan"
        );
    }

    #[test]
    fn navigation_overlay_requires_scoped_planned_ask() {
        let digest = "a".repeat(64);
        let selected = Cli::try_parse_from([
            "gptgrep",
            "ask",
            "find the rule",
            ".",
            "--document",
            "guide.md",
            "--experimental-query-plan",
            "--navigation-overlay-sha256",
            digest.as_str(),
        ])
        .expect("explicit bound navigation");
        assert!(matches!(selected.command, Some(Command::Ask {
            navigation_overlay_sha256: Some(value), experimental_query_plan: true,
            document: Some(document), ..
        }) if value == digest && document == "guide.md"));
        let bounded = Cli::try_parse_from([
            "gptgrep",
            "ask",
            "find the rule",
            ".",
            "--document",
            "guide.md",
            "--experimental-query-plan",
            "--navigation-overlay-sha256",
            digest.as_str(),
            "--navigation-max-jev-calls",
            "64",
            "--navigation-max-request-bytes",
            "1048576",
        ])
        .expect("explicit bounded navigation");
        assert!(matches!(
            bounded.command,
            Some(Command::Ask {
                navigation_max_jev_calls: Some(64),
                navigation_max_request_bytes: Some(1048576),
                ..
            })
        ));
        for arguments in [
            vec![
                "gptgrep",
                "ask",
                "find the rule",
                ".",
                "--navigation-overlay-sha256",
                digest.as_str(),
            ],
            vec![
                "gptgrep",
                "ask",
                "find the rule",
                ".",
                "--experimental-query-plan",
                "--navigation-overlay-sha256",
                digest.as_str(),
            ],
            vec![
                "gptgrep",
                "ask",
                "find the rule",
                ".",
                "--navigation-max-jev-calls",
                "64",
            ],
        ] {
            assert!(Cli::try_parse_from(arguments).is_err());
        }
        let schema = contract();
        assert_eq!(
            schema["commands"]["ask"]["options"]["navigation-overlay-sha256"]["report_schema"],
            "gptgrep.navigation-query.v1"
        );
        assert_eq!(
            schema["commands"]["ask"]["options"]["navigation-max-jev-calls"]["default"],
            32
        );
    }

    #[test]
    fn enrichment_has_a_separate_builder_model_and_offline_plan() {
        let plan = Cli::try_parse_from([
            "gptgrep",
            "enrich",
            ".",
            "--ledger-path",
            "/tmp/gptgrep-builder.jsonl",
            "--max-builder-calls",
            "12",
            "--max-jev-calls",
            "12",
            "--plan-only",
        ])
        .expect("explicit zero-model enrichment plan");
        assert!(matches!(plan.command, Some(Command::Enrich {
            plan_only: true, host: EnrichHostArgs { builder_model, .. }, ..
        }) if builder_model == "gpt-6-luna"));
        assert!(
            Cli::try_parse_from([
                "gptgrep",
                "enrich",
                ".",
                "--ledger-path",
                "/tmp/gptgrep-builder.jsonl",
                "--max-builder-calls",
                "12",
                "--max-jev-calls",
                "12",
                "--plan-output",
                "/tmp/gptgrep-plan.json",
            ])
            .is_err()
        );
        assert!(
            Cli::try_parse_from([
                "gptgrep",
                "enrich",
                ".",
                "--ledger-path",
                "/tmp/gptgrep-builder.jsonl",
                "--max-builder-calls",
                "12",
                "--max-jev-calls",
                "12",
                "--model",
                "gpt-5.6-luna",
            ])
            .is_err()
        );
        let schema = contract();
        assert_eq!(
            schema["commands"]["enrich"]["defaults"]["builder_model"],
            "gpt-6-luna"
        );
        assert_eq!(schema["commands"]["enrich"]["plan_only_model_calls"], 0);
    }

    #[test]
    fn source_continuation_requires_scoped_planned_ask_and_is_default_off() {
        let plain = Cli::try_parse_from(["gptgrep", "ask", "invented query", "."]).unwrap();
        assert!(matches!(
            plain.command,
            Some(Command::Ask {
                experimental_source_continuation: false,
                ..
            })
        ));
        for extra in [
            vec![],
            vec!["--experimental-query-plan"],
            vec!["--document", "notes.md"],
        ] {
            let mut args = vec![
                "gptgrep",
                "ask",
                "invented query",
                ".",
                "--experimental-source-continuation",
            ];
            args.extend(extra);
            assert!(Cli::try_parse_from(args).is_err());
        }
        let selected = Cli::try_parse_from([
            "gptgrep",
            "ask",
            "invented query",
            ".",
            "--experimental-source-continuation",
            "--experimental-query-plan",
            "--document",
            "notes.md",
        ])
        .unwrap();
        assert!(
            matches!(selected.command, Some(Command::Ask { experimental_source_continuation: true, experimental_query_plan: true, document: Some(path), .. }) if path == "notes.md")
        );
        assert!(
            Cli::try_parse_from([
                "gptgrep",
                "summarize",
                "invented-node",
                "--experimental-source-continuation"
            ])
            .is_err()
        );
        let schema = contract();
        assert_eq!(
            schema["commands"]["ask"]["options"]["experimental-source-continuation"]["default"],
            false
        );
        assert_eq!(
            schema["commands"]["ask"]["options"]["experimental-source-continuation"]["additional_model_calls"],
            0
        );
    }

    #[test]
    fn enrichment_rebuild_is_explicit_bounded_and_not_a_plan_only_effect() {
        let base = [
            "gptgrep",
            "enrich",
            ".",
            "--ledger-path",
            "/tmp/synthetic-enrich.jsonl",
            "--max-builder-calls",
            "3",
            "--max-jev-calls",
            "3",
            "--rebuild-failed-window",
            "4",
        ];
        assert!(Cli::try_parse_from(base).is_err());
        let mut args = base.to_vec();
        args.push("--resume");
        assert!(matches!(
            Cli::try_parse_from(&args).unwrap().command,
            Some(Command::Enrich {
                resume: true,
                rebuild_failed_window: Some(4),
                ..
            })
        ));
        args.push("--plan-only");
        assert!(Cli::try_parse_from(args).is_err());
        let schema = contract();
        assert_eq!(
            schema["commands"]["enrich"]["options"]["rebuild-failed-window"]["maximum_admissions_per_ledger"],
            2
        );
        let base = [
            "gptgrep",
            "enrich",
            ".",
            "--ledger-path",
            "/tmp/synthetic-enrich.jsonl",
            "--max-builder-calls",
            "3",
            "--max-jev-calls",
            "3",
            "--support-retries",
        ];
        let mut invalid = base.to_vec();
        invalid.push("3");
        assert!(Cli::try_parse_from(invalid).is_err());
        let mut valid = base.to_vec();
        valid.push("2");
        valid.push("--plan-only");
        assert!(matches!(
            Cli::try_parse_from(valid).unwrap().command,
            Some(Command::Enrich {
                support_retries: 2,
                plan_only: true,
                ..
            })
        ));
        assert_eq!(
            schema["commands"]["enrich"]["options"]["support-retries"]["default"],
            0
        );
        assert_eq!(
            schema["commands"]["enrich"]["options"]["support-retries"]["maximum"],
            2
        );
    }
}

#[cfg(test)]
mod multimodal_cli_tests {
    use super::*;

    #[test]
    fn explicit_workflow_maps_tighter_bounds_and_preserves_legacy_defaults() {
        let cli = Cli::try_parse_from([
            "gptgrep",
            "search",
            "图表",
            "--multimodal",
            "--document",
            "report.pdf",
            "--window-bytes",
            "512",
            "--max-visual-candidates",
            "3",
            "--max-concurrent-batches",
            "2",
            "--max-output-bytes",
            "900000",
            "--max-metadata-bytes",
            "32000",
            "--max-image-bytes",
            "500000",
            "--max-image-pixels",
            "4000000",
            "--min-score",
            "0.7",
        ])
        .unwrap();
        let Some(Command::Search {
            query,
            document,
            min_score,
            visual,
            ..
        }) = cli.command
        else {
            panic!("search");
        };
        let options = visual.options(&query, document, min_score, true).unwrap();
        assert!(options.require_shared_ledger);
        assert_eq!(options.document.as_deref(), Some("report.pdf"));
        assert_eq!(
            (
                options.window_bytes,
                options.max_visual_candidates,
                options.max_concurrent_batches
            ),
            (512, 3, 2)
        );
        assert_eq!(
            (options.max_output_bytes, options.max_metadata_bytes),
            (900000, 32000)
        );
        assert_eq!(
            (
                options.resources.image_file_bytes,
                options.resources.image_pixels
            ),
            (500000, 4000000)
        );
        assert_eq!(options.min_score, 0.7);
        let legacy = Cli::try_parse_from(["gptgrep", "search", "text"]).unwrap();
        assert!(matches!(
            legacy.command,
            Some(Command::Search {
                visual: MultimodalArgs {
                    multimodal: false,
                    ..
                },
                limit: 20,
                context: 0,
                ..
            })
        ));
    }

    #[test]
    fn legacy_candidate_options_and_unselected_visual_controls_do_not_silently_change_workflow() {
        for flag in ["--limit", "--context", "--max-candidates", "--routing-docs"] {
            assert!(
                Cli::try_parse_from(["gptgrep", "search", "chart", "--multimodal", flag, "1"])
                    .is_err()
            );
        }
        for flag in ["--ignore-case", "--fixed-strings"] {
            assert!(
                Cli::try_parse_from(["gptgrep", "search", "chart", "--multimodal", flag]).is_err()
            );
        }
        assert!(Cli::try_parse_from(["gptgrep", "ask", "chart", "--max-citations", "2"]).is_err());
        assert!(
            Cli::try_parse_from(["gptgrep", "search", "chart", "--window-bytes", "100"]).is_err()
        );
    }
}
