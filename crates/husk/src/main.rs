use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use tokio::io::AsyncReadExt;
use tokio_tungstenite::tungstenite;

#[derive(Parser)]
#[command(name = "husk", about = "An open source microVM manager", version)]
struct Cli {
    /// Path to config file
    #[arg(long)]
    config: Option<PathBuf>,

    /// Daemon API address (for client commands)
    #[arg(long, default_value = "http://127.0.0.1:7777")]
    api_url: String,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Start the husk daemon
    Daemon {
        /// Address to listen on
        #[arg(long, default_value = "127.0.0.1:7777")]
        listen: SocketAddr,
    },

    /// Create and boot a new VM
    Run {
        /// Path to rootfs ext4 image
        rootfs: PathBuf,

        /// VM name
        #[arg(long)]
        name: Option<String>,

        /// Path to kernel (vmlinux)
        #[arg(long)]
        kernel: Option<PathBuf>,

        /// Path to initrd/initramfs (auto-detected if not specified)
        #[arg(long)]
        initrd: Option<PathBuf>,

        /// Number of vCPUs
        #[arg(long, default_value = "1")]
        cpus: u32,

        /// Memory in MiB
        #[arg(long, default_value = "128")]
        memory: u32,

        /// Path to userdata script to execute after VM boots
        #[arg(long)]
        userdata: Option<PathBuf>,
    },

    /// List running VMs
    #[command(alias = "ls")]
    List,

    /// Get info about a VM
    Info {
        /// VM name
        name: String,
    },

    /// Stop a running VM
    Stop {
        /// VM name
        name: String,
    },

    /// Pause a running VM
    Pause {
        /// VM name
        name: String,
    },

    /// Resume a paused VM
    Resume {
        /// VM name
        name: String,
    },

    /// Destroy a VM and clean up resources
    #[command(alias = "rm")]
    Destroy {
        /// VM name
        name: String,
    },

    /// Execute a command in a VM
    Exec {
        /// VM name
        name: String,

        /// Working directory inside the VM
        #[arg(long, short = 'w')]
        workdir: Option<String>,

        /// Command and arguments (after --)
        #[arg(last = true, required = true)]
        command: Vec<String>,
    },

    /// Copy files between host and VM
    ///
    /// Use vmname:/path syntax for VM paths:
    ///   husk cp local.txt myvm:/tmp/local.txt
    ///   husk cp myvm:/var/log/syslog ./syslog
    Cp {
        /// Source (local path or vmname:/guest/path)
        source: String,

        /// Destination (local path or vmname:/guest/path)
        dest: String,

        /// File mode (octal, e.g. 755) when copying to VM
        #[arg(long, value_parser = parse_octal_mode)]
        mode: Option<u32>,
    },

    /// Manage port forwards for a VM
    #[command(alias = "pf")]
    PortForward {
        /// VM name
        name: String,
        #[command(subcommand)]
        action: PortForwardAction,
    },

    /// Open an interactive shell in a VM
    Shell {
        /// VM name
        name: String,
        /// Shell command (default: /bin/sh)
        #[arg(long)]
        command: Option<String>,
    },
}

#[derive(Subcommand)]
enum PortForwardAction {
    /// Add a port forward
    Add {
        /// Host port
        host_port: u16,
        /// Guest port
        guest_port: u16,
    },
    /// Remove a port forward
    Remove {
        /// Host port
        host_port: u16,
    },
    /// List port forwards
    List,
}

#[derive(Debug, Deserialize)]
struct Config {
    #[cfg(feature = "linux-net")]
    #[serde(default = "default_firecracker_bin")]
    firecracker_bin: PathBuf,
    #[serde(default = "default_data_dir")]
    data_dir: PathBuf,
    #[serde(default = "default_kernel_path")]
    default_kernel: PathBuf,
    #[cfg(feature = "linux-net")]
    #[serde(default = "default_host_interface")]
    host_interface: String,
}

#[cfg(feature = "linux-net")]
fn default_firecracker_bin() -> PathBuf {
    PathBuf::from("firecracker")
}

fn default_data_dir() -> PathBuf {
    if cfg!(target_os = "macos")
        && let Some(home) = std::env::var_os("HOME")
    {
        return PathBuf::from(home).join(".local/share/husk");
    }
    PathBuf::from("/var/lib/husk")
}

