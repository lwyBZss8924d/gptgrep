//! Foreground keyholder only; ordinary commands never start a helper.
use anyhow::{Result, anyhow, ensure};
use clap::Subcommand;
use gptgrep_jev::helper_ipc::{self, FilePin};
use serde_json::{Value, json};
#[cfg(target_os = "linux")]
use std::{io::Read, time::Instant};
use std::{io::Write, path::PathBuf, time::Duration};

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Serve one private frozen budget; live credentials arrive only through stdin.
    Serve {
        #[arg(long, value_name = "PRIVATE_JSON")]
        control: PathBuf,
        #[arg(long, value_name = "SHA256")]
        expected_control_sha256: String,
        #[arg(
            long,
            help = "Required for a live helper; bounded credential bytes followed by EOF"
        )]
        key_stdin: bool,
    },
    /// Probe a provider-disabled helper without reserving or sending a decision.
    Probe {
        #[arg(long, value_name = "PRIVATE_JSON")]
        control: PathBuf,
        #[arg(long, value_name = "SHA256")]
        expected_control_sha256: String,
    },
}

fn emit(value: &Value) -> Result<()> {
    let mut stdout = std::io::stdout().lock();
    serde_json::to_writer(&mut stdout, value)?;
    stdout.write_all(b"\n")?;
    stdout.flush()?;
    Ok(())
}

#[cfg(target_os = "linux")]
fn read_key(remaining: Duration) -> Result<String> {
    use std::os::fd::{AsRawFd, FromRawFd};
    // Own and close stdin before any client is admitted. Poll every underlying
    // read against this one original deadline; neither partial input nor EINTR renews it.
    let mut stdin = unsafe { std::fs::File::from_raw_fd(0) };
    let deadline = Instant::now() + remaining.min(gptgrep_jev::REQUEST_TIMEOUT);
    let mut bytes = Vec::new();
    loop {
        let left = deadline
            .checked_duration_since(Instant::now())
            .ok_or_else(|| anyhow!("Helper key input expired"))?;
        let mut poll = libc::pollfd {
            fd: stdin.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let result = unsafe {
            libc::poll(
                &mut poll,
                1,
                left.as_millis().saturating_add(1).min(i32::MAX as u128) as i32,
            )
        };
        if result < 0 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
            continue;
        }
        ensure!(result > 0, "Helper key input expired");
        let mut part = [0u8; 1024];
        let count = stdin
            .read(&mut part)
            .map_err(|_| anyhow!("Helper key input unavailable"))?;
        if count == 0 {
            break;
        }
        ensure!(
            bytes.len().saturating_add(count) <= 8192,
            "Helper key input exceeded its bound"
        );
        bytes.extend_from_slice(&part[..count]);
    }
    if bytes.last() == Some(&b'\n') {
        bytes.pop();
        if bytes.last() == Some(&b'\r') {
            bytes.pop();
        }
    }
    ensure!(
        !bytes.is_empty()
            && !bytes
                .iter()
                .any(|value| value.is_ascii_whitespace() || value.is_ascii_control()),
        "Helper key input is invalid"
    );
    String::from_utf8(bytes).map_err(|_| anyhow!("Helper key input is invalid"))
}
#[cfg(not(target_os = "linux"))]
fn read_key(_: Duration) -> Result<String> {
    Err(anyhow!("Decisions helper IPC requires Linux"))
}

