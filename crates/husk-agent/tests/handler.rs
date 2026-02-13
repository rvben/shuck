use husk_agent_proto::{
    AgentRequest, AgentResponse, ExecRequest, ReadFileRequest, ShellDataRequest,
    ShellResizeRequest, ShellStartRequest, WriteFileRequest, base64_decode, base64_encode,
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

#[tokio::test]
async fn shell_echo_with_cat() {
    let mut stream = spawn_agent().await;

    // Start shell with `cat` which echoes stdin to stdout
    let request = AgentRequest::ShellStart(ShellStartRequest {
        command: Some("cat".into()),
        env: vec![],
        cols: 80,
        rows: 24,
    });
    write_message(&mut stream, &request).await.unwrap();

    let response: AgentResponse = read_message(&mut stream).await.unwrap().unwrap();
    assert!(
        matches!(response, AgentResponse::ShellStarted),
        "expected ShellStarted, got {response:?}"
    );

    // Send data to cat via stdin
    let input = b"hello\n";
    let request = AgentRequest::ShellData(ShellDataRequest {
        data: base64_encode(input),
    });
    write_message(&mut stream, &request).await.unwrap();

    // Read echoed output
    let response: AgentResponse = read_message(&mut stream).await.unwrap().unwrap();
    match response {
        AgentResponse::ShellData(d) => {
            let bytes = base64_decode(&d.data).unwrap();
            assert_eq!(bytes, b"hello\n");
        }
        other => panic!("expected ShellData, got {other:?}"),
    }

    // Drop the stream to close stdin, which causes cat to exit
    drop(stream);
}

#[tokio::test]
async fn shell_immediate_exit() {
    let mut stream = spawn_agent().await;

    // Start shell with a command that exits immediately with output
    let request = AgentRequest::ShellStart(ShellStartRequest {
        command: Some("echo".into()),
        env: vec![],
        cols: 80,
        rows: 24,
    });
    write_message(&mut stream, &request).await.unwrap();

    let response: AgentResponse = read_message(&mut stream).await.unwrap().unwrap();
    assert!(
        matches!(response, AgentResponse::ShellStarted),
        "expected ShellStarted, got {response:?}"
    );

    // Collect all responses until ShellExit
    let mut output_data = Vec::new();
    let exit_code = loop {
        let response: AgentResponse = read_message(&mut stream).await.unwrap().unwrap();
        match response {
            AgentResponse::ShellData(d) => {
                output_data.extend(base64_decode(&d.data).unwrap());
            }
            AgentResponse::ShellExit(e) => {
                break e.exit_code;
            }
            other => panic!("unexpected response: {other:?}"),
        }
    };

    // `echo` with no args outputs a newline
    assert_eq!(output_data, b"\n");
    assert_eq!(exit_code, 0);
}

#[tokio::test]
async fn shell_nonzero_exit_code() {
    let mut stream = spawn_agent().await;

    let request = AgentRequest::ShellStart(ShellStartRequest {
        command: Some("sh".into()),
        env: vec![],
        cols: 80,
        rows: 24,
    });
    write_message(&mut stream, &request).await.unwrap();

    let response: AgentResponse = read_message(&mut stream).await.unwrap().unwrap();
    assert!(matches!(response, AgentResponse::ShellStarted));

    // Send exit 42 to the shell
    let request = AgentRequest::ShellData(ShellDataRequest {
        data: base64_encode(b"exit 42\n"),
    });
    write_message(&mut stream, &request).await.unwrap();

    // Collect until ShellExit
    let exit_code = loop {
        let response: AgentResponse = read_message(&mut stream).await.unwrap().unwrap();
        match response {
            AgentResponse::ShellData(_) => {}
            AgentResponse::ShellExit(e) => {
                break e.exit_code;
            }
            other => panic!("unexpected response: {other:?}"),
        }
    };

    assert_eq!(exit_code, 42);
}

#[tokio::test]
async fn shell_resize_accepted() {
    let mut stream = spawn_agent().await;

    let request = AgentRequest::ShellStart(ShellStartRequest {
        command: Some("cat".into()),
        env: vec![],
        cols: 80,
        rows: 24,
    });
    write_message(&mut stream, &request).await.unwrap();

    let response: AgentResponse = read_message(&mut stream).await.unwrap().unwrap();
    assert!(matches!(response, AgentResponse::ShellStarted));

    // Send resize — should be accepted without error
    let request = AgentRequest::ShellResize(ShellResizeRequest {
        cols: 120,
        rows: 40,
    });
    write_message(&mut stream, &request).await.unwrap();

    // Send data to verify the shell is still alive after resize
    let request = AgentRequest::ShellData(ShellDataRequest {
        data: base64_encode(b"test\n"),
    });
    write_message(&mut stream, &request).await.unwrap();

    let response: AgentResponse = read_message(&mut stream).await.unwrap().unwrap();
    match response {
        AgentResponse::ShellData(d) => {
            let bytes = base64_decode(&d.data).unwrap();
            assert_eq!(bytes, b"test\n");
        }
        other => panic!("expected ShellData, got {other:?}"),
    }

    drop(stream);
}

#[tokio::test]
async fn normal_requests_still_work_with_shell_protocol() {
    // Regression test: verify existing request types still work
    // after the shell protocol was added
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
        args: vec!["regression_test".into()],
        working_dir: None,
        env: vec![],
    });
    write_message(&mut stream, &request).await.unwrap();
    let response: AgentResponse = read_message(&mut stream).await.unwrap().unwrap();
    match response {
        AgentResponse::Exec(r) => {
            assert_eq!(r.exit_code, 0);
            assert_eq!(r.stdout.trim(), "regression_test");
        }
        _ => panic!("expected Exec response, got {response:?}"),
    }

    // Write + Read file
    let dir = tempfile::tempdir().unwrap();
    let file_path = dir
        .path()
        .join("shell_test.txt")
        .to_string_lossy()
        .into_owned();
    let content = b"shell protocol regression";

    let request = AgentRequest::WriteFile(WriteFileRequest {
        path: file_path.clone(),
        data: base64_encode(content),
        mode: None,
    });
    write_message(&mut stream, &request).await.unwrap();
    let response: AgentResponse = read_message(&mut stream).await.unwrap().unwrap();
    assert!(matches!(response, AgentResponse::WriteFile(_)));

    let request = AgentRequest::ReadFile(ReadFileRequest { path: file_path });
    write_message(&mut stream, &request).await.unwrap();
    let response: AgentResponse = read_message(&mut stream).await.unwrap().unwrap();
    match response {
        AgentResponse::ReadFile(r) => {
            let decoded = base64_decode(&r.data).unwrap();
            assert_eq!(decoded, content);
        }
        _ => panic!("expected ReadFile response, got {response:?}"),
    }
}