fn default_kernel_path() -> PathBuf {
    let data_dir = default_data_dir();
    if cfg!(target_os = "macos") {
        data_dir.join("kernels/Image-virt")
    } else {
        data_dir.join("kernels/vmlinux")
    }
}

#[cfg(feature = "linux-net")]
fn default_host_interface() -> String {
    "eth0".into()
}

/// Extract a clean error message from an API error response.
///
/// Handles JSON error bodies, plain text, and empty responses gracefully
/// so the CLI never dumps raw stack traces at the user.
async fn api_error(resp: reqwest::Response, subject: &str) -> String {
    let status = resp.status();
    match resp.text().await {
        Ok(body) if !body.is_empty() => {
            if let Ok(json) = serde_json::from_str::<serde_json::Value>(&body)
                && let Some(msg) = json["error"].as_str()
            {
                return msg.to_string();
            }
            body
        }
        _ => match status.as_u16() {
            404 => format!("{subject} not found"),
            409 => format!("{subject} already exists"),
            _ => format!("{subject}: {status}"),
        },
    }
}

/// Send a request to the daemon API and return the response.
///
/// Wraps connection errors with a hint about whether the daemon is running.
async fn api_request(request: reqwest::RequestBuilder) -> Result<reqwest::Response> {
    request.send().await.map_err(|e| {
        if e.is_connect() {
            anyhow::anyhow!("cannot connect to daemon (is `husk daemon` running?)")
        } else {
            anyhow::anyhow!("{e}")
        }
    })
}

