use std::process::Command;

fn cli() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_gptgrep"));
    command
        .env_remove("OPENROUTER_API_KEY")
        .env_remove(gptgrep_jev::helper_ipc::CONTROL_ENV)
        .env_remove(gptgrep_jev::helper_ipc::CONTROL_SHA_ENV);
    command
}

#[test]
fn helper_command_is_explicit_and_schema_declares_keyless_clients() {
    let help = cli().args(["decisions-helper", "--help"]).output().unwrap();
    assert!(help.status.success());
    let help = String::from_utf8(help.stdout).unwrap();
    assert!(help.contains("serve") && help.contains("probe"));
    let schema = cli().arg("--schema").output().unwrap();
    assert!(schema.status.success());
    let schema: serde_json::Value = serde_json::from_slice(&schema.stdout).unwrap();
    let helper = &schema["commands"]["decisions-helper"];
    assert_eq!(
        helper["client_environment"],
        serde_json::json!([
            "GPTGREP_DECISIONS_IPC_CONTROL",
            "GPTGREP_DECISIONS_IPC_SHA256"
        ])
    );
    assert!(
        helper["transport"]
            .as_str()
            .unwrap()
            .contains("no reconnect")
    );
}

#[test]
fn helper_missing_control_has_fixed_diagnostic_and_no_http_fallback() {
    let result = cli()
        .args([
            "decisions-helper",
            "probe",
            "--control",
            "/unavailable-helper-control.json",
            "--expected-control-sha256",
            &"a".repeat(64),
        ])
        .output()
        .unwrap();
    assert_eq!(result.status.code(), Some(2));
    assert!(result.stderr.is_empty());
    let result: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(result["code"], "decisions_helper_failed");
    assert!(!result["error"].as_str().unwrap().contains("/unavailable"));
}

#[cfg(target_os = "linux")]
mod linux {
    use super::*;
    use gptgrep_jev::helper_ipc::{
        FilePin, NodeIdentity, PeerScope, ProcessIdentity, SERVER_SCHEMA, ServerControl,
    };
    use serde_json::json;
    use sha2::{Digest, Sha256};
    use std::{
        fs::OpenOptions,
        io::{BufRead, BufReader, Write},
        os::{
            fd::AsRawFd,
            unix::{
                fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
                process::CommandExt,
            },
        },
        path::Path,
        process::Stdio,
        sync::mpsc,
        time::Duration,
    };

