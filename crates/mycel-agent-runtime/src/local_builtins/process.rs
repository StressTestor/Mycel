use std::{ffi::OsString, path::Path, process::Stdio, sync::Arc, time::Duration};

use mycel_agent_protocol::{ToolUpdate, ToolUpdateKind};
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::Command,
    sync::mpsc,
};

use crate::{CancellationToken, ToolUpdateSink};

const RAW_OUTPUT_CAP: usize = 10 * 1024 * 1024;
const KILL_GRACE: Duration = Duration::from_millis(500);

#[derive(Clone, Copy, Debug)]
enum Stream {
    Stdout,
    Stderr,
}

#[derive(Debug)]
struct Chunk {
    stream: Stream,
    bytes: Vec<u8>,
}

#[derive(Debug)]
pub(crate) struct ProcessOutcome {
    pub(crate) exit_code: Option<i32>,
    pub(crate) stdout: Vec<u8>,
    pub(crate) stderr: Vec<u8>,
    pub(crate) combined: String,
    pub(crate) raw_truncated: bool,
    pub(crate) timed_out: bool,
    pub(crate) cancelled: bool,
}

pub(crate) struct ProcessRequest<'a> {
    pub(crate) program: &'a Path,
    pub(crate) args: &'a [OsString],
    pub(crate) cwd: &'a Path,
    pub(crate) env: &'a [(&'a str, OsString)],
    pub(crate) timeout: Duration,
    pub(crate) cancellation: &'a CancellationToken,
    pub(crate) updates: Arc<dyn ToolUpdateSink>,
    /// Raw process streams are safe for Bash, but search tools must suppress
    /// them until sensitive-path filtering has run.
    pub(crate) stream_updates: bool,
}

