//! Compose public GPTgrep Rust APIs over a new, caller-owned synthetic corpus.
//!
//! This example uses exact regex retrieval and makes no model or provider calls.
//! Pass an absolute, nonexistent output directory; all generated data stays there.
use anyhow::{Context, Result, ensure};
use gptgrep_core::{SearchOptions, index, read_node, search};
use serde_json::json;
use std::{fs, path::PathBuf};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let mut args = std::env::args_os().skip(1);
    let output = PathBuf::from(
        args.next()
            .context("pass an absolute new output directory")?,
    );
    ensure!(args.next().is_none(), "expected one output directory");
    ensure!(output.is_absolute(), "output directory must be absolute");
    fs::create_dir(&output).context("create new output directory; parent must exist")?;
    let corpus = output.join("documents");
    fs::create_dir(&corpus)?;
    fs::write(
        corpus.join("example.md"),
        "# Service notes\n\nThe retry window is thirty seconds.\nBlue status means ready.\n",
    )?;

    let indexed = index(&corpus, 4).await?;
    ensure!(indexed.indexed_files == 1, "expected one indexed document");
    let options = SearchOptions {
        mode: "regex".into(),
        limit: 4,
        ..SearchOptions::default()
    };
    // Independent retrieval operations can be composed by the caller's Rust code.
    let (retry, status) = tokio::try_join!(
        search(&corpus, "retry window", &options),
        search(&corpus, "Blue status", &options),
    )?;
    let mut citations = Vec::new();
    for (report, expected) in [(retry, "thirty seconds"), (status, "means ready")] {
        ensure!(report.index_used, "expected the published trigram index");
        ensure!(
            report.hits.len() == 1,
            "expected one matching source window"
        );
        ensure!(
            report.metrics.jev_calls_attempted == 0 && report.metrics.jev_requests == 0,
            "exact retrieval unexpectedly attempted a provider call"
        );
        let hit = &report.hits[0];
        ensure!(hit.source_fresh, "source evidence must be fresh");
        let node = read_node(&corpus, &hit.node_id, 4096)?;
        ensure!(
            node.source_fresh && node.text.contains(expected),
            "the public node reader did not return the expected source text"
        );
        citations.push(node.citation);
    }
    println!(
        "{}",
        json!({
            "schema": "gptgrep.rust-ptc-example.v1",
            "mode": "regex",
            "indexed_files": indexed.indexed_files,
            "composed_queries": citations.len(),
            "index_used": true,
            "source_fresh": true,
            "read_citations": citations,
            "jev_calls_attempted": 0,
        })
    );
    Ok(())
}