    fn pin(path: &Path, value: &impl serde::Serialize) -> FilePin {
        let bytes = serde_json::to_vec(value).unwrap();
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
            .unwrap();
        file.write_all(&bytes).unwrap();
        file.sync_all().unwrap();
        FilePin {
            path: path.to_owned(),
            sha256: format!("{:x}", Sha256::digest(bytes)),
        }
    }
    fn process(pid: u32) -> ProcessIdentity {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
        let start_ticks = stat
            .rsplit_once(") ")
            .unwrap()
            .1
            .split_whitespace()
            .nth(19)
            .unwrap()
            .parse()
            .unwrap();
        let namespace = std::fs::read_link(format!("/proc/{pid}/ns/pid")).unwrap();
        let pid_namespace = namespace
            .to_str()
            .unwrap()
            .strip_prefix("pid:[")
            .unwrap()
            .strip_suffix(']')
            .unwrap()
            .parse()
            .unwrap();
        let cgroup = std::fs::read_to_string(format!("/proc/{pid}/cgroup"))
            .unwrap()
            .lines()
            .find_map(|line| line.strip_prefix("0::"))
            .unwrap()
            .to_owned();
        let boot_id = std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
            .unwrap()
            .trim()
            .to_owned();
        ProcessIdentity {
            pid,
            start_ticks,
            pid_namespace,
            cgroup,
            boot_id,
        }
    }
    fn mono() -> u64 {
        let mut value = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        assert_eq!(
            unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut value) },
            0
        );
        value.tv_sec as u64 * 1_000_000_000 + value.tv_nsec as u64
    }

    #[test]
    fn helper_foreground_disabled_process_probes_and_shuts_down_without_credentials() {
        let root = tempfile::tempdir().unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let run = root.path().join("run");
        let bindings = run.join("bindings");
        let temporary = root.path().join("temporary");
        let endpoint = temporary.join("socket-parent-".repeat(10));
        for path in [&bindings, &endpoint] {
            std::fs::create_dir_all(path).unwrap();
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let plan = root.path().join("plan.json");
        std::fs::write(&plan, b"synthetic plan").unwrap();
        let budget = root.path().join("budget.json");
        let ledger = root.path().join("ledger.jsonl");
        let init = cli()
            .args([
                "decisions-budget",
                "init",
                "--run-id",
                "helper-process-synthetic",
                "--plan-file",
                plan.to_str().unwrap(),
                "--deadline-seconds",
                "30",
                "--max-submissions",
                "5",
                "--ledger",
                ledger.to_str().unwrap(),
                "--output",
                budget.to_str().unwrap(),
            ])
            .output()
            .unwrap();
        assert!(
            init.status.success(),
            "{}",
            String::from_utf8_lossy(&init.stderr)
        );
        let budget_bytes = std::fs::read(&budget).unwrap();
        let lease_id = "a".repeat(32);
        let digest = "b".repeat(64);
        let owner = std::process::id();
        let lease = pin(
            &run.join("run.lock"),
            &json!({"lease_id":lease_id,"owner_pid":owner,"plan_digest":digest}),
        );
        let lock = OpenOptions::new().read(true).open(&lease.path).unwrap();
        assert_eq!(
            unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );
        let binding = pin(
            &bindings.join("binding.json"),
            &json!({"schema":"environments-harness.storage-binding.v1",
        "plan":{"targets":{"temporary":temporary},"digest":digest,"run_state":run},
        "deadline_monotonic_ns":mono()+30_000_000_000u64,"lease_id":lease_id,"lease_owner_pid":owner}),
        );
        let metadata = std::fs::metadata(&endpoint).unwrap();
        let control = ServerControl {
            schema_version: SERVER_SCHEMA.into(),
            task_binding_sha256: "c".repeat(64),
            socket_parent: endpoint.clone(),
            parent_identity: NodeIdentity {
                device: metadata.dev(),
                inode: metadata.ino(),
                uid: metadata.uid(),
                gid: metadata.gid(),
                mode: 0o700,
            },
            client_visible_parent: endpoint.clone(),
            client_control_path: endpoint.join("client.json"),
            budget: FilePin {
                path: budget,
                sha256: format!("{:x}", Sha256::digest(budget_bytes)),
            },
            owner_binding: binding.clone(),
            peer: PeerScope::EngineeringDescendants {
                ancestor: process(owner),
            },
            model: gptgrep_jev::DEFAULT_MODEL.into(),
            provider_disabled: true,
        };
        let control = pin(&root.path().join("server.json"), &control);
        let mut command = cli();
        command
            .args([
                "decisions-helper",
                "serve",
                "--control",
                control.path.to_str().unwrap(),
                "--expected-control-sha256",
                &control.sha256,
            ])
            .env("ENVIRONMENTS_HARNESS_STORAGE_BINDING", &binding.path)
            .env("ENVIRONMENTS_HARNESS_STORAGE_DIGEST", &binding.sha256)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        unsafe {
            command.pre_exec(|| {
                libc::umask(0o077);
                Ok(())
            });
        }
        let mut child = command.spawn().unwrap();
        let pid = child.id();
        let stdout = child.stdout.take().unwrap();
        let (send, receive) = mpsc::channel();
        let reader = std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                if send.send(line.unwrap()).is_err() {
                    break;
                }
            }
        });
        let ready: String = receive.recv_timeout(Duration::from_secs(5)).unwrap();
        let ready: serde_json::Value = serde_json::from_str(&ready).unwrap();
        assert_eq!(ready["ready"], true);
        assert_eq!(ready["server_pid"], pid);
        let probe = cli()
            .args([
                "decisions-helper",
                "probe",
                "--control",
                endpoint.join("client.json").to_str().unwrap(),
                "--expected-control-sha256",
                ready["client_control_sha256"].as_str().unwrap(),
            ])
            .output()
            .unwrap();
        assert!(
            probe.status.success(),
            "{}",
            String::from_utf8_lossy(&probe.stdout)
        );
        assert!(probe.stderr.is_empty());
        let probe: serde_json::Value = serde_json::from_slice(&probe.stdout).unwrap();
        assert_eq!(probe["snapshot"]["logical_reservations"], 0);
        assert_eq!(unsafe { libc::kill(pid as i32, libc::SIGTERM) }, 0);
        let terminal = receive.recv_timeout(Duration::from_secs(5)).unwrap();
        let terminal: serde_json::Value = serde_json::from_str(&terminal).unwrap();
        assert_eq!(terminal["terminal"], true);
        assert_eq!(terminal["snapshot"]["logical_reservations"], 0);
        assert!(child.wait().unwrap().success());
        reader.join().unwrap();
        let stderr = child.stderr.take().unwrap();
        let mut stderr = BufReader::new(stderr);
        let mut line = String::new();
        assert_eq!(stderr.read_line(&mut line).unwrap(), 0);
        assert!(endpoint.join("decisions.sock").exists());
        assert!(!Path::new(&format!("/proc/{pid}")).exists());
    }
}