impl Default for Config {
    fn default() -> Self {
        Self {
            #[cfg(feature = "linux-net")]
            firecracker_bin: default_firecracker_bin(),
            data_dir: default_data_dir(),
            default_kernel: default_kernel_path(),
            #[cfg(feature = "linux-net")]
            host_interface: default_host_interface(),
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("husk=info".parse().unwrap()),
        )
        .init();

    let cli = Cli::parse();

    match cli.command {
        Commands::Daemon { listen } => {
            let config = load_config(cli.config.as_deref());
            start_daemon(config, listen).await
        }
        Commands::Run {
            rootfs,
            name,
            kernel,
            initrd,
            cpus,
            memory,
            userdata,
        } => {
            let config = load_config(cli.config.as_deref());
            let kernel = kernel.unwrap_or(config.default_kernel);
            let name =
                name.unwrap_or_else(|| format!("vm-{}", &uuid::Uuid::new_v4().to_string()[..8]));

            let mut body = serde_json::json!({
                "name": name,
                "kernel_path": kernel,
                "rootfs_path": rootfs,
                "vcpu_count": cpus,
                "mem_size_mib": memory,
            });
            if let Some(ref initrd_path) = initrd {
                body["initrd_path"] = serde_json::json!(initrd_path);
            }
            if let Some(ref userdata_path) = userdata {
                let script = std::fs::read_to_string(userdata_path).with_context(|| {
                    format!("reading userdata script {}", userdata_path.display())
                })?;
                body["userdata"] = serde_json::json!(script);
            }

            let client = reqwest::Client::new();
            let resp =
                api_request(client.post(format!("{}/v1/vms", cli.api_url)).json(&body)).await?;

            if !resp.status().is_success() {
                let msg = api_error(resp, &format!("VM '{name}'")).await;
                eprintln!("Error: {msg}");
                if msg.contains("already exists") {
                    eprintln!("Hint: stop or destroy it first with `husk destroy {name}`");
                }
                std::process::exit(1);
            }

            let vm: serde_json::Value = resp.json().await?;
            println!("Created VM: {}", vm["name"].as_str().unwrap_or("-"));
            println!("  ID:    {}", vm["id"].as_str().unwrap_or("-"));
            println!("  State: {}", vm["state"].as_str().unwrap_or("-"));
            println!("  CPUs:  {}", vm["vcpu_count"]);
            println!("  RAM:   {} MiB", vm["mem_size_mib"]);

            if userdata.is_some() {
                println!("  Userdata script queued (check status with `husk info {name}`)");
            }
            Ok(())
        }
        Commands::List => {
            let client = reqwest::Client::new();
            let resp = api_request(client.get(format!("{}/v1/vms", cli.api_url))).await?;

            if !resp.status().is_success() {
                eprintln!("Error: {}", api_error(resp, "listing VMs").await);
                std::process::exit(1);
            }

            let vms: Vec<serde_json::Value> = resp.json().await?;
            if vms.is_empty() {
                println!("No VMs found");
            } else {
                println!(
                    "{:<20} {:<12} {:>4}   {:<10} {:<16}",
                    "NAME", "STATE", "CPUS", "MEMORY", "GUEST IP"
                );
                for vm in vms {
                    println!(
                        "{:<20} {:<12} {:>4}   {:>4} MiB   {:<16}",
                        vm["name"].as_str().unwrap_or("-"),
                        vm["state"].as_str().unwrap_or("-"),
                        vm["vcpu_count"],
                        vm["mem_size_mib"],
                        vm["guest_ip"].as_str().unwrap_or("-"),
                    );
                }
            }
            Ok(())
        }
        Commands::Info { name } => {
            let client = reqwest::Client::new();
            let resp = api_request(client.get(format!("{}/v1/vms/{name}", cli.api_url))).await?;

            if !resp.status().is_success() {
                eprintln!("Error: {}", api_error(resp, &format!("VM '{name}'")).await);
                std::process::exit(1);
            }

            let vm: serde_json::Value = resp.json().await?;
            let s = |key: &str| vm[key].as_str().unwrap_or("-").to_string();
            println!("Name:      {}", s("name"));
            println!("State:     {}", s("state"));
            println!("vCPUs:     {}", vm["vcpu_count"]);
            println!("Memory:    {} MiB", vm["mem_size_mib"]);
            if let Some(ip) = vm["guest_ip"].as_str() {
                println!("Guest IP:  {ip}");
            }
            if let Some(ip) = vm["host_ip"].as_str() {
                println!("Host IP:   {ip}");
            }
            if let Some(status) = vm["userdata_status"].as_str() {
                println!("Userdata:  {status}");
            }
            println!("ID:        {}", s("id"));
            Ok(())
        }
        Commands::Stop { name } => {
            let client = reqwest::Client::new();
            let resp =
                api_request(client.post(format!("{}/v1/vms/{name}/stop", cli.api_url))).await?;

            if resp.status().is_success() {
                println!("Stopped VM: {name}");
            } else {
                let msg = api_error(resp, &format!("VM '{name}'")).await;
                eprintln!("Error: {msg}");
                if msg.contains("stopped") {
                    eprintln!("Hint: VM is already stopped");
                }
                std::process::exit(1);
            }
            Ok(())
        }
        Commands::Pause { name } => {
            let client = reqwest::Client::new();
            let resp =
                api_request(client.post(format!("{}/v1/vms/{name}/pause", cli.api_url))).await?;

            if resp.status().is_success() {
                println!("Paused VM: {name}");
            } else {
                let msg = api_error(resp, &format!("VM '{name}'")).await;
                eprintln!("Error: {msg}");
                if msg.contains("stopped") {
                    eprintln!("Hint: start the VM first with `husk run`");
                }
                std::process::exit(1);
            }
            Ok(())
        }
        Commands::Resume { name } => {
            let client = reqwest::Client::new();
            let resp =
                api_request(client.post(format!("{}/v1/vms/{name}/resume", cli.api_url))).await?;

            if resp.status().is_success() {
                println!("Resumed VM: {name}");
            } else {
                let msg = api_error(resp, &format!("VM '{name}'")).await;
                eprintln!("Error: {msg}");
                if msg.contains("stopped") {
                    eprintln!("Hint: start the VM first with `husk run`");
                } else if msg.contains("running") {
                    eprintln!("Hint: VM is already running, nothing to resume");
                }
                std::process::exit(1);
            }
            Ok(())
        }
        Commands::Destroy { name } => {
            let client = reqwest::Client::new();
            let resp = api_request(client.delete(format!("{}/v1/vms/{name}", cli.api_url))).await?;

            if resp.status().is_success() {
                println!("Destroyed VM: {name}");
            } else {
                eprintln!("Error: {}", api_error(resp, &format!("VM '{name}'")).await);
                std::process::exit(1);
            }
            Ok(())
        }
        Commands::Exec {
            name,
            workdir,
            command,
        } => {
            let (cmd, args) = command.split_first().context("command required after --")?;

            let mut body = serde_json::json!({
                "command": cmd,
                "args": args,
            });
            if let Some(ref wd) = workdir {
                body["working_dir"] = serde_json::json!(wd);
            }

            let client = reqwest::Client::new();
            let resp = api_request(
                client
                    .post(format!("{}/v1/vms/{name}/exec", cli.api_url))
                    .json(&body),
            )
            .await?;

            if !resp.status().is_success() {
                eprintln!("Error: {}", api_error(resp, &format!("VM '{name}'")).await);
                std::process::exit(1);
            }

            let result: serde_json::Value = resp.json().await?;
            let stdout = result["stdout"].as_str().unwrap_or("");
            let stderr = result["stderr"].as_str().unwrap_or("");
            if !stdout.is_empty() {
                print!("{stdout}");
            }
            if !stderr.is_empty() {
                eprint!("{stderr}");
            }
            let exit_code = result["exit_code"].as_i64().unwrap_or(1) as i32;
            if exit_code != 0 {
                std::process::exit(exit_code);
            }
            Ok(())
        }
        Commands::Cp { source, dest, mode } => {
            let src = parse_cp_path(&source);
            let dst = parse_cp_path(&dest);

            match (src, dst) {
                (CpPath::Local(local), CpPath::Vm { name, path }) => {
                    let data = std::fs::read(&local)
                        .with_context(|| format!("reading {}", local.display()))?;
                    let encoded = husk_agent_proto::base64_encode(&data);

                    let mut body = serde_json::json!({
                        "path": path,
                        "data": encoded,
                    });
                    if let Some(m) = mode {
                        body["mode"] = serde_json::json!(m);
                    }

                    let client = reqwest::Client::new();
                    let resp = api_request(
                        client
                            .post(format!("{}/v1/vms/{name}/files/write", cli.api_url))
                            .json(&body),
                    )
                    .await?;

                    if resp.status().is_success() {
                        let result: serde_json::Value = resp.json().await?;
                        let bytes = result["bytes_written"].as_u64().unwrap_or(0);
                        println!("{bytes} bytes copied to {name}:{path}");
                    } else {
                        eprintln!("Error: {}", api_error(resp, &format!("VM '{name}'")).await);
                        std::process::exit(1);
                    }
                }
                (CpPath::Vm { name, path }, CpPath::Local(local)) => {
                    let client = reqwest::Client::new();
                    let resp = api_request(
                        client
                            .post(format!("{}/v1/vms/{name}/files/read", cli.api_url))
                            .json(&serde_json::json!({ "path": path })),
                    )
                    .await?;

                    if resp.status().is_success() {
                        let result: serde_json::Value = resp.json().await?;
                        let b64 = result["data"].as_str().unwrap_or("");
                        let data = husk_agent_proto::base64_decode(b64)
                            .map_err(|e| anyhow::anyhow!("invalid base64 from server: {e}"))?;
                        std::fs::write(&local, &data)
                            .with_context(|| format!("writing {}", local.display()))?;
                        println!("{} bytes copied from {name}:{path}", data.len());
                    } else {
                        eprintln!("Error: {}", api_error(resp, &format!("VM '{name}'")).await);
                        std::process::exit(1);
                    }
                }
                (CpPath::Local(_), CpPath::Local(_)) => {
                    anyhow::bail!(
                        "both source and destination are local paths; prefix one with vmname:"
                    );
                }
                (CpPath::Vm { .. }, CpPath::Vm { .. }) => {
                    anyhow::bail!("VM-to-VM copy is not supported; copy to local first");
                }
            }
            Ok(())
        }
        Commands::PortForward { name, action } => {
            let client = reqwest::Client::new();
            match action {
                PortForwardAction::Add {
                    host_port,
                    guest_port,
                } => {
                    let resp = api_request(
                        client
                            .post(format!("{}/v1/vms/{name}/ports", cli.api_url))
                            .json(&serde_json::json!({
                                "host_port": host_port,
                                "guest_port": guest_port,
                            })),
                    )
                    .await?;
                    if resp.status().is_success() {
                        println!("Port forward added: {host_port} -> {name}:{guest_port}");
                    } else {
                        eprintln!("Error: {}", api_error(resp, &format!("VM '{name}'")).await);
                        std::process::exit(1);
                    }
                }
                PortForwardAction::Remove { host_port } => {
                    let resp = api_request(
                        client.delete(format!("{}/v1/vms/{name}/ports/{host_port}", cli.api_url)),
                    )
                    .await?;
                    if resp.status().is_success() {
                        println!("Port forward removed: {host_port}");
                    } else {
                        eprintln!(
                            "Error: {}",
                            api_error(resp, &format!("port forward {host_port}")).await
                        );
                        std::process::exit(1);
                    }
                }
                PortForwardAction::List => {
                    let resp =
                        api_request(client.get(format!("{}/v1/vms/{name}/ports", cli.api_url)))
                            .await?;
                    if !resp.status().is_success() {
                        eprintln!("Error: {}", api_error(resp, &format!("VM '{name}'")).await);
                        std::process::exit(1);
                    }

                    let forwards: Vec<serde_json::Value> = resp.json().await?;
                    if forwards.is_empty() {
                        println!("No port forwards for {name}");
                    } else {
                        println!(
                            "{:<12} {:<12} {:<10}",
                            "HOST PORT", "GUEST PORT", "PROTOCOL"
                        );
                        for pf in forwards {
                            println!(
                                "{:<12} {:<12} {:<10}",
                                pf["host_port"],
                                pf["guest_port"],
                                pf["protocol"].as_str().unwrap_or("tcp"),
                            );
                        }
                    }
                }
            }
            Ok(())
        }
        Commands::Shell { name, command } => {
            run_shell(cli.api_url, cli.config, name, command).await
        }
    }
}

use husk_api::{WsShellInput, WsShellOutput};

type WsStream =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// Run an interactive shell session inside a VM.
///
/// On Linux, connects directly to the Firecracker vsock UDS proxy for lower
/// latency. Falls back to the WebSocket path if the vsock socket is missing.
/// On macOS, always uses the WebSocket path through the daemon.
#[cfg(feature = "linux-net")]
async fn run_shell(
    api_url: String,
    config_path: Option<PathBuf>,
    name: String,
    command: Option<String>,
) -> Result<()> {
    let client = reqwest::Client::new();
    let resp = api_request(client.get(format!("{}/v1/vms/{name}", api_url))).await?;

    if !resp.status().is_success() {
        eprintln!("Error: {}", api_error(resp, &format!("VM '{name}'")).await);
        std::process::exit(1);
    }

    let vm: serde_json::Value = resp.json().await?;
    let vm_id = vm["id"].as_str().context("missing VM id")?;

    let config = load_config(config_path.as_deref());
    let runtime_dir = config.data_dir.join("run");
    let vsock_path = runtime_dir.join(format!("{vm_id}.vsock"));

    if !std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        eprintln!("Error: `husk shell` requires an interactive terminal");
        std::process::exit(1);
    }

