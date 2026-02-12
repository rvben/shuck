use serde::{Deserialize, Serialize};

/// Messages sent from the host to the guest agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum AgentRequest {
    /// Execute a command inside the VM.
    Exec(ExecRequest),

    /// Read a file from the guest filesystem.
    ReadFile(ReadFileRequest),

    /// Write a file to the guest filesystem.
    WriteFile(WriteFileRequest),

    /// Check if the agent is alive.
    Ping,
}

/// Messages sent from the guest agent back to the host.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum AgentResponse {
    /// Result of a command execution.
    Exec(ExecResponse),

    /// Contents of a file read from the guest.
    ReadFile(ReadFileResponse),

    /// Acknowledgement of a file write.
    WriteFile(WriteFileResponse),

    /// Response to a ping.
    Pong,

    /// An error occurred processing the request.
    Error(ErrorResponse),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecRequest {
    pub command: String,
    pub args: Vec<String>,
    pub working_dir: Option<String>,
    pub env: Vec<(String, String)>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecResponse {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReadFileRequest {
    pub path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReadFileResponse {
    /// Base64-encoded file contents.
    pub data: String,
    pub size: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WriteFileRequest {
    pub path: String,
    /// Base64-encoded file contents.
    pub data: String,
    pub mode: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WriteFileResponse {
    pub bytes_written: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorResponse {
    pub message: String,
}

/// Encode a message as a length-prefixed JSON frame.
///
/// Wire format: 4-byte big-endian length prefix followed by JSON bytes.
pub fn encode_message<T: Serialize>(msg: &T) -> Result<Vec<u8>, ProtocolError> {
    let json = serde_json::to_vec(msg)?;
    if json.len() > MAX_MESSAGE_SIZE {
        return Err(ProtocolError::MessageTooLarge { size: json.len() });
    }
    let len = (json.len() as u32).to_be_bytes();
    let mut buf = Vec::with_capacity(4 + json.len());
    buf.extend_from_slice(&len);
    buf.extend_from_slice(&json);
    Ok(buf)
}

/// Decode a length-prefixed JSON frame from a byte buffer.
///
/// Returns the decoded message and the number of bytes consumed,
/// or `None` if the buffer doesn't contain a complete frame.
pub fn decode_message<T: for<'de> Deserialize<'de>>(
    buf: &[u8],
) -> Result<Option<(T, usize)>, ProtocolError> {
    if buf.len() < 4 {
        return Ok(None);
    }
    let len = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
    if len > MAX_MESSAGE_SIZE {
        return Err(ProtocolError::MessageTooLarge { size: len });
    }
    if buf.len() < 4 + len {
        return Ok(None);
    }
    let msg = serde_json::from_slice(&buf[4..4 + len])?;
    Ok(Some((msg, 4 + len)))
}

/// Maximum allowed message size (16 MiB) to prevent unbounded allocations.
pub const MAX_MESSAGE_SIZE: usize = 16 * 1024 * 1024;

/// Default vsock port the guest agent listens on.
pub const AGENT_VSOCK_PORT: u32 = 52;

#[derive(Debug, thiserror::Error)]
pub enum ProtocolError {
    #[error("message too large: {size} bytes (max {MAX_MESSAGE_SIZE})")]
    MessageTooLarge { size: usize },
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_exec_request() {
        let req = AgentRequest::Exec(ExecRequest {
            command: "ls".into(),
            args: vec!["-la".into()],
            working_dir: Some("/tmp".into()),
            env: vec![("FOO".into(), "bar".into())],
        });
        let encoded = encode_message(&req).unwrap();
        let (decoded, consumed): (AgentRequest, usize) = decode_message(&encoded).unwrap().unwrap();
        assert_eq!(consumed, encoded.len());
        match decoded {
            AgentRequest::Exec(e) => {
                assert_eq!(e.command, "ls");
                assert_eq!(e.args, vec!["-la"]);
            }
            _ => panic!("expected Exec"),
        }
    }

    #[test]
    fn roundtrip_ping_pong() {
        let ping = AgentRequest::Ping;
        let encoded = encode_message(&ping).unwrap();
        let (decoded, _): (AgentRequest, usize) = decode_message(&encoded).unwrap().unwrap();
        assert!(matches!(decoded, AgentRequest::Ping));

        let pong = AgentResponse::Pong;
        let encoded = encode_message(&pong).unwrap();
        let (decoded, _): (AgentResponse, usize) = decode_message(&encoded).unwrap().unwrap();
        assert!(matches!(decoded, AgentResponse::Pong));
    }

    #[test]
    fn incomplete_buffer_returns_none() {
        let req = AgentRequest::Ping;
        let encoded = encode_message(&req).unwrap();
        // Truncate the buffer
        let partial = &encoded[..encoded.len() - 1];
        let result: Result<Option<(AgentRequest, usize)>, _> = decode_message(partial);
        assert!(result.unwrap().is_none());
    }

    #[test]
    fn empty_buffer_returns_none() {
        let result: Result<Option<(AgentRequest, usize)>, _> = decode_message(&[]);
        assert!(result.unwrap().is_none());
    }

    #[test]
    fn oversized_message_rejected() {
        let mut buf = vec![0u8; 8];
        let bogus_len = (MAX_MESSAGE_SIZE as u32 + 1).to_be_bytes();
        buf[..4].copy_from_slice(&bogus_len);
        let result: Result<Option<(AgentRequest, usize)>, _> = decode_message(&buf);
        assert!(matches!(result, Err(ProtocolError::MessageTooLarge { .. })));
    }
}
