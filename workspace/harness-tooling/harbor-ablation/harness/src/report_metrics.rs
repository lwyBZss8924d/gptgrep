use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

type Result<T> = std::result::Result<T, String>;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    schema_version: String,
    cohort_sha256: String,
    configuration_sha256: String,
    judge_rubric_sha256: String,
    campaign_final: bool,
    tasks: Vec<Task>,
    repeats: Vec<u32>,
    outcomes: Vec<Outcome>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Task {
    task_id: String,
    document_group: String,
}

#[derive(Clone, Copy, Deserialize, Eq, Ord, PartialEq, PartialOrd)]
#[serde(rename_all = "snake_case")]
enum Arm {
    Control,
    Gptgrep,
}

impl Arm {
    fn name(self) -> &'static str {
        match self {
            Self::Control => "control",
            Self::Gptgrep => "gptgrep",
        }
    }
}

#[derive(Clone, Copy, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
enum Operational {
    Unexecuted,
    Pending,
    Unknown,
    Completed,
    Failed,
}

impl Operational {
    fn name(self) -> &'static str {
        match self {
            Self::Unexecuted => "unexecuted",
            Self::Pending => "pending",
            Self::Unknown => "unknown",
            Self::Completed => "completed",
            Self::Failed => "failed",
        }
    }
}

#[derive(Clone, Copy, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
enum Judge {
    NotAttempted,
    Pending,
    Completed,
    Unavailable,
}

#[derive(Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
enum ClockScope {
    Unavailable,
    TaskDeliveryToQueryTerminal,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Outcome {
    cohort_sha256: String,
    configuration_sha256: String,
    judge_receipt_sha256: Option<String>,
    judged_answer_sha256: Option<String>,
    judge_rubric_sha256: Option<String>,
    task_id: String,
    repeat: u32,
    arm: Arm,
    operational_status: Operational,
    terminal_sha256: Option<String>,
    answer_sha256: Option<String>,
    failure_reason: Option<String>,
    judge_status: Judge,
    equivalent: Option<bool>,
    query_wall_ms: Option<u64>,
    query_wall_scope: ClockScope,
    native_latency_ms: Option<u64>,
}

fn hash(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}

fn identity(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"-_.:".contains(&c))
}

fn terminal(row: &Outcome) -> bool {
    matches!(
        row.operational_status,
        Operational::Completed | Operational::Failed
    )
}

fn outcome_valid(row: &Outcome, rubric_sha256: &str) -> bool {
    if row.judge_status == Judge::Completed {
        if !row.judge_receipt_sha256.as_deref().is_some_and(hash)
            || !row.judged_answer_sha256.as_deref().is_some_and(hash)
            || row.judged_answer_sha256 != row.answer_sha256
            || row.judge_rubric_sha256.as_deref() != Some(rubric_sha256)
        {
            return false;
        }
    } else if row.judge_receipt_sha256.is_some()
        || row.judged_answer_sha256.is_some()
        || row.judge_rubric_sha256.is_some()
    {
        return false;
    }
    let has_terminal = row.terminal_sha256.as_deref().is_some_and(hash);
    let has_answer = row.answer_sha256.as_deref().is_some_and(hash);
    if row.terminal_sha256.as_deref().is_some_and(|v| !hash(v))
        || row.answer_sha256.as_deref().is_some_and(|v| !hash(v))
        || row.failure_reason.as_deref().is_some_and(|v| !identity(v))
        || row.query_wall_ms.is_some()
            != (row.query_wall_scope == ClockScope::TaskDeliveryToQueryTerminal)
        || (row.query_wall_ms.is_some() && !terminal(row))
    {
        return false;
    }
    match row.operational_status {
        Operational::Completed => {
            has_terminal
                && has_answer
                && row.failure_reason.is_none()
                && ((row.judge_status == Judge::Completed && row.equivalent.is_some())
                    || (row.judge_status != Judge::Completed && row.equivalent.is_none()))
        }
        Operational::Failed => {
            has_terminal
                && row.failure_reason.is_some()
                && row.answer_sha256.is_none()
                && row.judge_status == Judge::NotAttempted
                && row.equivalent.is_none()
        }
        Operational::Pending | Operational::Unexecuted | Operational::Unknown => {
            row.terminal_sha256.is_none()
                && row.answer_sha256.is_none()
                && row.judge_status == Judge::NotAttempted
                && row.equivalent.is_none()
        }
    }
}