    // Try direct vsock first (lower latency), fall back to WebSocket.
    if vsock_path.exists() {
        let mut conn =
            husk_core::AgentClient::connect(&vsock_path, husk_agent_proto::AGENT_VSOCK_PORT)
                .await
                .context("connecting to agent")?;

        let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));

        conn.shell_start(command.as_deref(), cols, rows)
            .await
            .context("starting shell")?;

        crossterm::terminal::enable_raw_mode().context("enabling raw mode")?;

        let result = run_shell_bridge(&mut conn).await;

        crossterm::terminal::disable_raw_mode().ok();
        println!();

        match result {
            Ok(exit_code) => std::process::exit(exit_code),
            Err(e) => {
                eprintln!("Shell error: {e}");
                std::process::exit(1);
            }
        }
    }

    // Direct vsock unavailable — use WebSocket through daemon.
    run_shell_ws(&api_url, &name, command.as_deref()).await
}

#[cfg(not(feature = "linux-net"))]
async fn run_shell(
    api_url: String,
    _config_path: Option<PathBuf>,
    name: String,
    command: Option<String>,
) -> Result<()> {
    run_shell_ws(&api_url, &name, command.as_deref()).await
}

/// WebSocket-based interactive shell, works on both Linux and macOS.
async fn run_shell_ws(api_url: &str, name: &str, command: Option<&str>) -> Result<()> {
    // Pre-check: verify VM is running before opening the WebSocket.
    let client = reqwest::Client::new();
    let resp = api_request(client.get(format!("{api_url}/v1/vms/{name}"))).await?;
    if !resp.status().is_success() {
        eprintln!("Error: {}", api_error(resp, &format!("VM '{name}'")).await);
        std::process::exit(1);
    }
    let vm: serde_json::Value = resp.json().await?;
    let state = vm["state"].as_str().unwrap_or("unknown");
    if state != "running" {
        eprintln!("Error: VM '{name}' is {state}, expected running");
        if state == "stopped" {
            eprintln!("Hint: start the VM first with `husk run`");
        } else if state == "paused" {
            eprintln!("Hint: resume the VM first with `husk resume {name}`");
        }
        std::process::exit(1);
    }

    let ws_url = api_url
        .replacen("http://", "ws://", 1)
        .replacen("https://", "wss://", 1);
    let url = format!("{ws_url}/v1/vms/{name}/shell");

    let (ws_stream, _) = tokio_tungstenite::connect_async(&url)
        .await
        .context("connecting to daemon WebSocket")?;

    let (mut ws_sink, mut ws_recv) = ws_stream.split();

    let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));

    let start_msg = serde_json::to_string(&WsShellInput::Start {
        command: command.map(String::from),
        cols,
        rows,
    })?;
    ws_sink
        .send(tungstenite::Message::Text(start_msg.into()))
        .await
        .context("sending start message")?;

    // Wait for Started response.
    let started = ws_recv.next().await.context("no response from server")?;
    match started {
        Ok(tungstenite::Message::Text(text)) => {
            let msg: WsShellOutput =
                serde_json::from_str(&text).context("invalid server message")?;
            match msg {
                WsShellOutput::Started => {}
                WsShellOutput::Error { message } => {
                    eprintln!("Error: {message}");
                    std::process::exit(1);
                }
                _ => {
                    eprintln!("Error: unexpected response from server");
                    std::process::exit(1);
                }
            }
        }
        Ok(_) => anyhow::bail!("unexpected message type from server"),
        Err(e) => anyhow::bail!("WebSocket error: {e}"),
    }

    crossterm::terminal::enable_raw_mode().context("enabling raw mode")?;

    let result = run_shell_ws_bridge(&mut ws_sink, &mut ws_recv).await;

    crossterm::terminal::disable_raw_mode().ok();
    println!();

    // Exit immediately — tokio's stdin reader holds a blocking thread that
    // prevents clean runtime shutdown. process::exit() is the standard pattern
    // for interactive CLI tools that use raw stdin.
    match result {
        Ok(exit_code) => std::process::exit(exit_code),
        Err(e) => {
            eprintln!("Shell error: {e}");
            std::process::exit(1);
        }
    }
}

