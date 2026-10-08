//! New synthetic request/verdict tests only. Qualified Python supplied explicitly.
use super::*;
use serde_json::json;

const PYTHON_ENV: &str = "GPTGREP_JUDGE_TEST_PYTHON";
const PYTHON_SHA_ENV: &str = "GPTGREP_JUDGE_TEST_PYTHON_SHA256";
const REVISION: &str = "1111111111111111111111111111111111111111";
const SOURCE: &str = "raise RuntimeError('source must not execute')\nPROMPT = 'Q={question}|A={answer}|F={answer_format}|R={response}'\nSCHEMA = {'type':'object','properties':{'equivalent':{'type':'boolean'},'abstained':{'type':'boolean'},'reason':{'type':'string'}},'required':['equivalent','abstained','reason'],'additionalProperties':False}\nMAX_RESPONSE_CHARS = 12000\nMODEL = 'source-history'\nEFFORT = 'source-effort'\n";
pub(super) struct Fixture {
    _temp: tempfile::TempDir,
    pub(super) root: PathBuf,
    pub(super) source: PathBuf,
    pub(super) reference: PathBuf,
    pub(super) answer: PathBuf,
    pub(super) output: PathBuf,
}
impl Fixture {
    pub(super) fn new() -> Result<Self> {
        let temp = tempfile::tempdir()?;
        let root = temp.path().canonicalize()?;
        let source = root.join("inert-source.py");
        let reference = root.join("reference.json");
        let answer = root.join("answer.json");
        fs::write(&source, SOURCE)?;
        fs::write(
            &reference,
            serde_json::to_vec(&json!({"schema_version":"gptgrep.judge.reference.v1",
            "question":"  synthetic\t\u{1c} question\u{2003} text ", "answer":[true,null,"generic"], "answer_format":"generic"}))?,
        )?;
        fs::write(&answer, serde_json::to_vec(&json!({"answer":"中🙂x"}))?)?;
        let output = root.join("prepared");
        Ok(Self {
            _temp: temp,
            root,
            source,
            reference,
            answer,
            output,
        })
    }
    pub(super) fn load(&self, path: &Path, cap: usize) -> Result<Snapshot> {
        Snapshot::load(path, &digest(&fs::read(path)?), cap)
    }
    pub(super) fn python(&self) -> Result<Snapshot> {
        let path = PathBuf::from(
            std::env::var(PYTHON_ENV).context("explicit qualified test Python required")?,
        );
        Snapshot::load(
            &path,
            &std::env::var(PYTHON_SHA_ENV)
                .context("explicit qualified test Python digest required")?,
            SOURCE_MAX,
        )
    }
    pub(super) fn prepare(&self) -> Result<String> {
        prepare(
            self.load(&self.source, SOURCE_MAX)?,
            self.load(&self.reference, ARTIFACT_MAX)?,
            self.load(&self.answer, ARTIFACT_MAX)?,
            self.python()?,
            REVISION,
            &self.output,
        )
    }
    fn verdict(&self, equivalent: bool, abstained: bool) -> Result<PathBuf> {
        let raw = fs::read(self.output.join("request.json"))?;
        let r: Request = strict(&raw)?;
        let v = json!({"schema_version":"gptgrep.judge.verdict.v1", "request_sha256":digest(&raw),
            "judge_source_sha256":r.formatted.judge_source_sha256,"reference_sha256":r.formatted.reference_sha256,
            "answer_sha256":r.formatted.answer_sha256,"verdict_schema_sha256":r.formatted.verdict_schema_sha256,
            "verdict":{"equivalent":equivalent,"abstained":abstained,"reason":""}});
        let path = self.root.join("verdict.json");
        fs::write(&path, serde_json::to_vec(&v)?)?;
        Ok(path)
    }
    fn validate(&self, verdict: &Path, output: &Path) -> Result<String> {
        validate_verdict(
            &self.output,
            &digest(&fs::read(self.output.join("request-manifest.json"))?),
            self.load(verdict, ARTIFACT_MAX)?,
            self.python()?,
            output,
        )
    }
}

