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
        }
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

fn contract() -> Value {
    json!({
        "schema_version":"gptgrep.cli.v1", "name":"gptgrep", "version":env!("CARGO_PKG_VERSION"),
        "transport":"argv/stdout", "mcp":false, "default_output":"text", "machine_output":"--json",
        "exit_codes":{"0":"success or matches","1":"no matches","2":"error or stale evidence excluded"},
        "commands":{
            "index":{"usage":"gptgrep index ROOT [--max-files 100000] [--optimize-merge] --json","effect":"write owned .gptgrep immutable generation","network":false},
            "search":{"usage":"gptgrep search QUERY ROOT --mode regex|lexical|hybrid|semantic --json","options":{"mode":{"type":"string","default":"hybrid","enum":["hybrid","semantic","regex","lexical"]},"document":{"type":"string","description":"Exact indexed source-relative path; scope applies before candidate limits"},"limit":{"type":"integer","default":20,"minimum":1,"maximum":1000},"context":{"type":"integer","default":0,"maximum":100},"max-candidates":{"type":"integer","default":24,"maximum":24},"routing-docs":{"type":"integer","default":32,"maximum":32},"ignore-case":{"type":"boolean"},"fixed-strings":{"type":"boolean"},"model":{"type":"string","default":"typesafe/jev-1.13"},"min-score":{"type":"number","minimum":0,"maximum":1,"default":0.5}},"network":"required Jev for default hybrid and semantic; explicit regex/lexical primitives are local","credentials":"OPENROUTER_API_KEY environment only","output_schema":"gptgrep.v1"},
            "tree":{"usage":"gptgrep tree FILE --root ROOT --json","effect":"read"},
            "read":{"usage":"gptgrep read DOCUMENT_ID:NODE_ID --root ROOT --max-bytes 8192 --offset BYTES --json","effect":"read"},
            "files":{"usage":"gptgrep files ROOT --json","effect":"read"},
            "status":{"usage":"gptgrep status ROOT --json","scope":"existing indexed files; index discovers new files"},
            "parse":{"usage":"gptgrep parse FILE --json","effect":"local parser"},
            "judge":{"usage":"gptgrep judge --input FILE [--model MODEL] --json","input":{"state":"JSON","questions":"Choice/Noul/Score map"},"effect":"explicit remote Decisions call"},
            "ask":{"usage":"gptgrep ask QUESTION ROOT [--codex-home HOME] [--codex-bin codex] [--experimental-query-plan] --json","effect":"required Jev routing/reranking followed by bounded local Codex reasoning","options":{"jev-model":{"type":"string","default":"typesafe/jev-1.13"},"document":{"type":"string"},"experimental-query-plan":{"type":"boolean","default":false,"effect":"one extra no-tools Luna planner turn; up to two alternate retrieval queries; bounded parallel routes and final Jev reranking against the original question","limits":{"alternate_queries":2,"routing_concurrency":2,"union_candidates":24,"planner_timeout_seconds":45},"accounting":"model_attempts reports planner and reader separately; legacy usage is reader-only; overall timeout is shared"}},"defaults":{"model":"gpt-5.6-luna","reasoning_effort":"max","service_tier":"fast","timeout_seconds":180,"max_tool_calls":12}},
            "summarize":{"usage":"gptgrep summarize DOCUMENT_ID:NODE_ID --root ROOT [host options] --json","effect":"required Jev retrieval in the selected document, then model-written summary with issued evidence citations"},
            "host-complete":{"usage":"gptgrep host-complete --input FILE_OR_DASH [host options] --json","input":{"instructions":"string","state":"JSON","schema":"JSON Schema object"},"effect":"explicit schema-validated local Codex completion; no citation assertion","defaults":{"max_input_bytes":262144,"service_tier":"fast"},"hard_max_input_bytes":1048576},
            "doctor":{"usage":"gptgrep doctor --json","effect":"local capability probe"}
        },
        "search_output":{"fields":["schema_version","query","mode","document_scope","root","generation","index_used","source_fresh","minimum_relevance_score","hits","coverage","metrics","warnings"],"hit_fields":["path","node_id","title","line_start","line_end","page_start","page_end","match_line","match_column","byte_start","byte_end","column_start","node_offset","next_offset","coordinate_system","text","text_truncated","score","confidence","literal_anchor","source_sha256","source_fresh","citation"]},
        "bounds":{"source_file_bytes":67108864,"jev_candidates":24,"semantic_routing_documents":32},
        "limitations":["No full PageIndex Flash parity claim", "No OCR in the initial native build", "Office conversion requires LibreOffice", "Semantic routing and evidence snippets have explicit budgets", "Generative synthesis requires explicit local Codex host mode", "Relevance floor is operational policy, not calibrated confidence; hybrid exact-token anchors are retained"]
    })
}

fn llms() -> &'static str {
    "# GPTgrep\n\nLocal vectorless retrieval helper for agents. No MCP server.\n\n1. gptgrep index ./docs --json\n2. gptgrep search 'concept in natural language' ./docs --json\n3. gptgrep search 'pattern' ./docs --mode regex --json\n4. gptgrep tree manual.pdf --root ./docs --json\n5. gptgrep read DOCUMENT_ID:NODE_ID --root ./docs --json\n\nJev routing/reranking is required by default search, ask and summarize. OPENROUTER_API_KEY must be supplied; failures never silently downgrade to local retrieval. Explicit regex/lexical remain offline primitives. Use --document RELATIVE_PATH to scope before candidate budgets. Hybrid/semantic and judge send bounded data to OpenRouter Jev. Inspect coverage, source_fresh, coordinate_system and text_truncated before citing evidence. The main agent performs reasoning and controls follow-up reads. New files require reindexing. --schema provides the command contract.\n"
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
        } => {
            let mut config = host.config();
            config.jev_model = jev_model;
            config.document = document;
            config.query_plan =
                experimental_query_plan.then(gptgrep_host::QueryPlanConfig::default);
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
    }
}
