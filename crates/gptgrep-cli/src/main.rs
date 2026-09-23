use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand, ValueEnum};
use gptgrep_core::SearchOptions;
use serde_json::{Value, json};
use std::io::{self, Write};
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

#[derive(Debug, Args)]
struct HostArgs {
    #[arg(long, default_value = "codex")]
    codex_bin: String,
    #[arg(
        long,
        help = "Existing Codex account home; defaults to CODEX_HOME or ~/.codex"
    )]
    codex_home: Option<PathBuf>,
    #[arg(long, default_value = "gpt-5.6-luna")]
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
            model: self.model,
            jev_model: None,
            document: None,
            reasoning_effort: self.reasoning_effort,
            service_tier: self.service_tier,
            timeout_secs: self.timeout,
            max_tool_calls: self.max_tool_calls,
            max_input_bytes: self.max_input_bytes,
            trace_path: self.protocol_trace,
            query_plan: None,
            navigation: None,
        }
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
        config.model = self.builder_model;
        config.reasoning_effort = self.reasoning_effort;
        config.service_tier = self.service_tier;
        config.timeout_secs = self.timeout;
        config.max_input_bytes = self.max_input_bytes;
        config
    }
}

#[derive(Debug, Subcommand)]
enum Command {
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
        #[arg(long, help = "Jev Decisions model; default typesafe/jev-1.13")]
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
        #[arg(short = 'n', long, default_value_t = 20)]
        limit: usize,
        #[arg(short = 'C', long, default_value_t = 0)]
        context: usize,
        #[arg(short = 'i', long)]
        ignore_case: bool,
        #[arg(short = 'F', long)]
        fixed_strings: bool,
        #[arg(long, default_value_t = 24)]
        max_candidates: usize,
        #[arg(long, default_value_t = 32)]
        routing_docs: usize,
        #[arg(long, help = "OpenRouter Decisions model; default typesafe/jev-1.13")]
        model: Option<String>,
        #[arg(
            long,
            default_value_t = 0.5,
            help = "Model relevance rubric floor (0..1); not calibrated confidence"
        )]
        min_score: f64,
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
        #[arg(long, help = "Jev Decisions model; default typesafe/jev-1.13")]
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
        host: HostArgs,
    },
    /// Summarize one verified node through the local Codex host.
    Summarize {
        node_id: String,
        #[arg(long, default_value = ".")]
        root: PathBuf,
        #[arg(long, help = "Jev Decisions model; default typesafe/jev-1.13")]
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
            "index":{"usage":"gptgrep index ROOT [--max-files 100000] [--optimize-merge] --json","effect":"write owned .gptgrep immutable generation","network":false},
            "enrich":{"usage":"gptgrep enrich ROOT --ledger-path ABSOLUTE_PATH --max-builder-calls N --max-jev-calls N [--plan-only] [--expected-plan-sha256 SHA] --json","effect":"explicit raw-only Codex/Jev navigation-overlay build after deterministic index","model_assisted":true,"plan_only_model_calls":0,"defaults":{"builder_model":"gpt-6-luna","reasoning_effort":"max","service_tier":"fast","jev_model":"typesafe/jev-1.13","window_bytes":8192,"max_hints_per_window":4},"output":"complete publishes a separate source-bound navigation overlay; incomplete/failed leaves the current index unchanged and exits 2","hint_authority":"non-citable navigation only; not full PageIndex Flash parity","resume":"same absolute ledger and bound plan; pending or failed calls are not automatically retried"},
            "search":{"usage":"gptgrep search QUERY ROOT --mode regex|lexical|hybrid|semantic --json","options":{"mode":{"type":"string","default":"hybrid","enum":["hybrid","semantic","regex","lexical"]},"document":{"type":"string","description":"Exact indexed source-relative path; scope applies before candidate limits"},"limit":{"type":"integer","default":20,"minimum":1,"maximum":1000},"context":{"type":"integer","default":0,"maximum":100},"max-candidates":{"type":"integer","default":24,"maximum":24},"routing-docs":{"type":"integer","default":32,"maximum":32},"ignore-case":{"type":"boolean"},"fixed-strings":{"type":"boolean"},"model":{"type":"string","default":"typesafe/jev-1.13"},"min-score":{"type":"number","minimum":0,"maximum":1,"default":0.5}},"network":"required Jev for default hybrid and semantic; explicit regex/lexical primitives are local","credentials":"OPENROUTER_API_KEY environment only","output_schema":"gptgrep.v1"},
            "tree":{"usage":"gptgrep tree FILE --root ROOT --json","effect":"read"},
            "read":{"usage":"gptgrep read DOCUMENT_ID:NODE_ID --root ROOT --max-bytes 8192 --offset BYTES --json","effect":"read"},
            "files":{"usage":"gptgrep files ROOT --json","effect":"read"},
            "status":{"usage":"gptgrep status ROOT --json","scope":"existing indexed files; index discovers new files"},
            "parse":{"usage":"gptgrep parse FILE --json","effect":"local parser"},
            "judge":{"usage":"gptgrep judge --input FILE [--model MODEL] --json","input":{"state":"JSON","questions":"Choice/Noul/Score map"},"effect":"explicit remote Decisions call"},
            "ask":{"usage":"gptgrep ask QUESTION ROOT [--codex-home HOME] [--codex-bin codex] [--experimental-query-plan] [--planner-model MODEL] [--navigation-overlay-sha256 SHA --navigation-max-jev-calls N --navigation-max-request-bytes BYTES] [--experimental-evidence-roles] --json","effect":"required Jev routing/reranking followed by bounded local Codex reasoning","options":{"jev-model":{"type":"string","default":"typesafe/jev-1.13"},"document":{"type":"string"},"navigation-overlay-sha256":{"type":"string","requires":["experimental-query-plan","document"],"effect":"require exact completed source-bound overlay and full Jev hint scan before planned retrieval","report_schema":"gptgrep.navigation-query.v1","citation_authority":false},"navigation-max-jev-calls":{"type":"integer","requires":"navigation-overlay-sha256","minimum":1,"maximum":128,"default":32},"navigation-max-request-bytes":{"type":"integer","requires":"navigation-overlay-sha256","minimum":1,"maximum":8388608,"default":2097152},"experimental-query-plan":{"type":"boolean","default":false,"effect":"one extra no-tools Luna planner turn; up to two alternate retrieval queries; bounded parallel routes and final Jev reranking against the original question","limits":{"alternate_queries":2,"routing_concurrency":2,"union_candidates":24,"planner_timeout_seconds":45},"accounting":"model_attempts reports planner and reader separately; legacy usage is reader-only; overall timeout is shared"},"planner-model":{"type":"string","default":"gpt-6-luna","requires":"experimental-query-plan","effect":"select the query-planner Codex model independently from the final reader"},"experimental-evidence-roles":{"type":"boolean","default":false,"requires":"experimental-query-plan","effect":"one additional Choice per union candidate in the same mandatory Jev decision; source-bound role ordering and continuation hints","limits":{"union_candidates":24,"final_questions":48,"complete_request_bytes":65536},"sufficiency":"unassessed","accounting":"extra judgments and tokens are measured within the existing request; discarded-candidate metadata stays private and is not citable"}},"defaults":{"model":"gpt-5.6-luna","reasoning_effort":"max","service_tier":"fast","timeout_seconds":180,"max_tool_calls":12}},
            "summarize":{"usage":"gptgrep summarize DOCUMENT_ID:NODE_ID --root ROOT [host options] --json","effect":"required Jev retrieval in the selected document, then model-written summary with issued evidence citations"},
            "host-complete":{"usage":"gptgrep host-complete --input FILE_OR_DASH [host options] --json","input":{"instructions":"string","state":"JSON","schema":"JSON Schema object"},"effect":"explicit schema-validated local Codex completion; no citation assertion","defaults":{"max_input_bytes":262144,"service_tier":"fast"},"hard_max_input_bytes":1048576},
            "doctor":{"usage":"gptgrep doctor --json","effect":"local capability probe"}
        },
        "search_output":{"fields":["schema_version","query","mode","document_scope","root","generation","index_used","source_fresh","minimum_relevance_score","hits","coverage","metrics","warnings"],"hit_fields":["path","node_id","title","line_start","line_end","page_start","page_end","match_line","match_column","byte_start","byte_end","column_start","node_offset","next_offset","node_coverage","coordinate_system","text","text_truncated","score","confidence","literal_anchor","source_sha256","source_fresh","citation"]},
        "bounds":{"source_file_bytes":67108864,"jev_candidates":24,"semantic_routing_documents":32},
        "limitations":["No full PageIndex Flash parity claim", "No OCR in the initial native build", "Office conversion requires LibreOffice", "Semantic routing and evidence snippets have explicit budgets", "Generative synthesis requires explicit local Codex host mode", "Relevance floor is operational policy, not calibrated confidence; hybrid exact-token anchors are retained"]
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
    contract
}

