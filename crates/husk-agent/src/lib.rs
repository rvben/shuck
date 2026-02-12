use anyhow::Result;
use husk_agent_proto::{
    AgentRequest, AgentResponse, ErrorResponse, ExecResponse, ReadFileResponse, WriteFileResponse,
    base64_decode, base64_encode, read_message, write_message,
};
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

        let response = handle_request(request).await;
        write_message(&mut stream, &response).await?;
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
    }
}
