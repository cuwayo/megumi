use megumi_whatsapp::commands::console::{MAX_BODY_CHARS, run};

#[tokio::test]
async fn replies_with_the_command_output_and_exit_status() {
    let reply = run("echo hello").await;
    assert!(reply.contains("*$ echo hello*"), "{reply}");
    assert!(reply.contains("hello"), "{reply}");
    assert!(reply.contains("exit 0"), "{reply}");
}

#[tokio::test]
async fn replies_with_stderr_and_the_failing_exit_status() {
    let reply = run("echo boom >&2; exit 3").await;
    assert!(reply.contains("boom"), "{reply}");
    assert!(reply.contains("exit 3"), "{reply}");
}

#[tokio::test]
async fn keeps_a_long_output_inside_the_reply_budget() {
    let reply = run("seq 1 2000").await;
    assert!(reply.contains("[…truncated]"), "{reply}");
    assert!(reply.contains("2000"), "{reply}");
    assert!(reply.chars().count() < MAX_BODY_CHARS + 128, "{reply}");
}