/// Bridge raw stdin/stdout to a WebSocket shell session.
///
/// Reads raw stdin bytes directly (preserving escape sequences as-is) and
/// detects terminal resizes via SIGWINCH. Handles SIGHUP for graceful shutdown.
async fn run_shell_ws_bridge(
    ws_sink: &mut futures_util::stream::SplitSink<WsStream, tungstenite::Message>,
    ws_recv: &mut futures_util::stream::SplitStream<WsStream>,
) -> Result<i32> {
    use tokio::signal::unix::{SignalKind, signal};

    let mut stdin = tokio::io::stdin();
    let mut stdin_buf = vec![0u8; 1024];
    let mut sigwinch = signal(SignalKind::window_change()).context("registering SIGWINCH")?;
    let mut sighup = signal(SignalKind::hangup()).context("registering SIGHUP")?;

    loop {
        tokio::select! {
            result = stdin.read(&mut stdin_buf) => {
                match result {
                    Ok(0) => return Ok(0),
                    Ok(n) => {
                        let encoded = husk_agent_proto::base64_encode(&stdin_buf[..n]);
                        let msg = serde_json::to_string(&WsShellInput::Data { data: encoded })?;
                        ws_sink.send(tungstenite::Message::Text(msg.into())).await?;
                    }
                    Err(e) => return Err(e.into()),
                }
            }
            _ = sigwinch.recv() => {
                if let Ok((cols, rows)) = crossterm::terminal::size() {
                    let msg = serde_json::to_string(&WsShellInput::Resize { cols, rows })?;
                    ws_sink.send(tungstenite::Message::Text(msg.into())).await?;
                }
            }
            _ = sighup.recv() => {
                let _ = ws_sink.send(tungstenite::Message::Close(None)).await;
                return Ok(0);
            }
            ws_msg = ws_recv.next() => {
                match ws_msg {
                    Some(Ok(tungstenite::Message::Text(text))) => {
                        let msg: WsShellOutput = serde_json::from_str(&text)?;
                        match msg {
                            WsShellOutput::Data { data } => {
                                let bytes = husk_agent_proto::base64_decode(&data)
                                    .map_err(|e| anyhow::anyhow!("base64 decode: {e}"))?;
                                use std::io::Write;
                                std::io::stdout().write_all(&bytes)?;
                                std::io::stdout().flush()?;
                            }
                            WsShellOutput::Exit { exit_code } => {
                                return Ok(exit_code);
                            }
                            WsShellOutput::Error { message } => {
                                return Err(anyhow::anyhow!("agent error: {message}"));
                            }
                            WsShellOutput::Started => {}
                        }
                    }
                    Some(Ok(tungstenite::Message::Close(_))) | None => return Ok(0),
                    Some(Ok(_)) => {}
                    Some(Err(e)) => return Err(anyhow::anyhow!("WebSocket error: {e}")),
                }
            }
        }
    }
}

