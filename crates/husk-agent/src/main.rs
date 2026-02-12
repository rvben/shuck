use anyhow::{Context, Result};
use husk_agent_proto::{
    AgentRequest, AgentResponse, ErrorResponse, ExecResponse, ReadFileResponse, WriteFileResponse,
    decode_message, encode_message,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tracing::{error, info, warn};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt().with_env_filter("info").init();

    info!("husk-agent starting");

    let listener = if let Ok(path) = std::env::var("HUSK_AGENT_SOCKET") {
        info!("listening on Unix socket: {path}");
        tokio::net::UnixListener::bind(&path).context("binding Unix socket")?
    } else {
        let default_path = "/tmp/husk-agent.sock";
        info!("listening on default socket: {default_path}");
        let _ = std::fs::remove_file(default_path);
        tokio::net::UnixListener::bind(default_path).context("binding default socket")?
    };

    loop {
        let (stream, _) = listener.accept().await?;
        tokio::spawn(async move {
            if let Err(e) = handle_connection(stream).await {
                error!("connection error: {e}");
            }
        });
    }
}

async fn handle_connection(mut stream: tokio::net::UnixStream) -> Result<()> {
    let mut buf = vec![0u8; 64 * 1024];
    let mut read_buf = Vec::new();

    loop {
        let n = stream.read(&mut buf).await?;
        if n == 0 {
            return Ok(());
        }
        read_buf.extend_from_slice(&buf[..n]);

        while let Some((request, consumed)) = decode_message::<AgentRequest>(&read_buf)? {
            read_buf.drain(..consumed);

            let response = handle_request(request).await;
            let encoded = encode_message(&response)?;
            stream.write_all(&encoded).await?;
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

const B64_CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn base64_encode(data: &[u8]) -> String {
    let mut output = Vec::with_capacity(data.len().div_ceil(3) * 4);
    let mut i = 0;

    while i + 2 < data.len() {
        output.push(B64_CHARS[(data[i] >> 2) as usize]);
        output.push(B64_CHARS[(((data[i] & 0x03) << 4) | (data[i + 1] >> 4)) as usize]);
        output.push(B64_CHARS[(((data[i + 1] & 0x0f) << 2) | (data[i + 2] >> 6)) as usize]);
        output.push(B64_CHARS[(data[i + 2] & 0x3f) as usize]);
        i += 3;
    }

    let remaining = data.len() - i;
    match remaining {
        1 => {
            output.push(B64_CHARS[(data[i] >> 2) as usize]);
            output.push(B64_CHARS[((data[i] & 0x03) << 4) as usize]);
            output.push(b'=');
            output.push(b'=');
        }
        2 => {
            output.push(B64_CHARS[(data[i] >> 2) as usize]);
            output.push(B64_CHARS[(((data[i] & 0x03) << 4) | (data[i + 1] >> 4)) as usize]);
            output.push(B64_CHARS[((data[i + 1] & 0x0f) << 2) as usize]);
            output.push(b'=');
        }
        _ => {}
    }

    // B64_CHARS only contains ASCII bytes, so this is always valid UTF-8
    String::from_utf8(output).expect("base64 output is always valid UTF-8")
}

fn base64_decode(input: &str) -> Result<Vec<u8>, String> {
    let input = input.as_bytes();
    let mut output = Vec::with_capacity(input.len() * 3 / 4);

    let decode_char = |c: u8| -> Result<u8, String> {
        match c {
            b'A'..=b'Z' => Ok(c - b'A'),
            b'a'..=b'z' => Ok(c - b'a' + 26),
            b'0'..=b'9' => Ok(c - b'0' + 52),
            b'+' => Ok(62),
            b'/' => Ok(63),
            b'=' => Ok(0),
            _ => Err(format!("invalid base64 character: {c}")),
        }
    };

    let mut i = 0;
    while i < input.len() {
        if i + 4 > input.len() {
            return Err("invalid base64 length".into());
        }
        let a = decode_char(input[i])?;
        let b = decode_char(input[i + 1])?;
        let c = decode_char(input[i + 2])?;
        let d = decode_char(input[i + 3])?;

        output.push((a << 2) | (b >> 4));
        if input[i + 2] != b'=' {
            output.push((b << 4) | (c >> 2));
        }
        if input[i + 3] != b'=' {
            output.push((c << 6) | d);
        }

        i += 4;
    }

    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_roundtrip_empty() {
        let data = b"";
        let encoded = base64_encode(data);
        assert_eq!(encoded, "");
        let decoded = base64_decode(&encoded).unwrap();
        assert_eq!(decoded, data);
    }

    #[test]
    fn base64_roundtrip_one_byte() {
        let data = b"A";
        let encoded = base64_encode(data);
        assert_eq!(encoded, "QQ==");
        let decoded = base64_decode(&encoded).unwrap();
        assert_eq!(decoded, data);
    }

    #[test]
    fn base64_roundtrip_two_bytes() {
        let data = b"AB";
        let encoded = base64_encode(data);
        assert_eq!(encoded, "QUI=");
        let decoded = base64_decode(&encoded).unwrap();
        assert_eq!(decoded, data);
    }

    #[test]
    fn base64_roundtrip_three_bytes() {
        let data = b"ABC";
        let encoded = base64_encode(data);
        assert_eq!(encoded, "QUJD");
        let decoded = base64_decode(&encoded).unwrap();
        assert_eq!(decoded, data);
    }

    #[test]
    fn base64_roundtrip_hello_world() {
        let data = b"Hello, World!";
        let encoded = base64_encode(data);
        assert_eq!(encoded, "SGVsbG8sIFdvcmxkIQ==");
        let decoded = base64_decode(&encoded).unwrap();
        assert_eq!(decoded, data);
    }

    #[test]
    fn base64_roundtrip_binary_data() {
        let data: Vec<u8> = (0..=255).collect();
        let encoded = base64_encode(&data);
        let decoded = base64_decode(&encoded).unwrap();
        assert_eq!(decoded, data);
    }

    #[test]
    fn base64_decode_invalid_length() {
        assert!(base64_decode("QQ=").is_err());
    }

    #[test]
    fn base64_decode_invalid_char() {
        assert!(base64_decode("QQ!=").is_err());
    }
}
