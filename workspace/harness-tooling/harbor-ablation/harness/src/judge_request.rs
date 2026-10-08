//! Verifier-only preparation, structural admission and gated native source foundation.
use anyhow::{Context, Result, bail, ensure};
use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File},
    io::{Read, Seek, SeekFrom, Write},
    path::{Component, Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

const FORMATTER: &str = include_str!("../../bridge/judge_format.py");
const SOURCE_MAX: usize = 1_048_576;
const ARTIFACT_MAX: usize = 65_536;
const REQUEST_MAX: usize = 262_144;
const STDOUT_MAX: usize = 262_144;
const STDERR_MAX: usize = 16_384;
const FORMATTER_WALL: Duration = Duration::from_secs(10);
const QUALIFICATION: &str = "source_structure_only_original_judge_unqualified";

#[derive(Parser)]
#[command(
    about = "Verifier-only original judge requests; native execution policy unavailable; no rewards"
)]
struct Cli {
    #[command(subcommand)]
    command: Operation,
}
#[derive(Subcommand)]
enum Operation {
    /// Admit the fixed app-server broker recipe. --run refuses before effects.
    ExecuteAppServer {
        #[arg(long)]
        control: PathBuf,
        #[arg(long)]
        control_sha256: String,
        #[arg(long, default_value_t = false)]
        run: bool,
    },
    /// Admit a native source plan only. --run fails closed before effects.
    ExecuteNative {
        #[arg(long)]
        control: PathBuf,
        #[arg(long)]
        control_sha256: String,
        #[arg(long, default_value_t = false)]
        run: bool,
    },
    Prepare {
        #[arg(long)]
        judge_source: PathBuf,
        #[arg(long)]
        judge_source_sha256: String,
        #[arg(long)]
        original_revision: String,
        #[arg(long)]
        reference: PathBuf,
        #[arg(long)]
        reference_sha256: String,
        #[arg(long)]
        answer: PathBuf,
        #[arg(long)]
        answer_sha256: String,
        #[arg(long)]
        python: PathBuf,
        #[arg(long)]
        python_sha256: String,
        #[arg(long)]
        output_dir: PathBuf,
    },
    ValidateVerdict {
        #[arg(long)]
        request_root: PathBuf,
        #[arg(long)]
        request_manifest_sha256: String,
        #[arg(long)]
        verdict: PathBuf,
        #[arg(long)]
        verdict_sha256: String,
        #[arg(long)]
        python: PathBuf,
        #[arg(long)]
        python_sha256: String,
        #[arg(long)]
        output_dir: PathBuf,
    },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Reference {
    schema_version: String,
    question: String,
    answer: Value,
    answer_format: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Answer {
    answer: String,
}
#[derive(Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Formatted {
    schema_version: String,
    judge_source_sha256: String,
    reference_sha256: String,
    answer_sha256: String,
    prompt_template_sha256: String,
    prompt_sha256: String,
    verdict_schema_sha256: String,
    prompt: String,
    verdict_schema: Value,
    upstream_model: String,
    upstream_effort: String,
    max_response_characters: usize,
    original_response_characters: usize,
    used_response_characters: usize,
    response_truncated: bool,
}
#[derive(Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Requested {
    model: String,
    reasoning_effort: String,
    service_tier: String,
}
#[derive(Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    schema_version: String,
    judge_status: String,
    qualification: String,
    requested: Requested,
    actual: Option<Requested>,
    reward_emitted: bool,
    formatted: Formatted,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Binding {
    sha256: String,
    bytes: usize,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    schema_version: String,
    role: String,
    judge_status: String,
    qualification: String,
    original_revision: String,
    formatter_sha256: String,
    python_sha256: String,
    request: Binding,
    judge_source: Binding,
    reference: Binding,
    answer: Binding,
    actual_judge_model: Option<String>,
    actual_provider_calls: Option<u64>,
    actual_usage: Option<Value>,
    actual_cost: Option<f64>,
    reward_emitted: bool,
}
#[derive(Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Verdict {
    equivalent: bool,
    abstained: bool,
    reason: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct VerdictEnvelope {
    schema_version: String,
    request_sha256: String,
    judge_source_sha256: String,
    reference_sha256: String,
    answer_sha256: String,
    verdict_schema_sha256: String,
    verdict: Verdict,
}

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn hex(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn absolute(path: &Path) -> Result<PathBuf> {
    let path = if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir()?.join(path)
    };
    ensure!(
        path.components()
            .all(|c| matches!(c, Component::RootDir | Component::Normal(_))),
        "unsafe_path"
    );
    Ok(path)
}
#[cfg(unix)]
fn open_held(path: &Path, directory: bool) -> Result<File> {
    use std::os::fd::{AsRawFd, FromRawFd};
    let path = absolute(path)?;
    let parts = path
        .components()
        .filter_map(|c| {
            if let Component::Normal(n) = c {
                Some(n)
            } else {
                None
            }
        })
        .collect::<Vec<_>>();
    let mut held = File::open("/")?;
    for (i, name) in parts.iter().enumerate() {
        let name = std::ffi::CString::new(name.as_encoded_bytes())?;
        let flags = libc::O_RDONLY
            | libc::O_NOFOLLOW
            | libc::O_CLOEXEC
            | libc::O_NONBLOCK
            | if directory || i + 1 < parts.len() {
                libc::O_DIRECTORY
            } else {
                0
            };
        // SAFETY: live owned directory and NUL-terminated component; successful fd becomes one File.
        let fd = unsafe { libc::openat(held.as_raw_fd(), name.as_ptr(), flags) };
        ensure!(fd >= 0, "input_type_or_symlink");
        // SAFETY: successful openat returned a new owned descriptor.
        held = unsafe { File::from_raw_fd(fd) };
    }
    ensure!(
        if directory {
            held.metadata()?.is_dir()
        } else {
            held.metadata()?.is_file()
        },
        "input_type"
    );
    Ok(held)
}
#[cfg(not(unix))]
fn open_held(_: &Path, _: bool) -> Result<File> {
    bail!("capability_unavailable: Unix held input custody required")
}
#[cfg(unix)]
fn identity(file: &File) -> Result<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    let m = file.metadata()?;
    Ok((m.dev(), m.ino()))
}
#[cfg(not(unix))]
fn identity(_: &File) -> Result<(u64, u64)> {
    bail!("capability_unavailable: Unix identity required")
}
fn read_bounded(file: &mut File, cap: usize) -> Result<Vec<u8>> {
    ensure!(file.metadata()?.len() <= cap as u64, "input_bytes_limit");
    file.seek(SeekFrom::Start(0))?;
    let mut bytes = Vec::new();
    let mut block = [0; 4096];
    loop {
        let n = file.read(&mut block)?;
        if n == 0 {
            break;
        }
        ensure!(
            bytes.len().checked_add(n).is_some_and(|n| n <= cap),
            "input_bytes_limit"
        );
        bytes.try_reserve_exact(n)?;
        bytes.extend_from_slice(&block[..n]);
    }
    ensure!(!bytes.is_empty(), "input_empty");
    Ok(bytes)
}
struct Snapshot {
    path: PathBuf,
    file: File,
    id: (u64, u64),
    bytes: Vec<u8>,
    sha256: String,
}
impl Snapshot {
    fn load(path: &Path, expected: &str, cap: usize) -> Result<Self> {
        ensure!(hex(expected, 64), "invalid_digest");
        let path = absolute(path)?;
        let mut file = open_held(&path, false)?;
        let id = identity(&file)?;
        let bytes = read_bounded(&mut file, cap)?;
        let sha256 = digest(&bytes);
        ensure!(sha256 == expected, "input_digest_mismatch");
        Ok(Self {
            path,
            file,
            id,
            bytes,
            sha256,
        })
    }
    fn fresh(&self) -> Result<()> {
        ensure!(
            identity(&open_held(&self.path, false)?)? == self.id,
            "input_identity_changed"
        );
        let mut held = self.file.try_clone()?;
        ensure!(
            read_bounded(&mut held, self.bytes.len())? == self.bytes,
            "input_bytes_changed"
        );
        Ok(())
    }
    fn binding(&self) -> Binding {
        Binding {
            sha256: self.sha256.clone(),
            bytes: self.bytes.len(),
        }
    }
}
fn strict<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> Result<T> {
    serde_json::from_slice(bytes).map_err(|_| anyhow::anyhow!("invalid_typed_artifact"))
}
fn inputs_valid(reference: &Snapshot, answer: &Snapshot) -> Result<()> {
    let r: Reference = strict(&reference.bytes)?;
    let a: Answer = strict(&answer.bytes)?;
    ensure!(
        r.schema_version == "gptgrep.judge.reference.v1"
            && !r.question.trim().is_empty()
            && r.question.chars().count() <= 12000
            && !r.answer.is_null()
            && !r.answer_format.is_empty()
            && r.answer_format.chars().count() <= 128,
        "reference_contract"
    );
    ensure!(!a.answer.trim().is_empty(), "answer_contract");
    Ok(())
}
fn schema_valid(schema: &Value) -> Result<()> {
    let object = schema.as_object().context("verdict_schema_contract")?;
    ensure!(
        object.len() == 4
            && object.get("type") == Some(&Value::String("object".into()))
            && object.get("additionalProperties") == Some(&Value::Bool(false)),
        "verdict_schema_contract"
    );
    let props = object
        .get("properties")
        .and_then(Value::as_object)
        .context("verdict_schema_contract")?;
    let required = object
        .get("required")
        .and_then(Value::as_array)
        .context("verdict_schema_contract")?;
    ensure!(
        props.len() == 3
            && required.len() == 3
            && ["equivalent", "abstained", "reason"]
                .iter()
                .all(|n| required.iter().filter(|v| v.as_str() == Some(n)).count() == 1),
        "verdict_schema_contract"
    );
    for (name, kind) in [
        ("equivalent", "boolean"),
        ("abstained", "boolean"),
        ("reason", "string"),
    ] {
        let property = props
            .get(name)
            .and_then(Value::as_object)
            .context("verdict_schema_contract")?;
        ensure!(
            property.get("type").and_then(Value::as_str) == Some(kind)
                && property.keys().all(|k| k == "type" || k == "description")
                && property
                    .get("description")
                    .is_none_or(|d| d.as_str().is_some_and(|s| s.chars().count() <= 1024)),
            "verdict_schema_contract"
        );
    }
    Ok(())
}