enum CpPath {
    Local(PathBuf),
    Vm { name: String, path: String },
}

fn parse_octal_mode(s: &str) -> Result<u32, String> {
    u32::from_str_radix(s, 8).map_err(|e| format!("invalid octal mode: {e}"))
}

fn parse_cp_path(s: &str) -> CpPath {
    if let Some(colon_pos) = s.find(':') {
        let name = &s[..colon_pos];
        let path = &s[colon_pos + 1..];
        if !name.is_empty() && !path.is_empty() {
            return CpPath::Vm {
                name: name.to_string(),
                path: path.to_string(),
            };
        }
    }
    CpPath::Local(PathBuf::from(s))
}

#[cfg(feature = "linux-net")]
async fn run_shell_bridge<S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin>(
    conn: &mut husk_core::AgentConnection<S>,
) -> Result<i32> {
    use tokio::signal::unix::{SignalKind, signal};

    let mut stdin = tokio::io::stdin();
    let mut stdin_buf = vec![0u8; 1024];
    let mut sigwinch = signal(SignalKind::window_change()).context("registering SIGWINCH")?;
    let mut sighup = signal(SignalKind::hangup()).context("registering SIGHUP")?;

    loop {
        tokio::select! {
            result = stdin.read(&mut stdin_buf) => {
                match result {
                    Ok(0) => return Ok(0),
                    Ok(n) => {
                        conn.shell_send(&stdin_buf[..n]).await?;
                    }
                    Err(e) => return Err(e.into()),
                }
            }
            _ = sigwinch.recv() => {
                if let Ok((cols, rows)) = crossterm::terminal::size() {
                    conn.shell_resize(cols, rows).await?;
                }
            }
            _ = sighup.recv() => {
                return Ok(0);
            }
            event = conn.shell_recv() => {
                match event? {
                    husk_core::ShellEvent::Data(data) => {
                        use std::io::Write;
                        std::io::stdout().write_all(&data)?;
                        std::io::stdout().flush()?;
                    }
                    husk_core::ShellEvent::Exit(code) => {
                        return Ok(code);
                    }
                }
            }
        }
    }
}

