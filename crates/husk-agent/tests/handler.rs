use husk_agent_proto::{
    AgentRequest, AgentResponse, ExecRequest, ReadFileRequest, WriteFileRequest, base64_encode,
    read_message, write_message,
};

/// Spawn the agent handler on a temporary Unix socket and return a connected client stream.
async fn spawn_agent() -> tokio::net::UnixStream {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("agent.sock");
    let listener = tokio::net::UnixListener::bind(&path).unwrap();

    // Leak the tempdir so it lives for the test duration
    let _dir = Box::leak(Box::new(dir));

    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        husk_agent::handle_connection(stream).await.unwrap();
    });

    tokio::net::UnixStream::connect(&path).await.unwrap()
}

#[tokio::test]
async fn ping() {
    let mut stream = spawn_agent().await;

    write_message(&mut stream, &AgentRequest::Ping)
        .await
        .unwrap();
    let response: AgentResponse = read_message(&mut stream).await.unwrap().unwrap();
    assert!(matches!(response, AgentResponse::Pong));
}

#[tokio::test]
async fn exec_echo() {
    let mut stream = spawn_agent().await;

    let request = AgentRequest::Exec(ExecRequest {
        command: "echo".into(),
        args: vec!["hello".into()],
        working_dir: None,
        env: vec![],
    });
    write_message(&mut stream, &request).await.unwrap();

    let response: AgentResponse = read_message(&mut stream).await.unwrap().unwrap();
    match response {
        AgentResponse::Exec(r) => {
            assert_eq!(r.exit_code, 0);
            assert_eq!(r.stdout.trim(), "hello");
            assert!(r.stderr.is_empty());
        }
        _ => panic!("expected Exec response, got {response:?}"),
    }
}

#[tokio::test]
async fn exec_with_env() {
    let mut stream = spawn_agent().await;

    let request = AgentRequest::Exec(ExecRequest {
        command: "sh".into(),
        args: vec!["-c".into(), "echo $MY_VAR".into()],
        working_dir: None,
        env: vec![("MY_VAR".into(), "test_value".into())],
    });
    write_message(&mut stream, &request).await.unwrap();

    let response: AgentResponse = read_message(&mut stream).await.unwrap().unwrap();
    match response {
        AgentResponse::Exec(r) => {
            assert_eq!(r.exit_code, 0);
            assert_eq!(r.stdout.trim(), "test_value");
        }
        _ => panic!("expected Exec response, got {response:?}"),
    }
}

#[tokio::test]
async fn exec_nonexistent_command() {
    let mut stream = spawn_agent().await;

    let request = AgentRequest::Exec(ExecRequest {
        command: "nonexistent_command_12345".into(),
        args: vec![],
        working_dir: None,
        env: vec![],
    });
    write_message(&mut stream, &request).await.unwrap();

    let response: AgentResponse = read_message(&mut stream).await.unwrap().unwrap();
    match response {
        AgentResponse::Error(e) => {
            assert!(e.message.contains("exec failed"), "got: {}", e.message);
        }
        _ => panic!("expected Error response, got {response:?}"),
    }
}

#[tokio::test]
async fn write_then_read_file() {
    let mut stream = spawn_agent().await;

    let dir = tempfile::tempdir().unwrap();
    let file_path = dir.path().join("test.txt").to_string_lossy().into_owned();
    let content = b"hello from test";

    // Write file
    let request = AgentRequest::WriteFile(WriteFileRequest {
        path: file_path.clone(),
        data: base64_encode(content),
        mode: None,
    });
    write_message(&mut stream, &request).await.unwrap();

    let response: AgentResponse = read_message(&mut stream).await.unwrap().unwrap();
    match response {
        AgentResponse::WriteFile(w) => {
            assert_eq!(w.bytes_written, content.len() as u64);
        }
        _ => panic!("expected WriteFile response, got {response:?}"),
    }

    // Read it back
    let request = AgentRequest::ReadFile(ReadFileRequest { path: file_path });
    write_message(&mut stream, &request).await.unwrap();

    let response: AgentResponse = read_message(&mut stream).await.unwrap().unwrap();
    match response {
        AgentResponse::ReadFile(r) => {
            assert_eq!(r.size, content.len() as u64);
            let decoded = husk_agent_proto::base64_decode(&r.data).unwrap();
            assert_eq!(decoded, content);
        }
        _ => panic!("expected ReadFile response, got {response:?}"),
    }
}

#[tokio::test]
async fn read_nonexistent_file() {
    let mut stream = spawn_agent().await;

    let request = AgentRequest::ReadFile(ReadFileRequest {
        path: "/tmp/nonexistent_file_12345_husk_test".into(),
    });
    write_message(&mut stream, &request).await.unwrap();

    let response: AgentResponse = read_message(&mut stream).await.unwrap().unwrap();
    match response {
        AgentResponse::Error(e) => {
            assert!(e.message.contains("read failed"), "got: {}", e.message);
        }
        _ => panic!("expected Error response, got {response:?}"),
    }
}

#[tokio::test]
async fn multiple_operations_on_one_connection() {
    let mut stream = spawn_agent().await;

    // Ping
    write_message(&mut stream, &AgentRequest::Ping)
        .await
        .unwrap();
    let response: AgentResponse = read_message(&mut stream).await.unwrap().unwrap();
    assert!(matches!(response, AgentResponse::Pong));

    // Exec
    let request = AgentRequest::Exec(ExecRequest {
        command: "echo".into(),
        args: vec!["test".into()],
        working_dir: None,
        env: vec![],
    });
    write_message(&mut stream, &request).await.unwrap();
    let response: AgentResponse = read_message(&mut stream).await.unwrap().unwrap();
    assert!(matches!(response, AgentResponse::Exec(_)));

    // Ping again
    write_message(&mut stream, &AgentRequest::Ping)
        .await
        .unwrap();
    let response: AgentResponse = read_message(&mut stream).await.unwrap().unwrap();
    assert!(matches!(response, AgentResponse::Pong));
}