fn capture<R: Read + Send + 'static>(
    mut reader: R,
    cap: usize,
    overflow: Arc<AtomicBool>,
) -> mpsc::Receiver<Result<Vec<u8>>> {
    let (send, receive) = mpsc::channel();
    thread::spawn(move || {
        let result = (|| {
            let mut output = Vec::new();
            let mut block = [0; 4096];
            loop {
                let n = reader.read(&mut block)?;
                if n == 0 {
                    break;
                }
                if output.len().checked_add(n).is_none_or(|n| n > cap) {
                    overflow.store(true, Ordering::SeqCst);
                    // Continue draining without retaining excess bytes until parent closes child.
                } else if !overflow.load(Ordering::SeqCst) {
                    output.try_reserve_exact(n)?;
                    output.extend_from_slice(&block[..n]);
                }
            }
            Ok(output)
        })();
        let _ = send.send(result);
    });
    receive
}
fn private_tempdir(parent: &Path, prefix: &str) -> Result<tempfile::TempDir> {
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        use std::os::unix::fs::PermissionsExt;
        let parent_fd = open_held(parent, true)?;
        let temporary = tempfile::Builder::new()
            .prefix(prefix)
            .permissions(fs::Permissions::from_mode(0o700))
            .tempdir_in(parent)?;
        // Creation never grants group/other rights. With umask 0777 the new
        // directory is mode 000, so restore owner rights relative to the held
        // parent without following a leaf symlink, before opening/writing it.
        let name = std::ffi::CString::new(
            temporary
                .path()
                .file_name()
                .context("private_directory_name")?
                .as_encoded_bytes(),
        )?;
        // SAFETY: parent_fd owns a directory; terminated name is the fresh leaf,
        // AT_SYMLINK_NOFOLLOW forbids changing a symlink target.
        let result = unsafe {
            libc::fchmodat(
                parent_fd.as_raw_fd(),
                name.as_ptr(),
                0o700,
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        ensure!(result == 0, "private_directory_mode");
        let held = open_held(temporary.path(), true)?;
        ensure!(
            held.metadata()?.permissions().mode() & 0o777 == 0o700,
            "private_directory_mode"
        );
        Ok(temporary)
    }
    #[cfg(not(unix))]
    bail!("capability_unavailable: private Unix directories required")
}
fn format(
    python: &Snapshot,
    source: &Snapshot,
    reference: &Snapshot,
    answer: &Snapshot,
    parent: &Path,
) -> Result<Formatted> {
    format_until(python, source, reference, answer, parent, None)
}
fn format_until(
    python: &Snapshot,
    source: &Snapshot,
    reference: &Snapshot,
    answer: &Snapshot,
    parent: &Path,
    admitted_deadline: Option<Instant>,
) -> Result<Formatted> {
    ensure!(
        admitted_deadline.is_none_or(|d| Instant::now() < d),
        "formatter_limit_or_deadline_unavailable"
    );
    python.fresh()?;
    source.fresh()?;
    reference.fresh()?;
    answer.fresh()?;
    let mut framed = Vec::new();
    let frame_size = source.bytes.len() + reference.bytes.len() + answer.bytes.len() + 24;
    ensure!(
        frame_size <= SOURCE_MAX + 2 * ARTIFACT_MAX + 24,
        "formatter_stdin_limit"
    );
    framed.try_reserve_exact(frame_size)?;
    for input in [&source.bytes, &reference.bytes, &answer.bytes] {
        framed.extend_from_slice(&(input.len() as u64).to_le_bytes());
        framed.extend_from_slice(input);
    }
    let cwd = private_tempdir(parent, ".judge-formatter-")?;
    let deadline = admitted_deadline.map_or(Instant::now() + FORMATTER_WALL, |d| {
        d.min(Instant::now() + FORMATTER_WALL)
    });
    ensure!(
        Instant::now() < deadline,
        "formatter_limit_or_deadline_unavailable"
    );
    let mut child = Command::new(&python.path)
        .args(["-I", "-S", "-c", FORMATTER])
        .env_clear()
        .env("LANG", "C.UTF-8")
        .env("LC_ALL", "C.UTF-8")
        .current_dir(cwd.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let overflow = Arc::new(AtomicBool::new(false));
    let stdout = capture(
        child.stdout.take().context("formatter_stdout")?,
        STDOUT_MAX,
        overflow.clone(),
    );
    let stderr = capture(
        child.stderr.take().context("formatter_stderr")?,
        STDERR_MAX,
        overflow.clone(),
    );
    let mut stdin = child.stdin.take().context("formatter_stdin")?;
    let (sent, sent_rx) = mpsc::channel();
    thread::spawn(move || {
        let result = stdin.write_all(&framed);
        drop(stdin);
        let _ = sent.send(result);
    });
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if overflow.load(Ordering::SeqCst) || Instant::now() >= deadline {
            let _ = child.kill();
            let cleanup = Instant::now();
            loop {
                if child.try_wait()?.is_some() {
                    bail!("formatter_limit_or_deadline_unavailable");
                }
                ensure!(
                    cleanup.elapsed() < Duration::from_secs(1),
                    "formatter_cleanup_unknown_no_retry"
                );
                thread::sleep(Duration::from_millis(10));
            }
        }
        thread::sleep(Duration::from_millis(10));
    };
    ensure!(
        Instant::now() < deadline,
        "formatter_limit_or_deadline_unavailable"
    );
    ensure!(
        status.success() && !overflow.load(Ordering::SeqCst),
        "formatter_failed_unavailable"
    );
    let remaining = deadline.saturating_duration_since(Instant::now());
    sent_rx
        .recv_timeout(remaining)
        .context("formatter_stdin_cleanup_unknown")??;
    let bytes = stdout
        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        .context("formatter_stdout_cleanup_unknown")??;
    let diagnostics = stderr
        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        .context("formatter_stderr_cleanup_unknown")??;
    ensure!(
        diagnostics.is_empty() && !overflow.load(Ordering::SeqCst),
        "formatter_diagnostics_unavailable"
    );
    python.fresh()?;
    source.fresh()?;
    reference.fresh()?;
    answer.fresh()?;
    let value: Formatted = strict(&bytes)?;
    schema_valid(&value.verdict_schema)?;
    let parsed_answer: Answer = strict(&answer.bytes)?;
    ensure!(
        value.schema_version == "gptgrep.judge.formatted.v1"
            && value.judge_source_sha256 == source.sha256
            && value.reference_sha256 == reference.sha256
            && value.answer_sha256 == answer.sha256
            && value.prompt_sha256 == digest(value.prompt.as_bytes())
            && value.verdict_schema_sha256 == digest(&serde_json::to_vec(&value.verdict_schema)?)
            && hex(&value.prompt_template_sha256, 64)
            && (1..=12000).contains(&value.max_response_characters)
            && value.original_response_characters == parsed_answer.answer.chars().count()
            && value.used_response_characters
                == value
                    .original_response_characters
                    .min(value.max_response_characters)
            && value.response_truncated
                == (value.used_response_characters < value.original_response_characters)
            && !value.upstream_model.is_empty()
            && value.upstream_model.len() <= 512
            && !value.upstream_effort.is_empty()
            && value.upstream_effort.len() <= 512,
        "formatter_response_binding"
    );
    ensure!(
        Instant::now() < deadline,
        "formatter_limit_or_deadline_unavailable"
    );
    Ok(value)
}
fn request(formatted: Formatted) -> Request {
    Request {
        schema_version: "gptgrep.judge.request.v1".into(),
        judge_status: "unavailable".into(),
        qualification: QUALIFICATION.into(),
        requested: Requested {
            model: "gpt-6-luna".into(),
            reasoning_effort: "max".into(),
            service_tier: "fast".into(),
        },
        actual: None,
        reward_emitted: false,
        formatted,
    }
}
struct Limited {
    bytes: Vec<u8>,
    cap: usize,
}
impl Write for Limited {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        if self
            .bytes
            .len()
            .checked_add(b.len())
            .is_none_or(|n| n > self.cap)
        {
            return Err(std::io::Error::other("output_bytes_limit"));
        }
        self.bytes
            .try_reserve_exact(b.len())
            .map_err(std::io::Error::other)?;
        self.bytes.extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
fn json_bytes(value: &impl Serialize, cap: usize) -> Result<Vec<u8>> {
    let mut writer = Limited {
        bytes: Vec::new(),
        cap,
    };
    serde_json::to_writer(&mut writer, value)?;
    Ok(writer.bytes)
}
fn fresh_output(path: &Path, inputs: &[&Path]) -> Result<PathBuf> {
    ensure!(
        cfg!(any(target_os = "linux", target_os = "macos")),
        "capability_unavailable: atomic fresh output requires Linux/macOS"
    );
    let path = absolute(path)?;
    // Reject lexical input/home overlap before inspecting an output path or parent.
    ensure!(
        !inputs
            .iter()
            .any(|p| p.starts_with(&path) || path.starts_with(p)),
        "output_input_overlap"
    );
    ensure!(fs::symlink_metadata(&path).is_err(), "output_must_be_fresh");
    let parent = path.parent().context("output_parent")?;
    open_held(parent, true)?;
    Ok(path)
}
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn rename_fresh(source: &Path, dest: &Path) -> Result<()> {
    let s = std::ffi::CString::new(source.as_os_str().as_encoded_bytes())?;
    let d = std::ffi::CString::new(dest.as_os_str().as_encoded_bytes())?;
    #[cfg(target_os = "linux")]
    // SAFETY: terminated absolute names, no replacement permitted.
    let result = unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            s.as_ptr(),
            libc::AT_FDCWD,
            d.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    #[cfg(target_os = "macos")]
    // SAFETY: terminated absolute names, no replacement permitted.
    let result = unsafe { libc::renamex_np(s.as_ptr(), d.as_ptr(), libc::RENAME_EXCL) };
    ensure!(result == 0, "fresh_publication_failed");
    Ok(())
}
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn rename_fresh(_: &Path, _: &Path) -> Result<()> {
    bail!("capability_unavailable: atomic fresh output")
}
fn publish(output: &Path, files: &[(&str, &[u8])], snapshots: &[&Snapshot]) -> Result<String> {
    publish_checked(output, files, snapshots, || Ok(()))
}
fn publish_checked(
    output: &Path,
    files: &[(&str, &[u8])],
    snapshots: &[&Snapshot],
    check: impl Fn() -> Result<()>,
) -> Result<String> {
    check()?;
    let input_paths = snapshots
        .iter()
        .map(|s| s.path.as_path())
        .collect::<Vec<_>>();
    let output = fresh_output(output, &input_paths)?;
    for snapshot in snapshots {
        snapshot.fresh()?;
    }
    let parent = output.parent().context("output_parent")?;
    let temp = private_tempdir(parent, ".judge-request-")?;
    for (name, data) in files {
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        let mut file = options.open(temp.path().join(name))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(fs::Permissions::from_mode(0o600))?;
            ensure!(
                file.metadata()?.permissions().mode() & 0o777 == 0o600,
                "private_file_mode"
            );
        }
        file.write_all(data)?;
        file.sync_all()?;
    }
    File::open(temp.path())?.sync_all()?;
    for snapshot in snapshots {
        snapshot.fresh()?;
    }
    fresh_output(&output, &input_paths)?;
    check()?;
    rename_fresh(temp.path(), &output)?;
    let _ = temp.keep();
    open_held(parent, true)
        .and_then(|d| d.sync_all().map_err(Into::into))
        .context("publication_sync_unknown_no_retry_output_may_exist")?;
    Ok(digest(files[0].1))
}
fn prepare(
    source: Snapshot,
    reference: Snapshot,
    answer: Snapshot,
    python: Snapshot,
    revision: &str,
    output: &Path,
) -> Result<String> {
    ensure!(hex(revision, 40), "original_revision");
    inputs_valid(&reference, &answer)?;
    ensure!(
        source.id != reference.id
            && source.id != answer.id
            && reference.id != answer.id
            && python.id != source.id
            && python.id != reference.id
            && python.id != answer.id,
        "input_alias"
    );
    let output = fresh_output(
        output,
        &[&source.path, &reference.path, &answer.path, &python.path],
    )?;
    let formed = format(
        &python,
        &source,
        &reference,
        &answer,
        output.parent().context("output_parent")?,
    )?;
    let request_bytes = json_bytes(&request(formed), REQUEST_MAX)?;
    let manifest = Manifest {
        schema_version: "gptgrep.judge.request-manifest.v1".into(),
        role: "verifier_only".into(),
        judge_status: "unavailable".into(),
        qualification: QUALIFICATION.into(),
        original_revision: revision.into(),
        formatter_sha256: digest(FORMATTER.as_bytes()),
        python_sha256: python.sha256.clone(),
        request: Binding {
            sha256: digest(&request_bytes),
            bytes: request_bytes.len(),
        },
        judge_source: source.binding(),
        reference: reference.binding(),
        answer: answer.binding(),
        actual_judge_model: None,
        actual_provider_calls: None,
        actual_usage: None,
        actual_cost: None,
        reward_emitted: false,
    };
    let manifest_bytes = json_bytes(&manifest, ARTIFACT_MAX)?;
    publish(
        &output,
        &[
            ("request-manifest.json", &manifest_bytes),
            ("request.json", &request_bytes),
            ("judge-source.py", &source.bytes),
            ("reference.json", &reference.bytes),
            ("answer.json", &answer.bytes),
        ],
        &[&source, &reference, &answer, &python],
    )
}
fn load_binding(root: &Path, name: &str, binding: &Binding, cap: usize) -> Result<Snapshot> {
    ensure!(
        binding.bytes > 0 && binding.bytes <= cap,
        "manifest_binding_limit"
    );
    let snapshot = Snapshot::load(&root.join(name), &binding.sha256, cap)?;
    ensure!(
        snapshot.bytes.len() == binding.bytes,
        "manifest_binding_bytes"
    );
    Ok(snapshot)
}
/// Immutable retained source custody. Loading has no formatter/native/output effects.
/// Structural preflight is not Python formatter parity or execution authority.
struct PreparedRequest {
    root: PathBuf,
    held_root: File,
    root_id: (u64, u64),
    manifest: Snapshot,
    source: Snapshot,
    reference: Snapshot,
    answer: Snapshot,
    saved: Snapshot,
    python: Snapshot,
    decoded: Request,
}
impl PreparedRequest {
    fn load(root: &Path, manifest_sha: &str, python: Snapshot) -> Result<Self> {
        let root = absolute(root)?;
        let held_root = open_held(&root, true)?;
        let root_id = identity(&held_root)?;
        let manifest = Snapshot::load(
            &root.join("request-manifest.json"),
            manifest_sha,
            ARTIFACT_MAX,
        )?;
        let m: Manifest = strict(&manifest.bytes)?;
        ensure!(
            m.schema_version == "gptgrep.judge.request-manifest.v1"
                && m.role == "verifier_only"
                && m.judge_status == "unavailable"
                && m.qualification == QUALIFICATION
                && hex(&m.original_revision, 40)
                && m.formatter_sha256 == digest(FORMATTER.as_bytes())
                && m.python_sha256 == python.sha256
                && m.actual_judge_model.is_none()
                && m.actual_provider_calls.is_none()
                && m.actual_usage.is_none()
                && m.actual_cost.is_none()
                && !m.reward_emitted,
            "request_manifest_contract"
        );
        let source = load_binding(&root, "judge-source.py", &m.judge_source, SOURCE_MAX)?;
        let reference = load_binding(&root, "reference.json", &m.reference, ARTIFACT_MAX)?;
        let answer = load_binding(&root, "answer.json", &m.answer, ARTIFACT_MAX)?;
        let saved = load_binding(&root, "request.json", &m.request, REQUEST_MAX)?;
        inputs_valid(&reference, &answer)?;
        let decoded: Request = strict(&saved.bytes)?;
        schema_valid(&decoded.formatted.verdict_schema)?;
        let parsed_answer: Answer = strict(&answer.bytes)?;
        let f = &decoded.formatted;
        ensure!(
            decoded.schema_version == "gptgrep.judge.request.v1"
                && decoded.judge_status == "unavailable"
                && decoded.qualification == QUALIFICATION
                && decoded.requested
                    == Requested {
                        model: "gpt-6-luna".into(),
                        reasoning_effort: "max".into(),
                        service_tier: "fast".into()
                    }
                && decoded.actual.is_none()
                && !decoded.reward_emitted
                && f.schema_version == "gptgrep.judge.formatted.v1"
                && f.judge_source_sha256 == source.sha256
                && f.reference_sha256 == reference.sha256
                && f.answer_sha256 == answer.sha256
                && f.prompt_sha256 == digest(f.prompt.as_bytes())
                && f.verdict_schema_sha256 == digest(&serde_json::to_vec(&f.verdict_schema)?)
                && hex(&f.prompt_template_sha256, 64)
                && !f.prompt.trim().is_empty()
                && f.prompt.len() <= REQUEST_MAX
                && (1..=12000).contains(&f.max_response_characters)
                && f.original_response_characters == parsed_answer.answer.chars().count()
                && f.used_response_characters
                    == f.original_response_characters
                        .min(f.max_response_characters)
                && f.response_truncated
                    == (f.used_response_characters < f.original_response_characters)
                && !f.upstream_model.is_empty()
                && f.upstream_model.len() <= 512
                && !f.upstream_effort.is_empty()
                && f.upstream_effort.len() <= 512,
            "request_format_or_qualification_mismatch"
        );
        let value = Self {
            root,
            held_root,
            root_id,
            manifest,
            source,
            reference,
            answer,
            saved,
            python,
            decoded,
        };
        let snapshots = value.snapshots();
        for (index, a) in snapshots.iter().enumerate() {
            ensure!(
                snapshots.iter().skip(index + 1).all(|b| a.id != b.id),
                "input_alias"
            );
        }
        value.fresh()?;
        Ok(value)
    }
    fn reconstruct(&self, parent: &Path, deadline: Instant) -> Result<()> {
        self.fresh()?;
        let formed = format_until(
            &self.python,
            &self.source,
            &self.reference,
            &self.answer,
            parent,
            Some(deadline),
        )?;
        ensure!(
            self.decoded == request(formed),
            "request_format_or_qualification_mismatch"
        );
        self.fresh()?;
        ensure!(
            Instant::now() < deadline,
            "formatter_limit_or_deadline_unavailable"
        );
        Ok(())
    }
    fn snapshots(&self) -> [&Snapshot; 6] {
        [
            &self.manifest,
            &self.source,
            &self.reference,
            &self.answer,
            &self.saved,
            &self.python,
        ]
    }
    fn fresh(&self) -> Result<()> {
        ensure!(
            identity(&self.held_root)? == self.root_id
                && identity(&open_held(&self.root, true)?)? == self.root_id,
            "request_root_changed"
        );
        for snapshot in self.snapshots() {
            snapshot.fresh()?;
        }
        Ok(())
    }
}

fn validate_verdict(
    root: &Path,
    manifest_sha: &str,
    verdict: Snapshot,
    python: Snapshot,
    output: &Path,
) -> Result<String> {
    let prepared = PreparedRequest::load(root, manifest_sha, python)?;
    let PreparedRequest {
        root,
        held_root: _held_root,
        root_id,
        manifest,
        source,
        reference,
        answer,
        saved,
        python,
        decoded: actual,
    } = prepared;
    let output = fresh_output(output, &[&root, &verdict.path, &python.path])?;
    let formed = format(
        &python,
        &source,
        &reference,
        &answer,
        output.parent().context("output_parent")?,
    )?;
    let expected = request(formed);
    ensure!(
        actual == expected,
        "request_format_or_qualification_mismatch"
    );
    let v: VerdictEnvelope = strict(&verdict.bytes)?;
    ensure!(
        v.schema_version == "gptgrep.judge.verdict.v1"
            && v.request_sha256 == saved.sha256
            && v.judge_source_sha256 == source.sha256
            && v.reference_sha256 == reference.sha256
            && v.answer_sha256 == answer.sha256
            && v.verdict_schema_sha256 == expected.formatted.verdict_schema_sha256
            && v.verdict.reason.len() <= STDERR_MAX,
        "verdict_binding_or_size"
    );
    ensure!(
        identity(&open_held(&root, true)?)? == root_id,
        "request_root_changed"
    );
    let bytes = json_bytes(
        &serde_json::json!({"schema_version":"gptgrep.judge.verdict-validation.v1",
        "validation_status":"typed_structural_only", "judge_status":"unavailable", "qualification":QUALIFICATION,
        "request_manifest_sha256":manifest.sha256,"request_sha256":saved.sha256,"judge_source_sha256":source.sha256,
        "reference_sha256":reference.sha256,"answer_sha256":answer.sha256,"verdict_schema_sha256":v.verdict_schema_sha256,
        "verdict_artifact_sha256":verdict.sha256,"verdict":v.verdict,"actual_judge_model":null,
        "actual_provider_calls":null,"actual_usage":null,"actual_cost":null,"reward_emitted":false}),
        ARTIFACT_MAX,
    )?;
    publish(
        &output,
        &[("validation.json", &bytes)],
        &[
            &manifest, &source, &reference, &answer, &saved, &verdict, &python,
        ],
    )
}
fn run(operation: Operation) -> Result<String> {
    match operation {
        Operation::ExecuteAppServer {
            control,
            control_sha256,
            run,
        } => judge_backend::dispatch_app_server(&control, &control_sha256, run),
        Operation::ExecuteNative {
            control,
            control_sha256,
            run,
        } => judge_backend::dispatch(&control, &control_sha256, run),
        Operation::Prepare {
            judge_source,
            judge_source_sha256,
            original_revision,
            reference,
            reference_sha256,
            answer,
            answer_sha256,
            python,
            python_sha256,
            output_dir,
        } => prepare(
            Snapshot::load(&judge_source, &judge_source_sha256, SOURCE_MAX)?,
            Snapshot::load(&reference, &reference_sha256, ARTIFACT_MAX)?,
            Snapshot::load(&answer, &answer_sha256, ARTIFACT_MAX)?,
            Snapshot::load(&python, &python_sha256, SOURCE_MAX)?,
            &original_revision,
            &output_dir,
        ),
        Operation::ValidateVerdict {
            request_root,
            request_manifest_sha256,
            verdict,
            verdict_sha256,
            python,
            python_sha256,
            output_dir,
        } => validate_verdict(
            &request_root,
            &request_manifest_sha256,
            Snapshot::load(&verdict, &verdict_sha256, ARTIFACT_MAX)?,
            Snapshot::load(&python, &python_sha256, SOURCE_MAX)?,
            &output_dir,
        ),
    }
}
fn failure_code(error: &anyhow::Error) -> &'static str {
    for (message, code) in [
        (
            "native_execution_policy_unavailable",
            "native_execution_policy_unavailable",
        ),
        (
            "native_invocation_consumed",
            "native_invocation_consumed_no_retry",
        ),
        (
            "native_completion_unknown_no_retry",
            "native_completion_unknown_no_retry",
        ),
        (
            "publication_sync_unknown_no_retry_output_may_exist",
            "publication_sync_unknown_no_retry_output_may_exist",
        ),
        (
            "formatter_cleanup_unknown_no_retry",
            "formatter_cleanup_unknown_no_retry",
        ),
        (
            "formatter_stdin_cleanup_unknown",
            "formatter_pipe_cleanup_unknown_no_retry",
        ),
        (
            "formatter_stdout_cleanup_unknown",
            "formatter_pipe_cleanup_unknown_no_retry",
        ),
        (
            "formatter_stderr_cleanup_unknown",
            "formatter_pipe_cleanup_unknown_no_retry",
        ),
        (
            "formatter_limit_or_deadline_unavailable",
            "formatter_limit_or_deadline_unavailable",
        ),
    ] {
        if error.chain().any(|e| e.to_string() == message) {
            return code;
        }
    }
    "judge_request_or_verdict_unavailable"
}
fn main() {
    let operation = Cli::parse().command;
    let native = matches!(
        &operation,
        Operation::ExecuteNative { .. } | Operation::ExecuteAppServer { .. }
    );
    match run(operation) {
        Ok(response) if native => println!("{response}"),
        Ok(sha) => println!(
            "{}",
            serde_json::json!({"artifact_sha256":sha,"judge_status":"unavailable","qualification":QUALIFICATION,"reward_emitted":false})
        ),
        Err(error) => {
            println!(
                "{}",
                serde_json::json!({"error_code":failure_code(&error),"judge_status":"unavailable","reward_emitted":false})
            );
            std::process::exit(75);
        }
    }
}
#[path = "judge_backend.rs"]
mod judge_backend;

#[cfg(test)]
#[path = "judge_request_tests.rs"]
mod tests;