pub async fn run(command: Command, incompatible_budget: bool) -> Result<i32> {
    let mut output_key = None;
    let result:Result<()> = async {
        ensure!(!incompatible_budget,"Helper commands take their own fixed budget control");
        match command {
            Command::Serve {control,expected_control_sha256,key_stdin}=>{
                helper_ipc::harden_keyholder()?;
                let control=helper_ipc::read_server_control(&FilePin {path:control,sha256:expected_control_sha256})?;
                ensure!(key_stdin != control.provider_disabled,
                    "Live helper requires stdin; provider-disabled helper accepts no key");
                let remaining=helper_ipc::key_input_remaining(&control)?;
                let key=if key_stdin {Some(read_key(remaining)?)} else {
                    #[cfg(target_os="linux")]
                    unsafe {libc::close(0);}
                    None
                };
                output_key=key.clone();
                let terminal=helper_ipc::serve_with_readiness(control,key,crate::shutdown_signal(),|ready|emit(&ready)).await?;
                emit(&terminal)?;
            }
            Command::Probe {control,expected_control_sha256}=>{
                let control=helper_ipc::read_client_control(&FilePin {path:control,sha256:expected_control_sha256})?;
                let snapshot=helper_ipc::probe(&control).await?;
                emit(&json!({"schema_version":"gptgrep.decisions-helper-probe.v1","provider_disabled":true,
                    "probe_only":true,"snapshot":snapshot}))?;
            }
        }
        Ok(())
    }.await;
    match result {
        Ok(()) => Ok(0),
        Err(_) => {
            // Dynamic OS/provider/control errors never cross the keyholder's first output write.
            let error = json!({"schema_version":"gptgrep.error.v1","ok":false,
                "code":"decisions_helper_failed","error":"Decisions helper refused startup or its original bounded operation"});
            if helper_ipc::keyholder_metadata_safe(output_key.as_deref(), &error) {
                emit(&error)?;
            }
            Ok(2)
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::{
        io::{BufRead, BufReader},
        process::{Command as ProcessCommand, Stdio},
        sync::mpsc,
    };
    const CASE: &str = "GPTGREP_HELPER_PUBLIC_STDIN_FIXTURE";
    const PUBLIC_KEY: &str = "public-synthetic-stdin-key-20261027";

    #[test]
    fn stdin_child() {
        let Ok(case) = std::env::var(CASE) else {
            return;
        };
        // This is a private test process, never a public helper option.
        unsafe {
            libc::umask(0o077);
        }
        helper_ipc::harden_keyholder().unwrap();
        println!("helper-stdin-fixture-ready");
        std::io::stdout().flush().unwrap();
        let start = Instant::now();
        let result = read_key(Duration::from_millis(500));
        assert_eq!(
            unsafe { libc::fcntl(0, libc::F_GETFD) },
            -1,
            "stdin remained open after ingestion"
        );
        match case.as_str() {
            "valid" => assert!(result.is_ok_and(|value| value == PUBLIC_KEY)),
            "too_long" | "invalid" | "empty" => assert!(result.is_err()),
            "stall" => {
                assert!(result.is_err());
                assert!(start.elapsed() >= Duration::from_millis(450));
            }
            _ => panic!("unknown public stdin fixture"),
        }
        assert!(start.elapsed() < Duration::from_secs(2));
        println!("helper-stdin-fixture-complete");
    }

    #[test]
    fn synthetic_stdin_is_bounded_closed_and_process_protected() {
        for case in ["valid", "too_long", "invalid", "empty", "stall"] {
            let mut command = ProcessCommand::new(std::env::current_exe().unwrap());
            command
                .args([
                    "--exact",
                    "decisions_helper::tests::stdin_child",
                    "--nocapture",
                ])
                .env_clear()
                .env(CASE, case)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            let mut child = command.spawn().unwrap();
            let pid = child.id();
            let stdout = child.stdout.take().unwrap();
            let (send, receive) = mpsc::channel();
            let reader = std::thread::spawn(move || {
                let mut output = String::new();
                for line in BufReader::new(stdout).lines() {
                    let line = line.unwrap();
                    output.push_str(&line);
                    output.push('\n');
                    let _ = send.send(line);
                }
                output
            });
            loop {
                let line = receive.recv_timeout(Duration::from_secs(2)).unwrap();
                if line.contains("helper-stdin-fixture-ready") {
                    break;
                }
            }
            // Inspect access denials, never process memory, descriptor contents or credentials.
            for leaf in ["mem", "environ"] {
                let error = std::fs::File::open(format!("/proc/{pid}/{leaf}"))
                    .err()
                    .expect("hardened process was readable");
                assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
            }
            assert_eq!(
                std::fs::read_link(format!("/proc/{pid}/fd/0"))
                    .unwrap_err()
                    .kind(),
                std::io::ErrorKind::PermissionDenied
            );
            let argv = std::fs::read(format!("/proc/{pid}/cmdline")).unwrap();
            assert!(
                !argv
                    .windows(PUBLIC_KEY.len())
                    .any(|value| value == PUBLIC_KEY.as_bytes())
            );
            let mut input = child.stdin.take().unwrap();
            match case {
                "valid" => {
                    input.write_all(PUBLIC_KEY.as_bytes()).unwrap();
                    input.write_all(b"\n").unwrap();
                }
                "too_long" => input.write_all(&vec![b'x'; 8193]).unwrap(),
                "invalid" => input.write_all(b"public invalid whitespace\n").unwrap(),
                "empty" => {}
                "stall" => input.write_all(b"p").unwrap(),
                _ => unreachable!(),
            }
            if case != "stall" {
                drop(input);
            } else {
                // Keep the pipe open; a partial byte must not renew the original deadline.
                std::thread::sleep(Duration::from_millis(650));
                drop(input);
            }
            let deadline = Instant::now() + Duration::from_secs(3);
            let status = loop {
                if let Some(status) = child.try_wait().unwrap() {
                    break status;
                }
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("bounded stdin child did not terminate");
                }
                std::thread::sleep(Duration::from_millis(10));
            };
            let output = reader.join().unwrap();
            let mut error = String::new();
            child
                .stderr
                .take()
                .unwrap()
                .read_to_string(&mut error)
                .unwrap();
            assert!(
                status.success(),
                "synthetic stdin child failed with metadata-only output: {output} {error}"
            );
            assert!(!output.contains(PUBLIC_KEY) && !error.contains(PUBLIC_KEY));
        }
    }
}
