use anyhow::Result;
use husk_agent_proto::{
    AgentRequest, AgentResponse, ErrorResponse, ExecResponse, ReadFileResponse,
    ShellDataResponse, ShellExitResponse, ShellStartRequest, WriteFileResponse, base64_decode,
    base64_encode, read_message, write_message,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tracing::warn;

/// Handle a single connection, processing requests until the stream closes.
///
/// Generic over the stream type so it works with both Unix sockets (dev/test)
/// and vsock streams (production in-VM).
pub async fn handle_connection<S>(mut stream: S) -> Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    loop {
        let request: Option<AgentRequest> = read_message(&mut stream).await?;
        let Some(request) = request else {
            return Ok(());
        };

        match request {
            AgentRequest::ShellStart(req) => {
                // Shell takes over the connection — no more request/response loop
                return handle_shell(&mut stream, req).await;
            }
            other => {
                let response = handle_request(other).await;
                write_message(&mut stream, &response).await?;
            }
        }
    }
}

async fn handle_shell<S>(stream: &mut S, req: ShellStartRequest) -> Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let command = req.command.as_deref().unwrap_or("/bin/sh");

    let mut child = match tokio::process::Command::new(command)
        .envs(req.env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(e) => {
            let resp = AgentResponse::Error(ErrorResponse {
                message: format!("failed to start shell: {e}"),
            });
            write_message(stream, &resp).await?;
            return Ok(());
        }
    };

    write_message(stream, &AgentResponse::ShellStarted).await?;

    let mut child_stdin = child.stdin.take().unwrap();
    let child_stdout = child.stdout.take().unwrap();
    let child_stderr = child.stderr.take().unwrap();

    let mut stdout_buf = vec![0u8; 4096];
    let mut stderr_buf = vec![0u8; 4096];
    let mut stdout_reader = child_stdout;
    let mut stderr_reader = child_stderr;
    let mut stdout_closed = false;
    let mut stderr_closed = false;

    loop {
        tokio::select! {
            result = stdout_reader.read(&mut stdout_buf), if !stdout_closed => {
                match result {
                    Ok(0) => {
                        stdout_closed = true;
                    }
                    Ok(n) => {
                        let data = base64_encode(&stdout_buf[..n]);
                        write_message(stream, &AgentResponse::ShellData(ShellDataResponse { data })).await?;
                    }
                    Err(_) => {
                        stdout_closed = true;
                    }
                }
            }
            result = stderr_reader.read(&mut stderr_buf), if !stderr_closed => {
                match result {
                    Ok(0) => {
                        stderr_closed = true;
                    }
                    Ok(n) => {
                        let data = base64_encode(&stderr_buf[..n]);
                        write_message(stream, &AgentResponse::ShellData(ShellDataResponse { data })).await?;
                    }
                    Err(_) => {
                        stderr_closed = true;
                    }
                }
            }
            msg = read_message::<AgentRequest, _>(stream) => {
                match msg {
                    Ok(Some(AgentRequest::ShellData(req))) => {
                        if let Ok(data) = base64_decode(&req.data) {
                            let _ = child_stdin.write_all(&data).await;
                            let _ = child_stdin.flush().await;
                        }
                    }
                    Ok(Some(AgentRequest::ShellResize(_))) => {
                        // No-op for piped mode; requires PTY for real resize
                    }
                    Ok(None) | Err(_) => {
                        // Host disconnected — kill the shell
                        let _ = child.kill().await;
                        return Ok(());
                    }
                    Ok(Some(_)) => {
                        // Unexpected message type during shell — ignore
                    }
                }
            }
            status = child.wait() => {
                let exit_code = status.map(|s| s.code().unwrap_or(-1)).unwrap_or(-1);
                // Drain any remaining stdout
                let mut remaining = Vec::new();
                let _ = AsyncReadExt::read_to_end(&mut stdout_reader, &mut remaining).await;
                if !remaining.is_empty() {
                    let data = base64_encode(&remaining);
                    write_message(stream, &AgentResponse::ShellData(ShellDataResponse { data })).await?;
                }
                // Drain any remaining stderr
                let mut remaining = Vec::new();
                let _ = AsyncReadExt::read_to_end(&mut stderr_reader, &mut remaining).await;
                if !remaining.is_empty() {
                    let data = base64_encode(&remaining);
                    write_message(stream, &AgentResponse::ShellData(ShellDataResponse { data })).await?;
                }
                write_message(stream, &AgentResponse::ShellExit(ShellExitResponse { exit_code })).await?;
                return Ok(());
            }
        }
    }
}

async fn handle_request(request: AgentRequest) -> AgentResponse {
    match request {
        AgentRequest::Ping => AgentResponse::Pong,

        AgentRequest::Exec(req) => {
            let result = tokio::process::Command::new(&req.command)
                .args(&req.args)
                .envs(req.env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
                .current_dir(req.working_dir.as_deref().unwrap_or("/"))
                .output()
                .await;

            match result {
                Ok(output) => AgentResponse::Exec(ExecResponse {
                    exit_code: output.status.code().unwrap_or(-1),
                    stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                    stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
                }),
                Err(e) => AgentResponse::Error(ErrorResponse {
                    message: format!("exec failed: {e}"),
                }),
            }
        }

        AgentRequest::ReadFile(req) => match tokio::fs::read(&req.path).await {
            Ok(data) => {
                let size = data.len() as u64;
                let encoded = base64_encode(&data);
                AgentResponse::ReadFile(ReadFileResponse {
                    data: encoded,
                    size,
                })
            }
            Err(e) => AgentResponse::Error(ErrorResponse {
                message: format!("read failed: {e}"),
            }),
        },

        AgentRequest::WriteFile(req) => {
            let data = match base64_decode(&req.data) {
                Ok(d) => d,
                Err(e) => {
                    return AgentResponse::Error(ErrorResponse {
                        message: format!("base64 decode failed: {e}"),
                    });
                }
            };
            let len = data.len() as u64;
            match tokio::fs::write(&req.path, &data).await {
                Ok(()) => {
                    #[cfg(unix)]
                    if let Some(mode) = req.mode {
                        use std::os::unix::fs::PermissionsExt;
                        if let Err(e) = tokio::fs::set_permissions(
                            &req.path,
                            std::fs::Permissions::from_mode(mode),
                        )
                        .await
                        {
                            warn!("failed to set permissions on {}: {e}", req.path);
                        }
                    }
                    AgentResponse::WriteFile(WriteFileResponse { bytes_written: len })
                }
                Err(e) => AgentResponse::Error(ErrorResponse {
                    message: format!("write failed: {e}"),
                }),
            }
        }

        // ShellStart is handled in handle_connection before reaching here.
        // ShellData and ShellResize are only valid during an active shell session.
        AgentRequest::ShellStart(_) | AgentRequest::ShellData(_) | AgentRequest::ShellResize(_) => {
            AgentResponse::Error(ErrorResponse {
                message: "shell messages are not valid outside a shell session".into(),
            })
        }
    }
}