fn llms() -> &'static str {
    "# GPTgrep\n\nLocal vectorless retrieval helper for agents. No MCP server.\n\n1. gptgrep index ./docs --json\n1a. gptgrep enrich ./docs --ledger-path /path/to/private/enrich.jsonl --max-builder-calls 100 --max-jev-calls 100 --plan-only --json\n2. gptgrep search 'concept in natural language' ./docs --json\n3. gptgrep search 'pattern' ./docs --mode regex --json\n4. gptgrep tree manual.pdf --root ./docs --json\n5. gptgrep read DOCUMENT_ID:NODE_ID --root ./docs --json\n\nJev routing/reranking is required by default search, ask and summarize. OPENROUTER_API_KEY must be supplied for model-assisted runs; failures never silently downgrade to local retrieval. Explicit regex/lexical and enrich --plan-only remain offline. Enrich is an explicit Codex/Jev step that creates non-citable navigation hints after local indexing. Use --document RELATIVE_PATH to scope before candidate budgets. Hybrid/semantic and judge send bounded data to OpenRouter Jev. Inspect coverage, source_fresh, coordinate_system and text_truncated before citing evidence. The main agent performs reasoning and controls follow-up reads. New files require reindexing. --schema provides the command contract.\n"
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
        Command::Index {
            root,
            max_files,
            optimize_merge,
        } => {
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
        } => {
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
            let report = gptgrep_core::search(&root, &query, &options).await?;
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
            let metadata = std::fs::metadata(&input)?;
            if metadata.len() > 65536 {
                bail!("judge input exceeds 64 KiB limit");
            }
            let data: Value = serde_json::from_slice(&std::fs::read(input)?)?;
            let state = data
                .get("state")
                .context("judge input requires state")?
                .clone();
            let questions = data
                .get("questions")
                .context("judge input requires questions")?
                .clone();
            let client = gptgrep_jev::JevClient::from_env(model.as_deref())?;
            emit(&serde_json::to_value(
                client.decide(state, questions).await?,
            )?)?;
        }
        Command::Doctor => {
            emit(
                &json!({"schema_version":"gptgrep.doctor.v1","version":env!("CARGO_PKG_VERSION"),
                "native_index":"embedded tgrep-core","vector_database":false,"mcp":false,
                "jev":{"required_for":["default search","ask","summarize"],"configured":std::env::var_os("OPENROUTER_API_KEY").is_some(),"default_model":"typesafe/jev-1.13","endpoint":"https://openrouter.ai/api/alpha/decisions","network_checked":false},
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
            navigation_overlay_sha256,
            navigation_max_jev_calls,
            navigation_max_request_bytes,
            planner_model,
            experimental_evidence_roles,
        } => {
            let mut config = host.config();
            config.jev_model = jev_model;
            config.document = document;
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
    let as_json = cli.json;
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
                        } else if error.is::<gptgrep_core::JevSearchError>() {
                            "jev_search_failed"
                        } else if error.is::<gptgrep_host::HostCapabilityError>() {
                            "host_no_evidence_tools"
                        } else {
                            "gptgrep_error"
                        }
                    });
                let mut report = json!({"schema_version":"gptgrep.error.v1","ok":false,"code":code,"error":format!("{error:#}")});
                if let Some(partial) = error.downcast_ref::<gptgrep_core::JevSearchError>() {
                    report["retrieval"] = serde_json::to_value(partial).unwrap_or(Value::Null);
                }
                if let Some(partial) = error.downcast_ref::<gptgrep_host::HostRetrievalError>() {
                    report["host_retrieval"] = serde_json::to_value(partial).unwrap_or(Value::Null);
                }
                if let Some(protocol) = error.downcast_ref::<gptgrep_host::HostProtocolError>() {
                    report["host_protocol"] = serde_json::to_value(protocol).unwrap_or(Value::Null);
                }
                let _ = emit(&report);
            } else {
                eprintln!("gptgrep: {error:#}");
            }
            std::process::exit(2);
        }
    }
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
