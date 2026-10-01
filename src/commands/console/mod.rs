use std::process::{Output, Stdio};
use std::time::Duration;

use megumi::{Error, Permission, command};

use crate::Context;
use tokio::process::Command;

/// Longest reply body sent to the chat. The tail is kept: the end of a
/// command's output is where failures surface.
pub const MAX_BODY_CHARS: usize = 3000;
const TIMEOUT: Duration = Duration::from_secs(10);

/// Runs a shell command and replies with its output; the owner's console.
#[command(
    name = "console",
    aliases("sh"),
    permission = Permission::Owner,
    hide_in_help
)]
async fn console(ctx: Context, #[rest] command: &str) -> Result<(), Error> {
    if command.trim().is_empty() {
        return ctx.say("Usage: `!console <shell command>`").await;
    }

    ctx.say(run(command).await).await
}

/// Runs `command` under `sh -c` and renders the outcome as a chat reply.
pub async fn run(command: &str) -> String {
    let header = format!("*$ {command}*");

    match tokio::time::timeout(TIMEOUT, shell(command)).await {
        Ok(Ok(output)) => format!("{header}\n```\n{}\n```\n{}", body(&output), status(&output)),
        Ok(Err(error)) => format!("{header}\nconsole could not run: {error}"),
        Err(_) => format!("{header}\nconsole timed out after {}s", TIMEOUT.as_secs()),
    }
}

async fn shell(command: &str) -> std::io::Result<Output> {
    Command::new("sh")
        .arg("-c")
        .arg(command)
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output()
        .await
}

fn body(output: &Output) -> String {
    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    let text = text.trim_end();
    let text = if text.is_empty() { "(no output)" } else { text };
    truncate(text)
}

fn status(output: &Output) -> String {
    output.status.code().map_or_else(
        || "killed by a signal".to_string(),
        |code| format!("exit {code}"),
    )
}

fn truncate(text: &str) -> String {
    if text.chars().count() <= MAX_BODY_CHARS {
        return text.to_string();
    }

    let tail: String = text
        .chars()
        .rev()
        .take(MAX_BODY_CHARS)
        .collect::<Vec<char>>()
        .into_iter()
        .rev()
        .collect();
    format!("[…truncated]\n{tail}")
}
