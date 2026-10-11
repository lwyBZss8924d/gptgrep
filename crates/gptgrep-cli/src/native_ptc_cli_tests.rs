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