/// Resolve the config file path by checking (in order):
/// 1. Explicit path from --config flag
/// 2. `~/.config/husk/config.toml` (XDG user config)
/// 3. `/etc/husk/config.toml` (system config)
fn resolve_config_path(explicit: Option<&Path>) -> PathBuf {
    if let Some(path) = explicit {
        return path.to_owned();
    }
    if let Some(home) = std::env::var_os("HOME") {
        let user_config = PathBuf::from(home).join(".config/husk/config.toml");
        if user_config.exists() {
            return user_config;
        }
    }
    PathBuf::from("/etc/husk/config.toml")
}

fn load_config(explicit_path: Option<&Path>) -> Config {
    let path = resolve_config_path(explicit_path);
    match std::fs::read_to_string(&path) {
        Ok(contents) => toml::from_str(&contents).unwrap_or_else(|e| {
            eprintln!("Warning: invalid config file: {e}");
            Config::default()
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Config::default(),
        Err(e) => {
            eprintln!(
                "Warning: could not read config file {}: {e}",
                path.display()
            );
            Config::default()
        }
    }
}

async fn start_daemon(config: Config, listen: SocketAddr) -> Result<()> {
    tracing::info!("starting husk daemon");

    let runtime_dir = config.data_dir.join("run");
    let db_path = config.data_dir.join("husk.db");

    std::fs::create_dir_all(&runtime_dir).context("creating runtime directory")?;
    std::fs::create_dir_all(config.data_dir.join("vms")).context("creating vms directory")?;

    let state = husk_state::StateStore::open(&db_path).context("opening state database")?;

    let stale_count = state
        .mark_stale_vms_stopped()
        .context("reconciling stale VM state")?;
    if stale_count > 0 {
        tracing::info!(stale_count, "marked stale VMs as stopped");
    }

    let storage = husk_storage::StorageConfig {
        data_dir: config.data_dir,
    };

    #[cfg(feature = "linux-net")]
    {
        let vmm =
            husk_vmm::firecracker::FirecrackerBackend::new(&config.firecracker_bin, &runtime_dir);
        let ip_allocator = husk_net::IpAllocator::new(std::net::Ipv4Addr::new(172, 20, 0, 0), 16);

        if let Err(e) = husk_net::init_nat().await {
            tracing::warn!("failed to initialize nftables: {e} (VM networking may not work)");
        }

        let core = Arc::new(husk_core::HuskCore::new(
            vmm,
            state,
            ip_allocator,
            storage,
            config.host_interface,
        ));
        husk_api::serve(core, listen).await?;
        Ok(())
    }

    #[cfg(not(feature = "linux-net"))]
    {
        let vmm = husk_vmm::apple_vz::AppleVzBackend::new();

        let core = Arc::new(husk_core::HuskCore::new(vmm, state, storage));
        husk_api::serve(core, listen).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_cp_path_local() {
        assert!(
            matches!(parse_cp_path("/tmp/file.txt"), CpPath::Local(p) if p == Path::new("/tmp/file.txt"))
        );
        assert!(
            matches!(parse_cp_path("relative.txt"), CpPath::Local(p) if p == Path::new("relative.txt"))
        );
        assert!(
            matches!(parse_cp_path("./dir/file"), CpPath::Local(p) if p == Path::new("./dir/file"))
        );
    }

    #[test]
    fn parse_cp_path_vm() {
        match parse_cp_path("myvm:/tmp/file.txt") {
            CpPath::Vm { name, path } => {
                assert_eq!(name, "myvm");
                assert_eq!(path, "/tmp/file.txt");
            }
            CpPath::Local(_) => panic!("expected Vm"),
        }
    }

    #[test]
    fn parse_cp_path_vm_relative_guest_path() {
        match parse_cp_path("myvm:relative/path") {
            CpPath::Vm { name, path } => {
                assert_eq!(name, "myvm");
                assert_eq!(path, "relative/path");
            }
            CpPath::Local(_) => panic!("expected Vm"),
        }
    }

    #[test]
    fn parse_cp_path_multiple_colons() {
        match parse_cp_path("myvm:/path:with:colons") {
            CpPath::Vm { name, path } => {
                assert_eq!(name, "myvm");
                assert_eq!(path, "/path:with:colons");
            }
            CpPath::Local(_) => panic!("expected Vm"),
        }
    }

    #[test]
    fn parse_cp_path_empty_name_is_local() {
        assert!(matches!(parse_cp_path(":/tmp/file"), CpPath::Local(_)));
    }

    #[test]
    fn parse_cp_path_empty_path_is_local() {
        assert!(matches!(parse_cp_path("vmname:"), CpPath::Local(_)));
    }

    #[test]
    fn octal_mode_parsing() {
        assert_eq!(parse_octal_mode("755").unwrap(), 0o755);
        assert_eq!(parse_octal_mode("644").unwrap(), 0o644);
        assert_eq!(parse_octal_mode("777").unwrap(), 0o777);
        assert_eq!(parse_octal_mode("400").unwrap(), 0o400);
    }

    #[test]
    fn octal_mode_invalid() {
        assert!(parse_octal_mode("999").is_err());
        assert!(parse_octal_mode("abc").is_err());
        assert!(parse_octal_mode("").is_err());
    }
}
