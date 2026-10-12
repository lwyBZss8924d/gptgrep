//! `gptgrep decisions-budget` and judge failure accounting through the real binary.
//! Paid paths are exercised only up to local refusal. Every judge invocation runs
//! behind a counting loopback proxy, so an accidental send could never leave the
//! host and is detected; consuming submissions uses a same-process loopback server.
#![cfg(unix)]

use serde_json::{Value, json};
use std::{
    fs,
    io::{Read, Write},
    net::TcpListener,
    os::unix::fs::PermissionsExt,
    path::Path,
    process::{Command, Output},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

const FAKE_KEY: &str = "synthetic-cli-key-sentinel";

/// Counts every connection; never answers. Used as HTTP(S)/ALL proxy.
fn counting_proxy() -> (String, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = format!("http://{}", listener.local_addr().unwrap());
    let count = Arc::new(AtomicUsize::new(0));
    let counter = count.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            counter.fetch_add(1, Ordering::SeqCst);
            drop(stream);
        }
    });
    (address, count)
}

/// Answers scripted 200 bodies on loopback; counts accepted connections.
fn decisions_server(bodies: Vec<String>) -> (String, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!(
        "http://{}/api/alpha/decisions",
        listener.local_addr().unwrap()
    );
    let count = Arc::new(AtomicUsize::new(0));
    let counter = count.clone();
    std::thread::spawn(move || {
        let mut bodies = bodies.into_iter();
        for stream in listener.incoming() {
            let Ok(mut socket) = stream else { continue };
            counter.fetch_add(1, Ordering::SeqCst);
            let Some(body) = bodies.next() else { continue };
            let mut request = Vec::new();
            loop {
                let mut chunk = [0; 4096];
                let read = socket.read(&mut chunk).unwrap_or(0);
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&chunk[..read]);
                if let Some(offset) = request.windows(4).position(|p| p == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&request[..offset]).to_lowercase();
                    let length: usize = headers
                        .lines()
                        .find_map(|l| l.strip_prefix("content-length:"))
                        .map_or(0, |v| v.trim().parse().unwrap());
                    if request.len() >= offset + 4 + length {
                        break;
                    }
                }
            }
            let _ = socket.write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            );
        }
    });
    (endpoint, count)
}

fn gptgrep(args: &[&str], key: Option<&str>, proxy: &str, home: &Path) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_gptgrep"));
    command
        .args(args)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", home)
        .env("TMPDIR", home)
        .env("HTTPS_PROXY", proxy)
        .env("HTTP_PROXY", proxy)
        .env("ALL_PROXY", proxy);
    if let Some(key) = key {
        command.env("OPENROUTER_API_KEY", key);
    }
    command.output().unwrap()
}