#[test]
fn prepare_preserves_python_semantics_and_unavailable_identity() -> Result<()> {
    let f = Fixture::new()?;
    let manifest_sha = f.prepare()?;
    assert_eq!(
        manifest_sha,
        digest(&fs::read(f.output.join("request-manifest.json"))?)
    );
    let request: Request = strict(&fs::read(f.output.join("request.json"))?)?;
    assert_eq!(
        request.formatted.prompt,
        "Q=synthetic question text|A=[True, None, 'generic']|F=generic|R=中🙂x"
    );
    assert_eq!(request.requested.model, "gpt-6-luna");
    assert_eq!(request.requested.reasoning_effort, "max");
    assert_eq!(request.requested.service_tier, "fast");
    assert_eq!(request.actual, None);
    assert_eq!(request.judge_status, "unavailable");
    assert!(!request.reward_emitted);
    assert_eq!(
        fs::read(&f.answer)?,
        fs::read(f.output.join("answer.json"))?
    );
    assert_eq!(
        fs::read(&f.reference)?,
        fs::read(f.output.join("reference.json"))?
    );
    assert!(!f.output.join("reward.txt").exists());
    assert!(!f.output.join("reward.json").exists());
    Ok(())
}
#[test]
fn response_truncation_uses_codepoints_and_retains_full_answer() -> Result<()> {
    let f = Fixture::new()?;
    fs::write(&f.source, SOURCE.replace("12000", "2"))?;
    f.prepare()?;
    let r: Request = strict(&fs::read(f.output.join("request.json"))?)?;
    assert!(r.formatted.prompt.ends_with("R=中🙂"));
    assert_eq!(r.formatted.original_response_characters, 3);
    assert_eq!(r.formatted.used_response_characters, 2);
    assert!(r.formatted.response_truncated);
    assert_eq!(r.formatted.answer_sha256, digest(&fs::read(&f.answer)?));
    Ok(())
}
#[test]
fn typed_verdict_retains_all_boolean_combinations_without_reward() -> Result<()> {
    let f = Fixture::new()?;
    f.prepare()?;
    for (n, (equivalent, abstained)) in [(false, false), (false, true), (true, false), (true, true)]
        .into_iter()
        .enumerate()
    {
        let v = f.verdict(equivalent, abstained)?;
        let out = f.root.join(format!("validated-{n}"));
        f.validate(&v, &out)?;
        let value: Value = strict(&fs::read(out.join("validation.json"))?)?;
        assert_eq!(value["verdict"]["equivalent"], equivalent);
        assert_eq!(value["verdict"]["abstained"], abstained);
        assert_eq!(value["judge_status"], "unavailable");
        assert_eq!(value["reward_emitted"], false);
        assert!(value["actual_judge_model"].is_null());
        assert!(value["actual_provider_calls"].is_null());
        assert!(value["actual_usage"].is_null());
        assert!(value["actual_cost"].is_null());
        assert!(!out.join("reward.txt").exists());
        assert!(!out.join("reward.json").exists());
    }
    Ok(())
}
#[test]
fn missing_malformed_empty_unknown_and_oversized_answers_have_no_output() -> Result<()> {
    let f = Fixture::new()?;
    for value in [
        b"{}".as_slice(),
        b"{bad}",
        b"{\"answer\":false}",
        b"{\"answer\":\" \"}",
        b"{\"answer\":\"text\",\"gold\":[]}",
    ] {
        fs::write(&f.answer, value)?;
        assert!(f.prepare().is_err());
        assert!(!f.output.exists());
    }
    fs::write(&f.answer, vec![b'x'; ARTIFACT_MAX + 1])?;
    assert!(f.prepare().is_err());
    fs::remove_file(&f.answer)?;
    assert!(f.prepare().is_err());
    assert!(!f.output.exists());
    Ok(())
}
#[test]
fn input_digest_source_limit_and_private_snapshot_mutation_refuse() -> Result<()> {
    let f = Fixture::new()?;
    assert!(Snapshot::load(&f.answer, &"0".repeat(64), ARTIFACT_MAX).is_err());
    let held = f.load(&f.answer, ARTIFACT_MAX)?;
    fs::write(&f.answer, b"{\"answer\":\"changed\"}")?;
    assert!(held.fresh().is_err());
    fs::write(&f.source, vec![b'x'; SOURCE_MAX + 1])?;
    assert!(f.prepare().is_err());
    assert!(!f.output.exists());
    Ok(())
}
#[test]
fn malformed_or_nonliteral_original_contract_has_no_request() -> Result<()> {
    for source in [
        SOURCE.to_owned() + "MODEL = 'duplicate'\n",
        SOURCE.replace("EFFORT = 'source-effort'", "EFFORT = str('expression')"),
        SOURCE.replace(
            "'additionalProperties':False",
            "'additionalProperties':True",
        ),
    ] {
        let f = Fixture::new()?;
        fs::write(&f.source, source)?;
        assert!(f.prepare().is_err());
        assert!(!f.output.exists());
    }
    Ok(())
}
#[test]
fn malformed_extra_coerced_mismatched_and_oversized_verdicts_refuse() -> Result<()> {
    let f = Fixture::new()?;
    f.prepare()?;
    let v = f.verdict(false, false)?;
    let original: Value = strict(&fs::read(&v)?)?;
    for (n, value) in [
        json!({}),
        json!({"equivalent":false}),
        {
            let mut value = original.clone();
            value["verdict"]["equivalent"] = json!(1);
            value
        },
        {
            let mut value = original.clone();
            value["verdict"]["citation_bonus"] = json!(1);
            value
        },
        {
            let mut value = original.clone();
            value["answer_sha256"] = json!("0".repeat(64));
            value
        },
        {
            let mut value = original.clone();
            value["verdict"]["reason"] = json!("x".repeat(STDERR_MAX + 1));
            value
        },
    ]
    .into_iter()
    .enumerate()
    {
        fs::write(&v, serde_json::to_vec(&value)?)?;
        let out = f.root.join(format!("invalid-{n}"));
        assert!(f.validate(&v, &out).is_err());
        assert!(!out.exists());
    }
    fs::write(&v, vec![b'x'; ARTIFACT_MAX + 1])?;
    assert!(f.validate(&v, &f.root.join("oversized")).is_err());
    Ok(())
}
#[test]
fn changed_admitted_request_inputs_and_consistent_prompt_rewrite_refuse() -> Result<()> {
    for name in [
        "judge-source.py",
        "reference.json",
        "answer.json",
        "request.json",
    ] {
        let f = Fixture::new()?;
        f.prepare()?;
        let v = f.verdict(false, false)?;
        fs::write(f.output.join(name), b"changed")?;
        let out = f.root.join("validated");
        assert!(f.validate(&v, &out).is_err());
        assert!(!out.exists());
    }
    let f = Fixture::new()?;
    f.prepare()?;
    let v = f.verdict(false, false)?;
    let request_path = f.output.join("request.json");
    let mut value: Value = strict(&fs::read(&request_path)?)?;
    value["formatted"]["prompt"] = json!("changed synthetic prompt");
    value["formatted"]["prompt_sha256"] = json!(digest(b"changed synthetic prompt"));
    let bytes = serde_json::to_vec(&value)?;
    fs::write(&request_path, &bytes)?;
    let path = f.output.join("request-manifest.json");
    let mut m: Manifest = strict(&fs::read(&path)?)?;
    m.request = Binding {
        sha256: digest(&bytes),
        bytes: bytes.len(),
    };
    fs::write(path, serde_json::to_vec(&m)?)?;
    assert!(f.validate(&v, &f.root.join("rewritten")).is_err());
    Ok(())
}
#[test]
fn fresh_output_collision_is_nonmutating_and_outputs_are_private() -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new()?;
    fs::create_dir(&f.output)?;
    fs::write(f.output.join("marker"), b"retain")?;
    assert!(f.prepare().is_err());
    assert_eq!(fs::read(f.output.join("marker"))?, b"retain");
    fs::remove_file(f.output.join("marker"))?;
    fs::remove_dir(&f.output)?;
    f.prepare()?;
    assert_eq!(fs::metadata(&f.output)?.permissions().mode() & 0o777, 0o700);
    for file in fs::read_dir(&f.output)? {
        assert_eq!(file?.metadata()?.permissions().mode() & 0o777, 0o600);
    }
    Ok(())
}
#[test]
fn symlink_leaf_and_parent_and_aliased_inputs_refuse() -> Result<()> {
    use std::os::unix::fs::symlink;
    let f = Fixture::new()?;
    let alias = f.root.join("alias-answer.json");
    symlink(&f.answer, &alias)?;
    assert!(Snapshot::load(&alias, &digest(&fs::read(&f.answer)?), ARTIFACT_MAX).is_err());
    let parent = f.root.join("alias-parent");
    symlink(&f.root, &parent)?;
    assert!(
        Snapshot::load(
            &parent.join("answer.json"),
            &digest(&fs::read(&f.answer)?),
            ARTIFACT_MAX
        )
        .is_err()
    );
    assert!(fresh_output(&f.output.join("nested"), &[&f.output]).is_err());
    Ok(())
}
#[test]
fn exact_schema_and_serialized_output_admission_have_no_defaults() -> Result<()> {
    assert!(strict::<Verdict>(b"{\"equivalent\":false,\"reason\":\"\"}").is_err());
    assert!(
        strict::<Verdict>(b"{\"equivalent\":false,\"abstained\":false,\"reason\":null}").is_err()
    );
    assert!(json_bytes(&"x".repeat(REQUEST_MAX), REQUEST_MAX).is_err());
    assert!(
        schema_valid(
            &json!({"type":"object","properties":{},"required":[],"additionalProperties":false})
        )
        .is_err()
    );
    Ok(())
}

#[test]
fn native_structural_custody_has_no_formatter_or_output_effects_and_rechecks() -> Result<()> {
    let f = Fixture::new()?;
    let sha = f.prepare()?;
    let admitted = PreparedRequest::load(&f.output, &sha, f.python()?)?;
    assert!(admitted.fresh().is_ok());
    assert_eq!(admitted.decoded.actual, None);
    assert!(!admitted.decoded.reward_emitted);
    fs::write(f.output.join("answer.json"), b"changed")?;
    assert!(admitted.fresh().is_err());
    assert!(!f.root.join("native-output").exists());
    Ok(())
}
