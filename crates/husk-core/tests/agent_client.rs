use husk_core::agent_client::{AgentClient, AgentConnection};

/// Spawn the agent handler on a temporary Unix socket and return the socket path.
async fn spawn_agent() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("agent.sock");
    let listener = tokio::net::UnixListener::bind(&path).unwrap();

    tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            tokio::spawn(async move {
                let _ = husk_agent::handle_connection(stream).await;
            });
        }
    });

    // Give the listener a moment to start
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;

    (dir, path)
}

#[tokio::test]
async fn client_ping() {
    let (_dir, path) = spawn_agent().await;

    let mut conn = AgentClient::connect_unix(&path).await.unwrap();
    conn.ping().await.unwrap();
}

#[tokio::test]
async fn client_exec() {
    let (_dir, path) = spawn_agent().await;

    let mut conn = AgentClient::connect_unix(&path).await.unwrap();
    let result = conn
        .exec("echo", &["hello world"], None, &[])
        .await
        .unwrap();
    assert_eq!(result.exit_code, 0);
    assert_eq!(result.stdout.trim(), "hello world");
    assert!(result.stderr.is_empty());
}

#[tokio::test]
async fn client_exec_with_env_and_workdir() {
    let (_dir, path) = spawn_agent().await;

    let mut conn = AgentClient::connect_unix(&path).await.unwrap();
    let result = conn
        .exec(
            "sh",
            &["-c", "echo $TEST_VAR && pwd"],
            Some("/tmp"),
            &[("TEST_VAR", "husk_test")],
        )
        .await
        .unwrap();
    assert_eq!(result.exit_code, 0);
    let lines: Vec<&str> = result.stdout.trim().lines().collect();
    assert_eq!(lines[0], "husk_test");
    // macOS uses /private/tmp
    assert!(
        lines[1] == "/tmp" || lines[1] == "/private/tmp",
        "unexpected pwd: {}",
        lines[1]
    );
}

#[tokio::test]
async fn client_exec_failure() {
    let (_dir, path) = spawn_agent().await;

    let mut conn = AgentClient::connect_unix(&path).await.unwrap();
    let result = conn
        .exec("sh", &["-c", "exit 42"], None, &[])
        .await
        .unwrap();
    assert_eq!(result.exit_code, 42);
}

#[tokio::test]
async fn client_write_and_read_file() {
    let (_dir, path) = spawn_agent().await;

    let file_dir = tempfile::tempdir().unwrap();
    let file_path = file_dir.path().join("test.bin");
    let file_path_str = file_path.to_string_lossy();

    let data = b"binary data \x00\x01\x02\xff";

    let mut conn = AgentClient::connect_unix(&path).await.unwrap();

    let bytes_written = conn.write_file(&file_path_str, data, None).await.unwrap();
    assert_eq!(bytes_written, data.len() as u64);

    let read_back = conn.read_file(&file_path_str).await.unwrap();
    assert_eq!(read_back, data);
}

#[tokio::test]
async fn client_read_nonexistent_file() {
    let (_dir, path) = spawn_agent().await;

    let mut conn = AgentClient::connect_unix(&path).await.unwrap();
    let result = conn.read_file("/nonexistent/path/12345").await;
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(err.contains("read failed"), "got: {err}");
}

#[tokio::test]
async fn client_multiple_operations() {
    let (_dir, path) = spawn_agent().await;

    let mut conn = AgentClient::connect_unix(&path).await.unwrap();

    // Ping
    conn.ping().await.unwrap();

    // Exec
    let result = conn.exec("echo", &["one"], None, &[]).await.unwrap();
    assert_eq!(result.stdout.trim(), "one");

    // Another exec
    let result = conn.exec("echo", &["two"], None, &[]).await.unwrap();
    assert_eq!(result.stdout.trim(), "two");

    // Ping again
    conn.ping().await.unwrap();
}

#[tokio::test]
async fn agent_connection_from_raw_stream() {
    let (_dir, path) = spawn_agent().await;

    // Test using AgentConnection::new() with a raw stream
    let stream = tokio::net::UnixStream::connect(&path).await.unwrap();
    let mut conn = AgentConnection::new(stream);
    conn.ping().await.unwrap();

    let result = conn.exec("echo", &["raw"], None, &[]).await.unwrap();
    assert_eq!(result.stdout.trim(), "raw");
}