fn label(row: Option<&Outcome>) -> Option<bool> {
    match row {
        Some(r) if r.operational_status == Operational::Failed => Some(false),
        Some(r) if r.operational_status == Operational::Completed => r.equivalent,
        _ => None,
    }
}

fn bounds(value: Option<bool>) -> (i64, i64) {
    match value {
        Some(true) => (1, 1),
        Some(false) => (0, 0),
        None => (0, 1),
    }
}

fn clock_summary(values: &[u64], expected: usize) -> Result<Value> {
    let total = values
        .iter()
        .try_fold(0u64, |sum, value| sum.checked_add(*value))
        .ok_or("query_clock_sum_overflow")?;
    Ok(json!({
        "scope": "task_delivery_to_query_terminal",
        "samples": values.len(), "missing": expected - values.len(),
        "known_total_ms": total,
        "complete": values.len() == expected,
        "min_ms": values.iter().min(), "max_ms": values.iter().max(),
        "mean_ms": if values.is_empty() { None } else { Some(total as f64 / values.len() as f64) },
        "component_times_added": false
    }))
}

pub(crate) fn report(bytes: &[u8]) -> Result<Value> {
    let input: Input = serde_json::from_slice(bytes).map_err(|_| "paired_input_schema")?;
    if input.schema_version != "gptgrep.harbor.paired-input.v1"
        || !hash(&input.cohort_sha256)
        || !hash(&input.configuration_sha256)
        || !hash(&input.judge_rubric_sha256)
        || input.tasks.is_empty()
        || input.tasks.len() > 4096
        || input.repeats.is_empty()
        || input.repeats.len() > 32
    {
        return Err("paired_input_contract".into());
    }
    let task_ids: BTreeSet<_> = input.tasks.iter().map(|t| t.task_id.as_str()).collect();
    let repeats: BTreeSet<_> = input.repeats.iter().copied().collect();
    if task_ids.len() != input.tasks.len()
        || repeats.len() != input.repeats.len()
        || input
            .tasks
            .iter()
            .any(|t| !identity(&t.task_id) || !identity(&t.document_group))
        || input.outcomes.len() > input.tasks.len() * input.repeats.len() * 2
    {
        return Err("paired_roster_identity".into());
    }
    let mut rows = BTreeMap::new();
    for row in &input.outcomes {
        if !task_ids.contains(row.task_id.as_str())
            || !repeats.contains(&row.repeat)
            || row.cohort_sha256 != input.cohort_sha256
            || row.configuration_sha256 != input.configuration_sha256
            || !outcome_valid(row, &input.judge_rubric_sha256)
        {
            return Err("paired_outcome_contract".into());
        }
        if rows
            .insert((row.task_id.as_str(), row.repeat, row.arm), row)
            .is_some()
        {
            return Err("paired_duplicate_outcome".into());
        }
    }
    let n = input.tasks.len();
    let mut per_repeat = Vec::new();
    for repeat in &input.repeats {
        let mut arms = serde_json::Map::new();
        let mut execution_complete = true;
        for arm in [Arm::Control, Arm::Gptgrep] {
            let mut correct = 0usize;
            let mut incorrect = 0usize;
            let mut failures = 0usize;
            let mut judge_unknown = 0usize;
            let mut execution_unknown = 0usize;
            let mut observed = 0usize;
            let mut clocks = Vec::new();
            let mut component_samples = 0usize;
            let mut stages: BTreeMap<&str, usize> = BTreeMap::new();
            let mut states: BTreeMap<&str, usize> = BTreeMap::new();
            for task in &input.tasks {
                let row = rows.get(&(task.task_id.as_str(), *repeat, arm)).copied();
                *states
                    .entry(row.map_or("missing", |r| r.operational_status.name()))
                    .or_default() += 1;
                if let Some(r) = row {
                    observed += 1;
                    if let Some(ms) = r.query_wall_ms {
                        clocks.push(ms);
                    }
                    component_samples += usize::from(r.native_latency_ms.is_some());
                    if r.operational_status == Operational::Failed {
                        failures += 1;
                        *stages
                            .entry(r.failure_reason.as_deref().unwrap_or("unknown"))
                            .or_default() += 1;
                    }
                }
                match row {
                    Some(r) if terminal(r) => match label(Some(r)) {
                        Some(true) => correct += 1,
                        Some(false) => incorrect += 1,
                        None => judge_unknown += 1,
                    },
                    _ => {
                        execution_unknown += 1;
                        execution_complete = false;
                    }
                }
            }
            arms.insert(arm.name().into(), json!({
                "expected": n, "observed_rows": observed, "missing_rows": n-observed,
                "correct": correct, "incorrect_including_operational_failures": incorrect,
                "operational_failures": failures, "failure_stages": stages,
                "judge_unknown": judge_unknown, "execution_pending_or_unknown": execution_unknown,
                "execution_states": states,
                "accuracy_lower": correct as f64 / n as f64,
                "accuracy_upper": (correct+judge_unknown+execution_unknown) as f64 / n as f64,
                "accuracy": if input.campaign_final && judge_unknown == 0 && execution_unknown == 0 {
                    Some(correct as f64 / n as f64)
                } else { None },
                "query_wall_ms": clock_summary(&clocks, n)?,
                "native_component_samples": component_samples
            }));
        }
        if input.campaign_final && !execution_complete {
            return Err("final_requires_terminal_outcomes".into());
        }
        let mut categories = [0usize; 5];
        let mut difference = (0i64, 0i64);
        let mut pairs = Vec::new();
        for task in &input.tasks {
            let br = rows
                .get(&(task.task_id.as_str(), *repeat, Arm::Control))
                .copied();
            let tr = rows
                .get(&(task.task_id.as_str(), *repeat, Arm::Gptgrep))
                .copied();
            let b = label(br);
            let t = label(tr);
            let category = match (b, t) {
                (Some(true), Some(true)) => 0,
                (Some(false), Some(true)) => 1,
                (Some(true), Some(false)) => 2,
                (Some(false), Some(false)) => 3,
                _ => 4,
            };
            categories[category] += 1;
            let (bl, bu) = bounds(b);
            let (tl, tu) = bounds(t);
            difference.0 += tl - bu;
            difference.1 += tu - bl;
            pairs.push(json!({"task_id":task.task_id,"document_group":task.document_group,
                "control":b,"gptgrep":t,
                "control_operational_status":br.map_or("missing", |r| r.operational_status.name()),
                "gptgrep_operational_status":tr.map_or("missing", |r| r.operational_status.name())}));
        }
        let conclusion = if !input.campaign_final {
            "pending_campaign"
        } else if difference.0 > 0 {
            "quality_gain_robust_to_unknowns"
        } else if difference.1 <= 0 {
            "quality_gain_not_demonstrated"
        } else {
            "undetermined_judge_unknowns"
        };
        per_repeat.push(json!({
            "repeat":repeat, "question_denominator":n, "expected_arm_rows":n*2,
            "execution_complete":execution_complete,"arms":arms,
            "paired":{"both_correct":categories[0],"treatment_only_correct":categories[1],
                      "baseline_only_correct":categories[2],"both_incorrect":categories[3],
                      "unresolved_pairs":categories[4]},
            "treatment_minus_control":{"lower":difference.0 as f64/n as f64,
                                       "upper":difference.1 as f64/n as f64},
            "quality_conclusion":conclusion,"pairs":pairs
        }));
    }
    Ok(json!({
        "schema_version":"gptgrep.harbor.paired-report.v1",
        "cohort_sha256":input.cohort_sha256,"configuration_sha256":input.configuration_sha256,
        "judge_rubric_sha256":input.judge_rubric_sha256,
        "campaign_final":input.campaign_final,"task_count":n,
        "document_groups":input.tasks.iter().map(|t| &t.document_group).collect::<BTreeSet<_>>().len(),
        "repeats":per_repeat,"scope":"metadata_projection_only_no_runtime_or_judgment",
        "terminal_evidence":"hash references supplied by the accepted collector; payloads not opened",
        "efficiency_endpoint":null,"release_authorized":false
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(n: usize) -> Value {
        json!({"schema_version":"gptgrep.harbor.paired-input.v1", "cohort_sha256":"a".repeat(64),
            "configuration_sha256":"b".repeat(64),"judge_rubric_sha256":"e".repeat(64),"campaign_final":true,
            "tasks":(0..n).map(|i| json!({"task_id":format!("synthetic-{i}"),"document_group":"synthetic-group"})).collect::<Vec<_>>(),
            "repeats":[0],"outcomes":[]})
    }
    fn row(i: usize, arm: &str, label: Option<bool>) -> Value {
        json!({"task_id":format!("synthetic-{i}"),"repeat":0,"arm":arm,
            "cohort_sha256":"a".repeat(64),"configuration_sha256":"b".repeat(64),
            "judge_receipt_sha256":label.map(|_| "f".repeat(64)),
            "judged_answer_sha256":label.map(|_| "d".repeat(64)),
            "judge_rubric_sha256":label.map(|_| "e".repeat(64)),
            "operational_status":"completed","terminal_sha256":"c".repeat(64),
            "answer_sha256":"d".repeat(64),"failure_reason":null,
            "judge_status":if label.is_some(){"completed"}else{"unavailable"},"equivalent":label,
            "query_wall_ms":null,"query_wall_scope":"unavailable","native_latency_ms":17})
    }
    fn run(v: &Value) -> Result<Value> {
        report(&serde_json::to_vec(v).unwrap())
    }
    fn push(v: &mut Value, row: Value) {
        v["outcomes"].as_array_mut().unwrap().push(row);
    }

    #[test]
    fn terminal_failure_zero_differs_from_judge_unknown() {
        let mut v = input(1);
        let mut failed = row(0, "control", None);
        failed["operational_status"] = json!("failed");
        failed["failure_reason"] = json!("startup_failure");
        failed["answer_sha256"] = Value::Null;
        failed["judge_status"] = json!("not_attempted");
        push(&mut v, failed);
        push(&mut v, row(0, "gptgrep", None));
        let r = run(&v).unwrap();
        let a = &r["repeats"][0]["arms"];
        assert_eq!(a["control"]["accuracy"], 0.0);
        assert_eq!(a["control"]["operational_failures"], 1);
        assert_eq!(a["gptgrep"]["judge_unknown"], 1);
        assert_eq!(a["gptgrep"]["accuracy_upper"], 1.0);
        assert!(a["gptgrep"]["accuracy"].is_null());
    }

    #[test]
    fn missing_execution_never_becomes_final_failure_zero() {
        let mut v = input(2);
        push(&mut v, row(0, "control", Some(false)));
        push(&mut v, row(0, "gptgrep", Some(true)));
        assert_eq!(run(&v).unwrap_err(), "final_requires_terminal_outcomes");
        v["campaign_final"] = json!(false);
        let r = run(&v).unwrap();
        assert_eq!(r["repeats"][0]["question_denominator"], 2);
        assert_eq!(
            r["repeats"][0]["arms"]["control"]["execution_pending_or_unknown"],
            1
        );
        assert_eq!(r["repeats"][0]["quality_conclusion"], "pending_campaign");
        let mut pending = row(1, "control", None);
        pending["operational_status"] = json!("unexecuted");
        pending["terminal_sha256"] = Value::Null;
        pending["answer_sha256"] = Value::Null;
        pending["judge_status"] = json!("not_attempted");
        push(&mut v, pending);
        v["campaign_final"] = json!(true);
        assert_eq!(run(&v).unwrap_err(), "final_requires_terminal_outcomes");
    }

    #[test]
    fn rejects_duplicate_and_unmatched_identity() {
        let mut v = input(2);
        v["campaign_final"] = json!(false);
        push(&mut v, row(0, "control", Some(true)));
        push(&mut v, row(0, "control", Some(false)));
        assert_eq!(run(&v).unwrap_err(), "paired_duplicate_outcome");
        v["outcomes"][1]["task_id"] = json!("outside-roster");
        assert_eq!(run(&v).unwrap_err(), "paired_outcome_contract");
    }

    #[test]
    fn four_categories_and_unknown_preserve_runtime_denominator() {
        let mut v = input(5);
        for (i, (b, t)) in [
            (Some(true), Some(true)),
            (Some(false), Some(true)),
            (Some(true), Some(false)),
            (Some(false), Some(false)),
            (Some(false), None),
        ]
        .into_iter()
        .enumerate()
        {
            push(&mut v, row(i, "control", b));
            push(&mut v, row(i, "gptgrep", t));
        }
        let r = run(&v).unwrap();
        let repeat = &r["repeats"][0];
        for key in [
            "both_correct",
            "treatment_only_correct",
            "baseline_only_correct",
            "both_incorrect",
            "unresolved_pairs",
        ] {
            assert_eq!(repeat["paired"][key], 1);
        }
        assert_eq!(repeat["question_denominator"], 5);
        assert_eq!(repeat["treatment_minus_control"]["lower"], 0.0);
        assert_eq!(repeat["treatment_minus_control"]["upper"], 0.2);
        assert_eq!(repeat["quality_conclusion"], "undetermined_judge_unknowns");
    }

    #[test]
    fn component_duration_is_not_query_wall() {
        let mut v = input(1);
        push(&mut v, row(0, "control", Some(true)));
        push(&mut v, row(0, "gptgrep", Some(true)));
        let r = run(&v).unwrap();
        assert_eq!(
            r["repeats"][0]["arms"]["control"]["query_wall_ms"]["missing"],
            1
        );
        v["outcomes"][0]["query_wall_ms"] = json!(25);
        assert_eq!(run(&v).unwrap_err(), "paired_outcome_contract");
        v["outcomes"][0]["query_wall_scope"] = json!("task_delivery_to_query_terminal");
        let r = run(&v).unwrap();
        assert_eq!(
            r["repeats"][0]["arms"]["control"]["query_wall_ms"]["known_total_ms"],
            25
        );
        assert_eq!(
            r["repeats"][0]["arms"]["control"]["query_wall_ms"]["component_times_added"],
            false
        );
    }
    #[test]
    fn rejects_stale_verdict_answer_rubric_and_configuration() {
        let mut original = input(1);
        push(&mut original, row(0, "control", Some(false)));
        push(&mut original, row(0, "gptgrep", Some(true)));
        assert!(run(&original).is_ok());
        for field in [
            "judged_answer_sha256",
            "judge_rubric_sha256",
            "configuration_sha256",
            "cohort_sha256",
        ] {
            let mut altered = original.clone();
            altered["outcomes"][1][field] = json!("9".repeat(64));
            assert_eq!(run(&altered).unwrap_err(), "paired_outcome_contract");
        }
        let mut absent_receipt = original;
        absent_receipt["outcomes"][1]["judge_receipt_sha256"] = Value::Null;
        assert_eq!(run(&absent_receipt).unwrap_err(), "paired_outcome_contract");
    }
}