pub(crate) async fn run_process(request: ProcessRequest<'_>) -> Result<ProcessOutcome, String> {
    if request.cancellation.is_cancelled() {
        return Ok(empty_outcome(false, true));
    }

    let mut command = Command::new(request.program);
    command
        .args(request.args)
        .current_dir(request.cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    for (name, value) in request.env {
        command.env(name, value);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.as_std_mut().process_group(0);
    }

    let mut child = command
        .spawn()
        .map_err(|error| format!("failed to start {}: {error}", request.program.display()))?;
    let process_id = child.id();
    let stdout = child.stdout.take().ok_or("child stdout was not captured")?;
    let stderr = child.stderr.take().ok_or("child stderr was not captured")?;
    let (sender, mut receiver) = mpsc::channel(32);
    let stdout_task = tokio::spawn(drain(stdout, Stream::Stdout, sender.clone()));
    let stderr_task = tokio::spawn(drain(stderr, Stream::Stderr, sender));

    let mut stdout_bytes = Vec::new();
    let mut stderr_bytes = Vec::new();
    let mut combined = String::new();
    let mut raw_truncated = false;

    enum Stop {
        Exited(std::process::ExitStatus),
        TimedOut,
        Cancelled,
    }

    let stop = {
        let wait = child.wait();
        tokio::pin!(wait);
        let deadline = tokio::time::sleep(request.timeout);
        tokio::pin!(deadline);
        loop {
            tokio::select! {
                status = &mut wait => {
                    let status = status.map_err(|error| format!("failed to wait for child: {error}"))?;
                    break Stop::Exited(status);
                }
                maybe_chunk = receiver.recv() => {
                    if let Some(chunk) = maybe_chunk {
                        record_chunk(
                            chunk,
                            &request.updates,
                            &mut stdout_bytes,
                            &mut stderr_bytes,
                            &mut combined,
                            &mut raw_truncated,
                            request.stream_updates,
                        );
                    }
                }
                () = request.cancellation.cancelled() => break Stop::Cancelled,
                () = &mut deadline => break Stop::TimedOut,
            }
        }
    };

    let (status, timed_out, cancelled) = match stop {
        Stop::Exited(status) => (Some(status), false, false),
        Stop::TimedOut => {
            terminate_process(&mut child, process_id).await;
            (child.wait().await.ok(), true, false)
        }
        Stop::Cancelled => {
            terminate_process(&mut child, process_id).await;
            (child.wait().await.ok(), false, true)
        }
    };

    let _ = stdout_task.await;
    let _ = stderr_task.await;
    while let Some(chunk) = receiver.recv().await {
        record_chunk(
            chunk,
            &request.updates,
            &mut stdout_bytes,
            &mut stderr_bytes,
            &mut combined,
            &mut raw_truncated,
            request.stream_updates,
        );
    }

    request.updates.emit(ToolUpdate {
        kind: ToolUpdateKind::Status,
        text: Some(if cancelled {
            "process cancelled".to_owned()
        } else if timed_out {
            "process timed out".to_owned()
        } else {
            format!(
                "process exited with {}",
                status.as_ref().and_then(|s| s.code()).unwrap_or(-1)
            )
        }),
        percent: Some(100.0),
        custom_kind: None,
        custom_data: None,
    });

    Ok(ProcessOutcome {
        exit_code: status.and_then(|status| status.code()),
        stdout: stdout_bytes,
        stderr: stderr_bytes,
        combined,
        raw_truncated,
        timed_out,
        cancelled,
    })
}

async fn drain<R>(mut reader: R, stream: Stream, sender: mpsc::Sender<Chunk>)
where
    R: AsyncRead + Unpin,
{
    let mut buffer = vec![0_u8; 8 * 1024];
    loop {
        match reader.read(&mut buffer).await {
            Ok(0) | Err(_) => break,
            Ok(read) => {
                if sender
                    .send(Chunk {
                        stream,
                        bytes: buffer[..read].to_vec(),
                    })
                    .await
                    .is_err()
                {
                    break;
                }
            }
        }
    }
}

fn record_chunk(
    chunk: Chunk,
    updates: &Arc<dyn ToolUpdateSink>,
    stdout: &mut Vec<u8>,
    stderr: &mut Vec<u8>,
    combined: &mut String,
    truncated: &mut bool,
    stream_updates: bool,
) {
    let remaining = RAW_OUTPUT_CAP.saturating_sub(stdout.len() + stderr.len());
    let kept = chunk.bytes.len().min(remaining);
    let target = match chunk.stream {
        Stream::Stdout => stdout,
        Stream::Stderr => stderr,
    };
    target.extend_from_slice(&chunk.bytes[..kept]);
    let mut display_truncated = false;
    if kept > 0 && stream_updates {
        let text = String::from_utf8_lossy(&chunk.bytes[..kept]);
        let remaining = RAW_OUTPUT_CAP.saturating_sub(combined.len());
        let boundary = utf8_prefix_boundary(&text, remaining);
        let original_len = text.len();
        let text = &text[..boundary];
        display_truncated = boundary < original_len;
        if !text.is_empty() {
            combined.push_str(text);
            updates.emit(ToolUpdate {
                kind: match chunk.stream {
                    Stream::Stdout => ToolUpdateKind::Stdout,
                    Stream::Stderr => ToolUpdateKind::Stderr,
                },
                text: Some(text.to_owned()),
                percent: None,
                custom_kind: None,
                custom_data: None,
            });
        }
    }
    if (kept < chunk.bytes.len() || display_truncated) && !*truncated {
        *truncated = true;
        updates.emit(ToolUpdate {
            kind: ToolUpdateKind::Status,
            text: Some(
                "process output truncated at 10 MiB; remaining output is being drained".to_owned(),
            ),
            percent: None,
            custom_kind: None,
            custom_data: None,
        });
    }
}

fn utf8_prefix_boundary(text: &str, max_bytes: usize) -> usize {
    if text.len() <= max_bytes {
        return text.len();
    }
    let mut boundary = max_bytes;
    while boundary > 0 && !text.is_char_boundary(boundary) {
        boundary -= 1;
    }
    boundary
}

async fn terminate_process(child: &mut tokio::process::Child, process_id: Option<u32>) {
    if child.try_wait().ok().flatten().is_some() {
        return;
    }
    #[cfg(unix)]
    if let Some(process_id) = process_id {
        let _ = Command::new("/bin/kill")
            .args(["-TERM", "--", &format!("-{process_id}")])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await;
    }
    #[cfg(not(unix))]
    let _ = process_id;
    if tokio::time::timeout(KILL_GRACE, child.wait())
        .await
        .is_err()
    {
        let _ = child.start_kill();
    }
}

fn empty_outcome(timed_out: bool, cancelled: bool) -> ProcessOutcome {
    ProcessOutcome {
        exit_code: None,
        stdout: Vec::new(),
        stderr: Vec::new(),
        combined: String::new(),
        raw_truncated: false,
        timed_out,
        cancelled,
    }
}