fn json_output(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|_| {
        panic!(
            "stdout is not JSON: {} / {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

fn assert_no_secret(output: &Output) {
    for stream in [&output.stdout, &output.stderr] {
        let text = String::from_utf8_lossy(stream);
        assert!(!text.contains(FAKE_KEY));
        assert!(!text.contains("synthetic-private-note"));
    }
}

#[test]
fn init_status_and_joined_judge_stop_at_the_cap_without_any_post() {
    let directory = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(directory.path()).unwrap();
    let home = root.join("home");
    fs::create_dir(&home).unwrap();
    let (proxy, proxied) = counting_proxy();
    let plan = root.join("plan.json");
    fs::write(&plan, br#"{"schema":"synthetic-plan","steps":["judge"]}"#).unwrap();
    let ledger = root.join("ledger.json");
    let control = root.join("control.json");
    let init_args = [
        "decisions-budget",
        "init",
        "--run-id",
        "synthetic-cli-run",
        "--plan-file",
        plan.to_str().unwrap(),
        "--deadline-seconds",
        "600",
        "--max-submissions",
        "1",
        "--ledger",
        ledger.to_str().unwrap(),
        "--output",
        control.to_str().unwrap(),
    ];
    let init = gptgrep(&init_args, None, &proxy, &home);
    assert!(init.status.success(), "{init:?}");
    let created = json_output(&init);
    assert_eq!(
        created["schema_version"],
        "gptgrep.decisions-budget-init.v1"
    );
    assert_eq!(created["status"], "initialized");
    assert_eq!(created["max_submissions"], 1);
    assert_eq!(created["joins_existing"], false);
    let digest = {
        use sha2::Digest;
        format!("{:x}", sha2::Sha256::digest(fs::read(&plan).unwrap()))
    };
    assert_eq!(created["plan_digest"], digest);
    let contract: Value = serde_json::from_slice(&fs::read(&control).unwrap()).unwrap();
    assert_eq!(
        contract,
        json!({"schema_version":"gptgrep.decisions-budget-contract.v1","run_id":"synthetic-cli-run",
            "plan_digest":digest,"deadline_unix_ms":created["deadline_unix_ms"],"max_submissions":1,
            "ledger_path":ledger})
    );
    assert_eq!(
        fs::metadata(&control).unwrap().permissions().mode() & 0o777,
        0o600
    );

    // A second init never resets or renews, and never replaces the control.
    let snapshot = |path: &Path| fs::read(path).unwrap();
    let before = [
        snapshot(&ledger),
        snapshot(&root.join("ledger.json.lock")),
        snapshot(&control),
    ];
    let mut renewed = init_args;
    renewed[7] = "1200";
    for args in [init_args, renewed] {
        let again = gptgrep(&args, None, &proxy, &home);
        assert_eq!(again.status.code(), Some(2));
        let error = json_output(&again);
        assert_eq!(error["code"], "decisions_budget_failed");
        assert!(
            error["error"]
                .as_str()
                .unwrap()
                .contains("never joins, resets or renews")
        );
    }
    let mut global = vec!["--decisions-budget", control.to_str().unwrap()];
    global.extend(init_args);
    let refused = gptgrep(&global, None, &proxy, &home);
    assert_eq!(refused.status.code(), Some(2));
    assert_eq!(json_output(&refused)["code"], "decisions_budget_failed");
    assert_eq!(
        before,
        [
            snapshot(&ledger),
            snapshot(&root.join("ledger.json.lock")),
            snapshot(&control),
        ]
    );

    let status_args = [
        "decisions-budget",
        "status",
        "--control",
        control.to_str().unwrap(),
        "--json",
    ];
    let status = json_output(&gptgrep(&status_args, None, &proxy, &home));
    assert_eq!(
        status["schema_version"],
        "gptgrep.decisions-budget-status.v1"
    );
    assert_eq!(status["reservations"]["total"], 0);
    assert_eq!(status["remaining_submissions"], 1);
    assert_eq!(status["deadline_state"], "open");
    assert_eq!(status["admission_open"], true);
    assert_eq!(status["read_only"], true);

    // Consume the only allowance through the shared ledger and a loopback server.
    let (endpoint, served) = decisions_server(vec![
        json!({"model":"openai/gpt-6-luna-decisions-20261006","id":"gen-synthetic-cli",
            "answers":{"q":{"type":"noul","noul":0.6}},"usage":{"input_tokens":12,"output_tokens":1,"cost":0.0000012}})
        .to_string(),
    ]);
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let client = gptgrep_jev::JevClient::with_endpoint(FAKE_KEY, None, &endpoint)
            .unwrap()
            .with_execution_context(
                gptgrep_jev::ExecutionContext::from_budget_file(&control).unwrap(),
            );
        client
            .decide(
                json!("synthetic note"),
                json!({"q":{"type":"noul","instructions":"Is it synthetic?"}}),
            )
            .await
            .unwrap();
    });
    assert_eq!(served.load(Ordering::SeqCst), 1);

    let request = root.join("request.json");
    fs::write(
        &request,
        json!({"state":{"note":"synthetic-private-note"},
            "questions":{"q":{"type":"noul","instructions":"Is it synthetic?"}}})
        .to_string(),
    )
    .unwrap();
    let judge = gptgrep(
        &[
            "--decisions-budget",
            control.to_str().unwrap(),
            "judge",
            "--input",
            request.to_str().unwrap(),
            "--json",
        ],
        Some(FAKE_KEY),
        &proxy,
        &home,
    );
    assert_eq!(judge.status.code(), Some(2));
    assert_no_secret(&judge);
    let error = json_output(&judge);
    assert_eq!(error["schema_version"], "gptgrep.error.v1");
    assert_eq!(error["code"], "decisions_judge_failed");
    assert_eq!(error["error"], "Decisions submission budget exhausted");
    let report = &error["judge"];
    assert_eq!(report["schema_version"], "gptgrep.judge-failure.v1");
    assert_eq!(report["stage"], "submission");
    assert_eq!(report["submission"], "not_submitted");
    assert_eq!(report["requested_model"], "openai/gpt-6-luna-decisions");
    assert!(report["served_model"].is_null());
    assert!(report["cost_usd"].is_null());
    assert_eq!(report["cost_unavailable_reason"], "not_submitted");
    assert_eq!(report["execution"]["scope"], "same_host_ledger");
    assert_eq!(report["execution"]["logical_reservations"], 1);
    let status = json_output(&gptgrep(&status_args, None, &proxy, &home));
    assert_eq!(status["reservations"]["total"], 1);
    assert_eq!(status["reservations"]["terminal"], 1);
    assert_eq!(status["remaining_submissions"], 0);
    assert_eq!(status["admission_open"], false);
    assert_eq!(status["submission_observations"], 2);
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(proxied.load(Ordering::SeqCst), 0, "no provider egress");
    assert_eq!(served.load(Ordering::SeqCst), 1, "no POST after exhaustion");
}

#[test]
fn judge_alias_failures_before_submission_are_diagnosed_without_network() {
    let directory = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(directory.path()).unwrap();
    let (proxy, proxied) = counting_proxy();
    let image = root.join("synthetic.png");
    let mut png = Vec::new();
    image::ImageEncoder::write_image(
        image::codecs::png::PngEncoder::new(&mut png),
        &[200, 0, 0, 200, 0, 0, 200, 0, 0, 200, 0, 0],
        2,
        2,
        image::ExtendedColorType::Rgb8,
    )
    .unwrap();
    fs::write(&image, &png).unwrap();
    let request = root.join("image.json");
    fs::write(
        &request,
        json!({"input":{"text":[{"source":{"source_id":"synthetic","unit_id":"caption","source_sha256":"a".repeat(64)},"text":"synthetic-private-note"}],
            "images":[{"source":{"source_id":"synthetic","unit_id":"image","source_sha256":"a".repeat(64)},"path":image}]},
            "questions":{"red":{"type":"noul","instructions":"Is the image mostly red?"}}})
        .to_string(),
    )
    .unwrap();
    let image_judge = gptgrep(
        &[
            "judge",
            "--input",
            request.to_str().unwrap(),
            "--model",
            "~typesafe/jev-latest",
            "--json",
        ],
        Some(FAKE_KEY),
        &proxy,
        &root,
    );
    assert_eq!(image_judge.status.code(), Some(2));
    assert_no_secret(&image_judge);
    let error = json_output(&image_judge);
    assert_eq!(error["code"], "decisions_judge_failed");
    assert_eq!(
        error["error"],
        "Decisions images require the Luna Decisions model; no request was prepared or submitted"
    );
    assert_eq!(error["judge"]["stage"], "preparation");
    assert_eq!(error["judge"]["requested_model"], "~typesafe/jev-latest");
    assert_eq!(error["judge"]["submission"], "not_submitted");
    assert_eq!(error["judge"]["execution"]["logical_reservations"], 0);
    assert_eq!(error["judge"]["execution"]["submission_observations"], 0);

    let malformed = gptgrep(
        &[
            "judge",
            "--input",
            request.to_str().unwrap(),
            "--model",
            "~typesafe/jev",
            "--json",
        ],
        Some(FAKE_KEY),
        &proxy,
        &root,
    );
    assert_eq!(malformed.status.code(), Some(2));
    assert_no_secret(&malformed);
    let error = json_output(&malformed);
    assert_eq!(error["judge"]["stage"], "client");
    assert!(
        error["error"]
            .as_str()
            .unwrap()
            .contains("Unsupported Decisions model alias")
    );

    let missing_key = gptgrep(
        &["judge", "--input", request.to_str().unwrap(), "--json"],
        None,
        &proxy,
        &root,
    );
    assert_eq!(missing_key.status.code(), Some(2));
    let error = json_output(&missing_key);
    assert_eq!(error["judge"]["stage"], "client");
    assert_eq!(error["judge"]["cost_unavailable_reason"], "not_submitted");
    assert_eq!(error["error"], "OPENROUTER_API_KEY is not available");
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(proxied.load(Ordering::SeqCst), 0, "no provider egress");
}