#[tokio::test]
async fn shell_nonexistent_command() {
    let mut stream = spawn_agent().await;

    let request = AgentRequest::ShellStart(ShellStartRequest {
        command: Some("nonexistent_command_12345".into()),
        env: vec![],
        cols: 80,
        rows: 24,
    });
    write_message(&mut stream, &request).await.unwrap();

    let response: AgentResponse = read_message(&mut stream).await.unwrap().unwrap();
    match response {
        AgentResponse::Error(e) => {
            assert!(
                e.message.contains("failed to start shell"),
                "got: {}",
                e.message
            );
        }
        other => panic!("expected Error, got {other:?}"),
    }
}

#[tokio::test]
async fn shell_with_env_vars() {
    let mut stream = spawn_agent().await;

    let request = AgentRequest::ShellStart(ShellStartRequest {
        command: Some("sh".into()),
        env: vec![("MY_SHELL_VAR".into(), "shell_value".into())],
        cols: 80,
        rows: 24,
    });
    write_message(&mut stream, &request).await.unwrap();

    let response: AgentResponse = read_message(&mut stream).await.unwrap().unwrap();
    assert!(matches!(response, AgentResponse::ShellStarted));

    // Ask the shell to print the env var
    let request = AgentRequest::ShellData(ShellDataRequest {
        data: base64_encode(b"echo $MY_SHELL_VAR\nexit 0\n"),
    });
    write_message(&mut stream, &request).await.unwrap();

    // Collect output
    let mut output_data = Vec::new();
    loop {
        let response: AgentResponse = read_message(&mut stream).await.unwrap().unwrap();
        match response {
            AgentResponse::ShellData(d) => {
                output_data.extend(base64_decode(&d.data).unwrap());
            }
            AgentResponse::ShellExit(e) => {
                assert_eq!(e.exit_code, 0);
                break;
            }
            other => panic!("unexpected response: {other:?}"),
        }
    }

    let output = String::from_utf8_lossy(&output_data);
    assert!(
        output.contains("shell_value"),
        "expected output to contain 'shell_value', got: {output}"
    );
}
