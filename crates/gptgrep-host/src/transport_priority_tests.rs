//! Controlled invocation/cleanup states test the actual runner's private helpers.
use super::*;

fn terminal_error() -> anyhow::Error {
    HostProtocolError::new(
        HostProtocolErrorKind::TerminalError,
        &codex_error::ParsedError::parse(&json!({"codexErrorInfo":"unauthorized"})),
        Some(false),
        2,
        Some(&json!({"total":{"inputTokens":7}})),
        None,
    )
    .into()
}

#[tokio::test]
async fn captured_concrete_error_survives_later_overflow_and_drain_failure() {
    let budget = transport::TransportBudget::new(8);
    let invocation = capture_invocation_result::<()>(Ok(Err(terminal_error())), &budget, false);
    tokio::time::sleep(Duration::from_millis(1)).await;
    assert!(budget.admit(9).is_err());
    let error = finalize_captured_invocation(
        invocation,
        &budget,
        Err(anyhow!("host_stderr_cleanup_unknown")),
    )
    .unwrap_err();
    let typed = error.downcast_ref::<HostProtocolError>().unwrap();
    assert_eq!(typed.kind, HostProtocolErrorKind::TerminalError);
    assert_eq!(typed.codex_error_info, Some(CodexErrorInfo::Unauthorized));
    assert_eq!(typed.server_retry_notifications, 2);
    assert_eq!(typed.usage.as_ref().unwrap()["total"]["inputTokens"], 7);
}

#[tokio::test]
async fn concrete_error_at_expired_clock_is_not_reclassified_as_timeout() {
    let budget = transport::TransportBudget::new(8);
    let deadline = tokio::time::Instant::now();
    tokio::time::sleep(Duration::from_millis(1)).await;
    assert!(tokio::time::Instant::now() >= deadline);
    let error =
        capture_invocation_result::<()>(Ok(Err(terminal_error())), &budget, false).unwrap_err();
    assert_eq!(
        error.downcast_ref::<HostProtocolError>().unwrap().kind,
        HostProtocolErrorKind::TerminalError
    );
}

#[tokio::test]
async fn typed_outer_and_inner_timeouts_keep_the_intended_invocation_code() {
    let budget = transport::TransportBudget::new(8);
    let elapsed = tokio::time::timeout(Duration::ZERO, std::future::pending::<()>())
        .await
        .unwrap_err();
    let outer = capture_invocation_result::<()>(Err(elapsed), &budget, true).unwrap_err();
    assert_eq!(outer.to_string(), "host_model_attempt_timeout");
    let elapsed = tokio::time::timeout(Duration::ZERO, std::future::pending::<()>())
        .await
        .unwrap_err();
    let inner = anyhow::Error::new(elapsed).context("host_receive_deadline_unknown");
    let captured = capture_invocation_result::<()>(Ok(Err(inner)), &budget, false);
    assert!(budget.admit(9).is_err());
    let error = finalize_captured_invocation(
        captured,
        &budget,
        Err(anyhow!("host_stderr_cleanup_unknown")),
    )
    .unwrap_err();
    assert!(error.to_string().contains("time limit"));
}

#[test]
fn overflow_already_observed_can_classify_invocation_but_success_checks_later_cleanup() {
    let overflow = transport::TransportBudget::new(8);
    assert!(overflow.admit(9).is_err());
    assert_eq!(
        capture_invocation_result::<()>(Ok(Err(terminal_error())), &overflow, false)
            .unwrap_err()
            .to_string(),
        "host_total_transport_limit"
    );
    let later = transport::TransportBudget::new(8);
    let invocation = capture_invocation_result(Ok(Ok(7)), &later, false);
    assert!(later.admit(9).is_err());
    assert_eq!(
        finalize_captured_invocation(invocation, &later, Ok((0, false)))
            .unwrap_err()
            .to_string(),
        "host_total_transport_limit"
    );
    let drain = transport::TransportBudget::new(8);
    let invocation = capture_invocation_result(Ok(Ok(7)), &drain, false);
    assert_eq!(
        finalize_captured_invocation(
            invocation,
            &drain,
            Err(anyhow!("host_stderr_cleanup_unknown"))
        )
        .unwrap_err()
        .to_string(),
        "host_stderr_cleanup_unknown"
    );
}
