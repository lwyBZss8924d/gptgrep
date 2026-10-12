#![cfg(unix)]
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

fn live(pid: i32) -> bool {
    let output = Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "stat="])
        .output()
        .unwrap();
    let state = String::from_utf8_lossy(&output.stdout);
    !state.trim().is_empty() && !state.trim().starts_with('Z')
}

#[test]
fn termination_cancels_host_and_its_launcher_descendant() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let binary = env!("CARGO_BIN_EXE_gptgrep");
    let input = root.join("completion.json");
    fs::write(
        &input,
        r#"{"instructions":"Return the value true.","state":{},"schema":{"type":"boolean"}}"#,
    )
    .unwrap();
    let launcher = root.join("mock-codex.sh");
    // The owned fixture ignores protocol input so the host is pending at initialization.
    let script = "#!/bin/sh\necho $$ > \"$GPTGREP_TEST_PID_DIR/server.pid.pending\"\nmv \"$GPTGREP_TEST_PID_DIR/server.pid.pending\" \"$GPTGREP_TEST_PID_DIR/server.pid\"\nsleep 60 &\necho $! > \"$GPTGREP_TEST_PID_DIR/descendant.pid.pending\"\nmv \"$GPTGREP_TEST_PID_DIR/descendant.pid.pending\" \"$GPTGREP_TEST_PID_DIR/descendant.pid\"\nwait\n";
    fs::write(&launcher, script).unwrap();
    fs::set_permissions(&launcher, fs::Permissions::from_mode(0o700)).unwrap();
    let mut child = Command::new(binary)
        .args(["host-complete", "--input"])
        .arg(&input)
        .arg("--codex-bin")
        .arg(&launcher)
        .arg("--codex-home")
        .arg(root)
        .args(["--timeout", "30", "--json"])
        .env("GPTGREP_TEST_PID_DIR", root)
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !root.join("descendant.pid").exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
    if !root.join("descendant.pid").exists() {
        let _ = child.kill();
        panic!("mock host did not start");
    }
    let pids: Vec<i32> = ["server.pid", "descendant.pid"]
        .iter()
        .map(|file| {
            fs::read_to_string(root.join(file))
                .unwrap()
                .trim()
                .parse()
                .unwrap()
        })
        .collect();
    // SAFETY: child.id() is the exact process spawned by this test, still held by Child.
    assert_eq!(unsafe { libc::kill(child.id() as i32, libc::SIGTERM) }, 0);
    let deadline = Instant::now() + Duration::from_secs(5);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            for pid in &pids {
                unsafe {
                    libc::kill(*pid, libc::SIGKILL);
                }
            }
            panic!("CLI did not exit after SIGTERM");
        }
        thread::sleep(Duration::from_millis(20));
    };
    assert_eq!(status.code(), Some(130));
    let deadline = Instant::now() + Duration::from_secs(3);
    while pids.iter().any(|p| live(*p)) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
    let remaining: Vec<_> = pids.iter().copied().filter(|p| live(*p)).collect();
    for pid in &remaining {
        unsafe {
            libc::kill(*pid, libc::SIGKILL);
        }
    }
    assert!(
        remaining.is_empty(),
        "host descendants remained live: {remaining:?}"
    );
}
