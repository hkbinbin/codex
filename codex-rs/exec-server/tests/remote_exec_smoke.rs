//! Cross-platform end-to-end smoke test for the remote execution channel.
//!
//! Unlike `exec_process.rs` (most cases are gated Unix-only because they use
//! POSIX commands), this test runs a real command on the *host* platform
//! through a live exec-server over the WebSocket JSON-RPC channel. It exercises
//! the same `ExecBackend::start` + `ExecProcess::read` path that the classic
//! `shell_command` remote helper (`execute_remote_exec_request`) and
//! `unified_exec` rely on, so a green run here means "start server + client,
//! execute a task" works end to end on this machine.

mod common;

use anyhow::Result;
use anyhow::anyhow;
use codex_exec_server::Environment;
use codex_exec_server::ExecOutputStream;
use codex_exec_server::ExecParams;
use codex_exec_server::ProcessId;
use codex_exec_server::ReadResponse;
use codex_exec_server::StartedExecProcess;
use codex_utils_path_uri::PathUri;
use common::exec_server::exec_server;
use pretty_assertions::assert_eq;
use tokio::time::Duration;
use tokio::time::timeout;

/// Builds an argv that prints `text` to stdout and exits 0, using the host
/// platform's default shell.
fn echo_command(text: &str) -> Vec<String> {
    if cfg!(windows) {
        vec!["cmd".to_string(), "/C".to_string(), format!("echo {text}")]
    } else {
        vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            format!("printf '%s\\n' '{text}'"),
        ]
    }
}

/// Builds an argv that writes to stderr and exits with a non-zero status.
fn failing_command() -> Vec<String> {
    if cfg!(windows) {
        vec![
            "cmd".to_string(),
            "/C".to_string(),
            "echo boom 1>&2 & exit 3".to_string(),
        ]
    } else {
        vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            "printf 'boom\\n' 1>&2; exit 3".to_string(),
        ]
    }
}

/// Drains a started remote process to completion, mirroring the production
/// `drain_remote_process` loop: read until `closed`, split stdout/stderr by
/// stream, and capture the exit code on `exited`.
async fn drain(started: StartedExecProcess) -> Result<(String, String, Option<i32>)> {
    let process = started.process;
    let mut stdout = String::new();
    let mut stderr = String::new();
    let mut exit_code = None;
    let mut after_seq: Option<u64> = None;

    loop {
        let ReadResponse {
            chunks,
            next_seq,
            exited,
            exit_code: code,
            closed,
            failure,
            sandbox_denied: _,
        } = timeout(
            Duration::from_secs(10),
            process.read(
                after_seq,
                /*max_bytes*/ None,
                /*wait_ms*/ Some(500),
            ),
        )
        .await
        .map_err(|_| anyhow!("timed out reading remote process output"))??;

        for chunk in chunks {
            let bytes = chunk.chunk.into_inner();
            let text = String::from_utf8_lossy(&bytes);
            match chunk.stream {
                ExecOutputStream::Stderr => stderr.push_str(&text),
                ExecOutputStream::Stdout | ExecOutputStream::Pty => stdout.push_str(&text),
            }
        }
        if exited {
            exit_code = code;
        }
        if let Some(message) = failure {
            return Err(anyhow!("remote process failed: {message}"));
        }
        after_seq = next_seq.checked_sub(1);
        if closed {
            break;
        }
    }

    Ok((stdout, stderr, exit_code))
}

async fn start(
    environment: &Environment,
    process_id: &str,
    argv: Vec<String>,
) -> Result<StartedExecProcess> {
    environment
        .get_exec_backend()
        .start(ExecParams {
            process_id: ProcessId::from(process_id),
            argv,
            cwd: PathUri::from_host_native_path(std::env::current_dir()?)?,
            env_policy: None,
            env: Default::default(),
            tty: false,
            pipe_stdin: false,
            arg0: None,
            sandbox: None,
            enforce_managed_network: false,
            managed_network: None,
        })
        .await
        .map_err(|err| anyhow!("failed to start remote process: {err}"))
}

/// A successful command run through a live exec-server returns its stdout and a
/// zero exit code.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_command_runs_and_returns_stdout() -> Result<()> {
    let server = exec_server().await?;
    let environment = Environment::create_for_tests(Some(server.websocket_url().to_string()))?;
    assert!(environment.is_remote(), "environment should be remote");

    let started = start(&environment, "echo-1", echo_command("hello-remote")).await?;
    let (stdout, _stderr, exit_code) = drain(started).await?;

    assert!(
        stdout.contains("hello-remote"),
        "stdout should contain the echoed text, got: {stdout:?}"
    );
    assert_eq!(exit_code, Some(0));
    Ok(())
}

/// A failing command surfaces its stderr and the non-zero exit code over the
/// remote channel.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_command_reports_nonzero_exit_and_stderr() -> Result<()> {
    let server = exec_server().await?;
    let environment = Environment::create_for_tests(Some(server.websocket_url().to_string()))?;

    let started = start(&environment, "fail-1", failing_command()).await?;
    let (_stdout, stderr, exit_code) = drain(started).await?;

    assert!(
        stderr.contains("boom"),
        "stderr should contain the failure text, got: {stderr:?}"
    );
    assert_eq!(exit_code, Some(3));
    Ok(())
}

/// Two sequential commands over the same environment both run on the server,
/// confirming the channel stays usable across calls.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_channel_handles_sequential_commands() -> Result<()> {
    let server = exec_server().await?;
    let environment = Environment::create_for_tests(Some(server.websocket_url().to_string()))?;

    let first = start(&environment, "seq-1", echo_command("first")).await?;
    let (out1, _, code1) = drain(first).await?;
    assert!(out1.contains("first"), "first output: {out1:?}");
    assert_eq!(code1, Some(0));

    let second = start(&environment, "seq-2", echo_command("second")).await?;
    let (out2, _, code2) = drain(second).await?;
    assert!(out2.contains("second"), "second output: {out2:?}");
    assert_eq!(code2, Some(0));

    Ok(())
}
