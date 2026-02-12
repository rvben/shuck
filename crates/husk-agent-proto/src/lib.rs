use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

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

/// Read a single length-prefixed JSON message from an async stream.
///
/// Returns `None` on clean EOF (stream closed with no partial data).
/// Returns an error on truncated messages or protocol violations.
pub async fn read_message<T, S>(stream: &mut S) -> Result<Option<T>, ProtocolError>
where
    T: for<'de> Deserialize<'de>,
    S: tokio::io::AsyncRead + Unpin,
{
    // Read the 4-byte length prefix
    let mut len_buf = [0u8; 4];
    match stream.read_exact(&mut len_buf).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.into()),
    }

    let len = u32::from_be_bytes(len_buf) as usize;
    if len > MAX_MESSAGE_SIZE {
        return Err(ProtocolError::MessageTooLarge { size: len });
    }

    // Read the JSON payload
    let mut payload = vec![0u8; len];
    stream.read_exact(&mut payload).await?;

    let msg = serde_json::from_slice(&payload)?;
    Ok(Some(msg))
}

/// Write a single length-prefixed JSON message to an async stream.
pub async fn write_message<T, S>(stream: &mut S, msg: &T) -> Result<(), ProtocolError>
where
    T: Serialize,
    S: tokio::io::AsyncWrite + Unpin,
{
    let json = serde_json::to_vec(msg)?;
    if json.len() > MAX_MESSAGE_SIZE {
        return Err(ProtocolError::MessageTooLarge { size: json.len() });
    }
    let len = (json.len() as u32).to_be_bytes();
    stream.write_all(&len).await?;
    stream.write_all(&json).await?;
    stream.flush().await?;
    Ok(())
}

#[derive(Debug, thiserror::Error)]
pub enum ProtocolError {
    #[error("message too large: {size} bytes (max {MAX_MESSAGE_SIZE})")]
    MessageTooLarge { size: usize },
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

// --- Base64 encoding/decoding ---
// Both the guest agent and host-side client need base64 for file transfer.

const B64_CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub fn base64_encode(data: &[u8]) -> String {
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

pub fn base64_decode(input: &str) -> Result<Vec<u8>, String> {
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
