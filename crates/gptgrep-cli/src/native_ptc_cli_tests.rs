use super::*;

#[test]
fn native_ptc_is_opt_in_and_requires_existing_named_permissions() {
    let cli = Cli::try_parse_from(["gptgrep", "ask", "neutral"]).unwrap();
    assert!(matches!(
        cli.command,
        Some(Command::Ask {
            native_ptc: false,
            ..
        })
    ));
    assert!(Cli::try_parse_from(["gptgrep", "ask", "neutral", "--native-ptc"]).is_err());
    let cli = Cli::try_parse_from([
        "gptgrep",
        "ask",
        "neutral",
        "--native-ptc",
        "--codex-permissions",
        "reader",
    ])
    .unwrap();
    match cli.command.unwrap() {
        Command::Ask {
            native_ptc, host, ..
        } => {
            assert!(native_ptc);
            let base = host.config();
            assert!(
                !base.native_ptc,
                "shared HostArgs must not enable completion or planners"
            );
            assert_eq!(base.permission_profile.as_deref(), Some("reader"));
        }
        _ => panic!("expected ask"),
    }
}

#[test]
fn native_ptc_is_not_a_shared_completion_parse_or_index_option() {
    for args in [
        vec!["gptgrep", "host-complete", "--native-ptc"],
        vec!["gptgrep", "summarize", "--native-ptc"],
        vec!["gptgrep", "index", "--native-ptc"],
        vec!["gptgrep", "parse-agent", "--native-ptc"],
        vec!["gptgrep", "search", "neutral", "--native-ptc"],
    ] {
        assert!(Cli::try_parse_from(args).is_err());
    }
}

#[test]
fn native_ptc_schema_describes_the_budget_soft_landing_and_native_hard_limit() {
    let option = &contract()["commands"]["ask"]["options"]["native-ptc"];
    let budget = option["budget"].as_str().unwrap();
    for term in [
        "max_tool_calls",
        "tool_budget_exhausted",
        "same turn",
        "host_native_ptc_tool_budget_exhausted",
        "native item",
        "not a pre-execution guard",
    ] {
        assert!(budget.contains(term), "{term}");
    }
    // The soft landing belongs to the text retrieval reader and valid requests.
    for term in [
        "the text retrieval reader answers each further valid GPTgrep request",
        "a malformed request stays fatal",
    ] {
        assert!(budget.contains(term), "{term}");
    }
    let accounting = option["accounting"].as_str().unwrap();
    for field in [
        "model_attempts[].native_ptc",
        "tool_budget",
        "admitted_tool_calls",
        "denied_tool_calls",
        "max_denied_tool_calls",
        "model_usage",
    ] {
        assert!(accounting.contains(field), "{field}");
    }
    // admitted_tool_calls is the restricted reader's shared count; the report
    // keeps the GPTgrep/native split in separate fields.
    for term in [
        "admitted_tool_calls (observed native items plus charged GPTgrep calls, so max_tool_calls at every denial)",
        "dynamic_requests_charged and native_tool_calls keep the split",
    ] {
        assert!(accounting.contains(term), "{term}");
    }
    assert!(!accounting.contains("(GPTgrep calls)"));
}

#[test]
fn native_ptc_schema_states_the_multimodal_hard_limit_for_the_accepted_combination() {
    // The combination is accepted, but the rich multimodal reader keeps the
    // hard limit; --schema must not promise it the text reader's denial.
    let cli = Cli::try_parse_from([
        "gptgrep",
        "ask",
        "neutral",
        "--multimodal",
        "--native-ptc",
        "--codex-permissions",
        "reader",
    ])
    .unwrap();
    assert!(matches!(
        cli.command,
        Some(Command::Ask {
            native_ptc: true,
            visual: MultimodalArgs {
                multimodal: true,
                ..
            },
            ..
        })
    ));
    let contract = contract();
    let budget = contract["commands"]["ask"]["options"]["native-ptc"]["budget"]
        .as_str()
        .unwrap();
    assert!(budget.contains(
        "ask --multimodal --native-ptc keeps failing with host_native_ptc_tool_budget_exhausted on the first GPTgrep request past the budget"
    ));
}
