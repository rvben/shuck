use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use tokio::io::AsyncReadExt;
use tokio_tungstenite::tungstenite;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

#[derive(Parser)]
#[command(name = "husk", about = "An open source microVM manager", version)]
struct Cli {
    /// Path to config file
    #[arg(long)]
    config: Option<PathBuf>,

    /// Daemon API address (for client commands)
    #[arg(long, default_value = "http://127.0.0.1:7777")]
    api_url: String,

    /// Bearer token for authenticated API access.
    #[arg(long)]
    api_token: Option<String>,

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
        /// Allow binding the daemon API to non-loopback addresses.
        ///
        /// By default husk refuses non-loopback binds to avoid accidental
        /// remote exposure of privileged VM control endpoints.
        #[arg(long)]
        allow_remote: bool,
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

        /// Environment variables for userdata script (KEY=VALUE)
        #[arg(long, short = 'e')]
        env: Vec<String>,
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

    /// Show serial console output from a VM
    Logs {
        /// VM name
        name: String,
        /// Follow log output (like tail -f)
        #[arg(long, short = 'f')]
        follow: bool,
        /// Show last N lines
        #[arg(long, short = 'n')]
        tail: Option<u64>,
    },

    /// Print version information (client and daemon)
    Version,

    /// Configuration management
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
}

#[derive(Subcommand)]
enum ConfigAction {
    /// Validate the configuration file
    Check,
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
    #[serde(default)]
    api_token: Option<String>,
    #[serde(default = "default_api_max_request_bytes")]
    api_max_request_bytes: usize,
    #[serde(default = "default_api_max_file_read_bytes")]
    api_max_file_read_bytes: usize,
    #[serde(default = "default_api_max_file_write_bytes")]
    api_max_file_write_bytes: usize,
    #[serde(default = "default_api_sensitive_rate_limit_per_minute")]
    api_sensitive_rate_limit_per_minute: u32,
    #[serde(default)]
    allowed_read_paths: Vec<String>,
    #[serde(default)]
    allowed_write_paths: Vec<String>,
    #[serde(default = "default_exec_timeout_secs")]
    exec_timeout_secs: u64,
    #[serde(default)]
    exec_allowlist: Vec<String>,
    #[serde(default)]
    exec_denylist: Vec<String>,
    #[serde(default)]
    exec_env_allowlist: Vec<String>,
    #[cfg(feature = "linux-net")]
    #[serde(default = "default_host_interface")]
    host_interface: String,
    #[cfg(feature = "linux-net")]
    #[serde(default = "default_bridge_name")]
    bridge_name: String,
    #[cfg(feature = "linux-net")]
    #[serde(default = "default_bridge_subnet")]
    bridge_subnet: String,
    #[cfg(feature = "linux-net")]
    #[serde(default = "default_dns_servers")]
    dns_servers: Vec<String>,
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

fn default_api_max_request_bytes() -> usize {
    2 * 1024 * 1024
}

fn default_api_max_file_read_bytes() -> usize {
    1024 * 1024
}

fn default_api_max_file_write_bytes() -> usize {
    1024 * 1024
}

fn default_api_sensitive_rate_limit_per_minute() -> u32 {
    120
}

fn default_exec_timeout_secs() -> u64 {
    30
}

#[cfg(feature = "linux-net")]
fn default_host_interface() -> String {
    "eth0".into()
}

#[cfg(feature = "linux-net")]
fn default_bridge_name() -> String {
    "husk0".into()
}

#[cfg(feature = "linux-net")]
fn default_bridge_subnet() -> String {
    "172.20.0.0/24".into()
}

#[cfg(feature = "linux-net")]
fn default_dns_servers() -> Vec<String> {
    vec!["8.8.8.8".into(), "1.1.1.1".into()]
}

/// Extract a clean error message from an API error response.
///
/// Handles JSON error bodies, plain text, and empty responses gracefully
/// so the CLI never dumps raw stack traces at the user.
async fn api_error(resp: reqwest::Response, subject: &str) -> String {
    let status = resp.status();
    match resp.text().await {
        Ok(body) if !body.is_empty() => {
            if let Ok(json) = serde_json::from_str::<serde_json::Value>(&body) {
                if let Some(msg) = json["message"].as_str() {
                    if let Some(hint) = json["hint"].as_str() {
                        return format!("{msg} (hint: {hint})");
                    }
                    return msg.to_string();
                }
                if let Some(msg) = json["error"].as_str() {
                    return msg.to_string();
                }
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

fn with_api_auth(
    request: reqwest::RequestBuilder,
    api_token: Option<&str>,
) -> reqwest::RequestBuilder {
    if let Some(token) = api_token {
        request.bearer_auth(token)
    } else {
        request
    }
}

fn resolve_api_token(cli_api_token: Option<String>, config_path: Option<&Path>) -> Option<String> {
    cli_api_token.or_else(|| load_config(config_path).api_token)
}

impl Default for Config {
    fn default() -> Self {
        Self {
            #[cfg(feature = "linux-net")]
            firecracker_bin: default_firecracker_bin(),
            data_dir: default_data_dir(),
            default_kernel: default_kernel_path(),
            api_token: None,
            api_max_request_bytes: default_api_max_request_bytes(),
            api_max_file_read_bytes: default_api_max_file_read_bytes(),
            api_max_file_write_bytes: default_api_max_file_write_bytes(),
            api_sensitive_rate_limit_per_minute: default_api_sensitive_rate_limit_per_minute(),
            allowed_read_paths: Vec::new(),
            allowed_write_paths: Vec::new(),
            exec_timeout_secs: default_exec_timeout_secs(),
            exec_allowlist: Vec::new(),
            exec_denylist: Vec::new(),
            exec_env_allowlist: Vec::new(),
            #[cfg(feature = "linux-net")]
            host_interface: default_host_interface(),
            #[cfg(feature = "linux-net")]
            bridge_name: default_bridge_name(),
            #[cfg(feature = "linux-net")]
            bridge_subnet: default_bridge_subnet(),
            #[cfg(feature = "linux-net")]
            dns_servers: default_dns_servers(),
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("husk=info".parse().expect("static directive")),
        )
        .init();

    let cli = Cli::parse();
    let Cli {
        config: config_path,
        api_url,
        api_token: cli_api_token,
        command,
    } = cli;

    match command {
        Commands::Daemon {
            listen,
            allow_remote,
        } => {
            validate_daemon_bind(listen, allow_remote)?;
            let mut config = load_config(config_path.as_deref());
            if let Some(token) = cli_api_token.clone() {
                config.api_token = Some(token);
            }
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
            env,
        } => {
            let config = load_config(config_path.as_deref());
            let api_token = cli_api_token.clone().or_else(|| config.api_token.clone());
            let kernel = kernel.unwrap_or(config.default_kernel);
            let name =
                name.unwrap_or_else(|| format!("vm-{}", &uuid::Uuid::new_v4().to_string()[..8]));

            let env_pairs: Vec<(String, String)> = env
                .iter()
                .filter_map(|s| {
                    let (k, v) = s.split_once('=')?;
                    Some((k.to_string(), v.to_string()))
                })
                .collect();

            let mut body = serde_json::json!({
                "name": name,
                "kernel_path": kernel,
                "rootfs_path": rootfs,
                "vcpu_count": cpus,
                "mem_size_mib": memory,
                "env": env_pairs,
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
            let resp = api_request(
                with_api_auth(
                    client.post(format!("{api_url}/v1/vms")),
                    api_token.as_deref(),
                )
                .json(&body),
            )
            .await?;

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
            let api_token = resolve_api_token(cli_api_token.clone(), config_path.as_deref());
            let client = reqwest::Client::new();
            let resp = api_request(with_api_auth(
                client.get(format!("{api_url}/v1/vms")),
                api_token.as_deref(),
            ))
            .await?;

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
            let api_token = resolve_api_token(cli_api_token.clone(), config_path.as_deref());
            let client = reqwest::Client::new();
            let resp = api_request(with_api_auth(
                client.get(format!("{api_url}/v1/vms/{name}")),
                api_token.as_deref(),
            ))
            .await?;

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
            let api_token = resolve_api_token(cli_api_token.clone(), config_path.as_deref());
            let client = reqwest::Client::new();
            let resp = api_request(with_api_auth(
                client.post(format!("{api_url}/v1/vms/{name}/stop")),
                api_token.as_deref(),
            ))
            .await?;

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
            let api_token = resolve_api_token(cli_api_token.clone(), config_path.as_deref());
            let client = reqwest::Client::new();
            let resp = api_request(with_api_auth(
                client.post(format!("{api_url}/v1/vms/{name}/pause")),
                api_token.as_deref(),
            ))
            .await?;

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
            let api_token = resolve_api_token(cli_api_token.clone(), config_path.as_deref());
            let client = reqwest::Client::new();
            let resp = api_request(with_api_auth(
                client.post(format!("{api_url}/v1/vms/{name}/resume")),
                api_token.as_deref(),
            ))
            .await?;

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
            let api_token = resolve_api_token(cli_api_token.clone(), config_path.as_deref());
            let client = reqwest::Client::new();
            let resp = api_request(with_api_auth(
                client.delete(format!("{api_url}/v1/vms/{name}")),
                api_token.as_deref(),
            ))
            .await?;

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
            let api_token = resolve_api_token(cli_api_token.clone(), config_path.as_deref());
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
                with_api_auth(
                    client.post(format!("{api_url}/v1/vms/{name}/exec")),
                    api_token.as_deref(),
                )
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
            let api_token = resolve_api_token(cli_api_token.clone(), config_path.as_deref());
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
                        with_api_auth(
                            client.post(format!("{api_url}/v1/vms/{name}/files/write")),
                            api_token.as_deref(),
                        )
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
                        with_api_auth(
                            client.post(format!("{api_url}/v1/vms/{name}/files/read")),
                            api_token.as_deref(),
                        )
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
            let api_token = resolve_api_token(cli_api_token.clone(), config_path.as_deref());
            port_forward(api_url, api_token, name, action).await
        }
        Commands::Logs { name, follow, tail } => {
            let api_token = resolve_api_token(cli_api_token.clone(), config_path.as_deref());
            let mut url = format!("{api_url}/v1/vms/{name}/logs");
            let mut params = Vec::new();
            if follow {
                params.push("follow=true".to_string());
            }
            if let Some(n) = tail {
                params.push(format!("tail={n}"));
            }
            if !params.is_empty() {
                url.push('?');
                url.push_str(&params.join("&"));
            }

            let client = reqwest::Client::new();
            let resp = api_request(with_api_auth(client.get(&url), api_token.as_deref())).await?;

            if !resp.status().is_success() {
                eprintln!("Error: {}", api_error(resp, &format!("VM '{name}'")).await);
                std::process::exit(1);
            }

            if follow {
                use tokio::io::AsyncWriteExt;
                let mut stream = resp.bytes_stream();
                let mut stdout = tokio::io::stdout();
                use futures_util::StreamExt;
                while let Some(chunk) = stream.next().await {
                    match chunk {
                        Ok(bytes) => {
                            stdout.write_all(&bytes).await?;
                            stdout.flush().await?;
                        }
                        Err(e) => {
                            eprintln!("Error reading stream: {e}");
                            std::process::exit(1);
                        }
                    }
                }
            } else {
                let body = resp.text().await?;
                print!("{body}");
            }
            Ok(())
        }
        Commands::Shell { name, command } => {
            let api_token = resolve_api_token(cli_api_token.clone(), config_path.as_deref());
            run_shell(api_url, config_path, name, command, api_token.as_deref()).await
        }
        Commands::Version => {
            println!("husk {}", env!("CARGO_PKG_VERSION"));

            let client = reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(2))
                .build()?;
            if let Ok(resp) = client.get(format!("{api_url}/v1/health")).send().await
                && resp.status().is_success()
                && let Ok(health) = resp.json::<serde_json::Value>().await
            {
                let version = health["version"].as_str().unwrap_or("unknown");
                let total = health["vms"]["total"].as_u64().unwrap_or(0);
                let running = health["vms"]["running"].as_u64().unwrap_or(0);
                println!("daemon {version} ({total} VMs, {running} running)");
            }
            Ok(())
        }
        Commands::Config { action } => match action {
            ConfigAction::Check => check_config(config_path.as_deref()),
        },
    }
}

fn validate_daemon_bind(listen: SocketAddr, allow_remote: bool) -> Result<()> {
    if !listen.ip().is_loopback() && !allow_remote {
        anyhow::bail!(
            "refusing to bind daemon to non-loopback address {listen} without \
             --allow-remote"
        );
    }
    Ok(())
}

#[cfg(not(feature = "linux-net"))]
async fn port_forward(
    _api_url: String,
    _api_token: Option<String>,
    _name: String,
    _action: PortForwardAction,
) -> Result<()> {
    eprintln!("Error: port forwarding is not supported on this platform");
    eprintln!();
    eprintln!("Port forwarding requires Linux with Firecracker and nftables.");
    eprintln!("On macOS, guests use shared NAT via Virtualization.framework");
    eprintln!("and can reach the host network, but inbound port mapping is");
    eprintln!("not available.");
    std::process::exit(1);
}

#[cfg(feature = "linux-net")]
async fn port_forward(
    api_url: String,
    api_token: Option<String>,
    name: String,
    action: PortForwardAction,
) -> Result<()> {
    let client = reqwest::Client::new();
    match action {
        PortForwardAction::Add {
            host_port,
            guest_port,
        } => {
            let resp = api_request(
                with_api_auth(
                    client.post(format!("{api_url}/v1/vms/{name}/ports")),
                    api_token.as_deref(),
                )
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
            let resp = api_request(with_api_auth(
                client.delete(format!("{api_url}/v1/vms/{name}/ports/{host_port}")),
                api_token.as_deref(),
            ))
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
            let resp = api_request(with_api_auth(
                client.get(format!("{api_url}/v1/vms/{name}/ports")),
                api_token.as_deref(),
            ))
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
    api_token: Option<&str>,
) -> Result<()> {
    let client = reqwest::Client::new();
    let resp = api_request(with_api_auth(
        client.get(format!("{api_url}/v1/vms/{name}")),
        api_token,
    ))
    .await?;

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
    run_shell_ws(&api_url, &name, command.as_deref(), api_token).await
}

#[cfg(not(feature = "linux-net"))]
async fn run_shell(
    api_url: String,
    _config_path: Option<PathBuf>,
    name: String,
    command: Option<String>,
    api_token: Option<&str>,
) -> Result<()> {
    run_shell_ws(&api_url, &name, command.as_deref(), api_token).await
}

/// WebSocket-based interactive shell, works on both Linux and macOS.
async fn run_shell_ws(
    api_url: &str,
    name: &str,
    command: Option<&str>,
    api_token: Option<&str>,
) -> Result<()> {
    // Pre-check: verify VM is running before opening the WebSocket.
    let client = reqwest::Client::new();
    let resp = api_request(with_api_auth(
        client.get(format!("{api_url}/v1/vms/{name}")),
        api_token,
    ))
    .await?;
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

    let mut ws_request = url
        .into_client_request()
        .context("building websocket request")?;
    if let Some(token) = api_token {
        let value = format!("Bearer {token}");
        let header = tungstenite::http::HeaderValue::from_str(&value)
            .context("invalid API token for websocket auth header")?;
        ws_request
            .headers_mut()
            .insert(tungstenite::http::header::AUTHORIZATION, header);
    }

    let (ws_stream, _) = tokio_tungstenite::connect_async(ws_request)
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

/// Apply environment variable overrides to the configuration.
///
/// Environment variables take precedence over file-based config.
fn apply_env_overrides(config: &mut Config) {
    if let Ok(val) = std::env::var("HUSK_DATA_DIR") {
        config.data_dir = PathBuf::from(val);
    }
    if let Ok(val) = std::env::var("HUSK_DEFAULT_KERNEL") {
        config.default_kernel = PathBuf::from(val);
    }
    if let Ok(val) = std::env::var("HUSK_API_TOKEN") {
        config.api_token = Some(val);
    }
    if let Ok(val) = std::env::var("HUSK_API_MAX_REQUEST_BYTES")
        && let Ok(parsed) = val.parse::<usize>()
    {
        config.api_max_request_bytes = parsed;
    }
    if let Ok(val) = std::env::var("HUSK_API_MAX_FILE_READ_BYTES")
        && let Ok(parsed) = val.parse::<usize>()
    {
        config.api_max_file_read_bytes = parsed;
    }
    if let Ok(val) = std::env::var("HUSK_API_MAX_FILE_WRITE_BYTES")
        && let Ok(parsed) = val.parse::<usize>()
    {
        config.api_max_file_write_bytes = parsed;
    }
    if let Ok(val) = std::env::var("HUSK_API_SENSITIVE_RATE_LIMIT_PER_MINUTE")
        && let Ok(parsed) = val.parse::<u32>()
    {
        config.api_sensitive_rate_limit_per_minute = parsed;
    }
    if let Ok(val) = std::env::var("HUSK_ALLOWED_READ_PATHS") {
        config.allowed_read_paths = val
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
    }
    if let Ok(val) = std::env::var("HUSK_ALLOWED_WRITE_PATHS") {
        config.allowed_write_paths = val
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
    }
    if let Ok(val) = std::env::var("HUSK_EXEC_TIMEOUT_SECS")
        && let Ok(parsed) = val.parse::<u64>()
    {
        config.exec_timeout_secs = parsed;
    }
    if let Ok(val) = std::env::var("HUSK_EXEC_ALLOWLIST") {
        config.exec_allowlist = val
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
    }
    if let Ok(val) = std::env::var("HUSK_EXEC_DENYLIST") {
        config.exec_denylist = val
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
    }
    if let Ok(val) = std::env::var("HUSK_EXEC_ENV_ALLOWLIST") {
        config.exec_env_allowlist = val
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
    }
    #[cfg(feature = "linux-net")]
    {
        if let Ok(val) = std::env::var("HUSK_FIRECRACKER_BIN") {
            config.firecracker_bin = PathBuf::from(val);
        }
        if let Ok(val) = std::env::var("HUSK_HOST_INTERFACE") {
            config.host_interface = val;
        }
        if let Ok(val) = std::env::var("HUSK_BRIDGE_NAME") {
            config.bridge_name = val;
        }
        if let Ok(val) = std::env::var("HUSK_BRIDGE_SUBNET") {
            config.bridge_subnet = val;
        }
        if let Ok(val) = std::env::var("HUSK_DNS_SERVERS") {
            config.dns_servers = val.split(',').map(|s| s.trim().to_string()).collect();
        }
    }
}

fn load_config(explicit_path: Option<&Path>) -> Config {
    let path = resolve_config_path(explicit_path);
    let mut config = match std::fs::read_to_string(&path) {
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
    };
    apply_env_overrides(&mut config);
    config
}

/// Parse a CIDR string (e.g. "172.20.0.0/24") into base address and prefix length.
///
/// Validates that:
/// - The string contains a `/` separator
/// - The base address is a valid IPv4 address
/// - The prefix length is between 1 and 30 (inclusive)
/// - The base address is network-aligned (host bits are zero)
#[cfg(feature = "linux-net")]
fn parse_cidr(cidr: &str) -> Result<(std::net::Ipv4Addr, u8)> {
    let (base_str, prefix_str) = cidr.split_once('/').context("invalid CIDR: missing '/'")?;
    let base: std::net::Ipv4Addr = base_str.parse().context("invalid CIDR base address")?;
    let prefix_len: u8 = prefix_str.parse().context("invalid CIDR prefix length")?;
    anyhow::ensure!(
        (1..=30).contains(&prefix_len),
        "prefix length must be 1..=30 (got {prefix_len})"
    );

    // Verify the base address has no host bits set (is a proper network address).
    let base_u32 = u32::from(base);
    let host_mask = (1u32 << (32 - prefix_len)) - 1;
    anyhow::ensure!(
        base_u32 & host_mask == 0,
        "base address {base} is not network-aligned for /{prefix_len} \
         (did you mean {}/{}?)",
        std::net::Ipv4Addr::from(base_u32 & !host_mask),
        prefix_len,
    );

    Ok((base, prefix_len))
}

/// Check if a binary name can be found in PATH.
#[cfg(feature = "linux-net")]
fn find_in_path(name: &Path) -> Option<PathBuf> {
    if name.is_absolute() {
        return name.is_file().then(|| name.to_path_buf());
    }
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Validate the configuration file and report results.
fn check_config(explicit_path: Option<&Path>) -> Result<()> {
    let path = resolve_config_path(explicit_path);
    let mut all_ok = true;

    let config = match std::fs::read_to_string(&path) {
        Ok(contents) => {
            println!("Config: {}", path.display());
            match toml::from_str::<Config>(&contents) {
                Ok(config) => config,
                Err(e) => {
                    println!("  parse .............. FAIL ({e})");
                    std::process::exit(1);
                }
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if explicit_path.is_some() {
                println!("Config: {} (not found)", path.display());
                println!("  config file .............. FAIL (not found)");
                std::process::exit(1);
            } else {
                println!("Config: (defaults, no config file found)");
                Config::default()
            }
        }
        Err(e) => {
            println!("Config: {}", path.display());
            println!("  config file .............. FAIL ({e})");
            std::process::exit(1);
        }
    };

    let dd_from_env = std::env::var("HUSK_DATA_DIR").is_ok();
    let kernel_from_env = std::env::var("HUSK_DEFAULT_KERNEL").is_ok();

    // data_dir
    let dd = &config.data_dir;
    let dd_env_hint = if dd_from_env {
        " (from HUSK_DATA_DIR)"
    } else {
        ""
    };
    if dd.exists() {
        println!("  data_dir ({}) ... OK{dd_env_hint}", dd.display());
    } else {
        match std::fs::create_dir_all(dd) {
            Ok(()) => {
                println!(
                    "  data_dir ({}) ... OK (created){dd_env_hint}",
                    dd.display()
                );
            }
            Err(e) => {
                println!("  data_dir ({}) ... FAIL ({e}){dd_env_hint}", dd.display());
                all_ok = false;
            }
        }
    }

    // default_kernel
    let kernel = &config.default_kernel;
    let kernel_env_hint = if kernel_from_env {
        " (from HUSK_DEFAULT_KERNEL)"
    } else {
        ""
    };
    if kernel.is_file() {
        println!(
            "  default_kernel ({}) ... OK{kernel_env_hint}",
            kernel.display()
        );
    } else if kernel.exists() {
        println!(
            "  default_kernel ({}) ... FAIL (not a regular file){kernel_env_hint}",
            kernel.display()
        );
        all_ok = false;
    } else {
        println!(
            "  default_kernel ({}) ... FAIL (not found){kernel_env_hint}",
            kernel.display()
        );
        all_ok = false;
    }

    #[cfg(feature = "linux-net")]
    {
        let fc_from_env = std::env::var("HUSK_FIRECRACKER_BIN").is_ok();
        let iface_from_env = std::env::var("HUSK_HOST_INTERFACE").is_ok();
        let subnet_from_env = std::env::var("HUSK_BRIDGE_SUBNET").is_ok();

        // firecracker_bin
        let fc = &config.firecracker_bin;
        let fc_env_hint = if fc_from_env {
            " (from HUSK_FIRECRACKER_BIN)"
        } else {
            ""
        };
        match find_in_path(fc) {
            Some(resolved) => {
                if fc.is_absolute() {
                    println!("  firecracker_bin ({}) ... OK{fc_env_hint}", fc.display());
                } else {
                    println!(
                        "  firecracker_bin ({}) ... OK ({}){fc_env_hint}",
                        fc.display(),
                        resolved.display()
                    );
                }
            }
            None => {
                println!(
                    "  firecracker_bin ({}) ... FAIL (not found){fc_env_hint}",
                    fc.display()
                );
                all_ok = false;
            }
        }

        // host_interface
        let iface = &config.host_interface;
        let iface_env_hint = if iface_from_env {
            " (from HUSK_HOST_INTERFACE)"
        } else {
            ""
        };
        let iface_path = PathBuf::from(format!("/sys/class/net/{iface}"));
        if iface_path.exists() {
            println!("  host_interface ({iface}) ... OK{iface_env_hint}");
        } else {
            println!("  host_interface ({iface}) ... FAIL (not found){iface_env_hint}");
            all_ok = false;
        }

        // bridge_subnet
        let subnet_env_hint = if subnet_from_env {
            " (from HUSK_BRIDGE_SUBNET)"
        } else {
            ""
        };
        match parse_cidr(&config.bridge_subnet) {
            Ok(_) => println!(
                "  bridge_subnet ({}) ... OK{subnet_env_hint}",
                config.bridge_subnet
            ),
            Err(e) => {
                println!(
                    "  bridge_subnet ({}) ... FAIL ({e}){subnet_env_hint}",
                    config.bridge_subnet
                );
                all_ok = false;
            }
        }
    }

    if all_ok {
        Ok(())
    } else {
        std::process::exit(1);
    }
}

async fn start_daemon(config: Config, listen: SocketAddr) -> Result<()> {
    tracing::info!("starting husk daemon");

    let runtime_dir = config.data_dir.join("run");
    let db_path = config.data_dir.join("husk.db");
    let api_token = config.api_token.clone();
    let api_policy = husk_api::ApiPolicy {
        max_request_bytes: config.api_max_request_bytes,
        max_file_read_bytes: config.api_max_file_read_bytes,
        max_file_write_bytes: config.api_max_file_write_bytes,
        sensitive_rate_limit_per_minute: config.api_sensitive_rate_limit_per_minute,
        allowed_read_paths: config.allowed_read_paths.clone(),
        allowed_write_paths: config.allowed_write_paths.clone(),
        exec_timeout_secs: config.exec_timeout_secs,
        exec_allowlist: config.exec_allowlist.clone(),
        exec_denylist: config.exec_denylist.clone(),
        exec_env_allowlist: config.exec_env_allowlist.clone(),
    };
    husk_api::set_policy(api_policy);

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

        let (base, prefix_len) = parse_cidr(&config.bridge_subnet)?;
        let ip_allocator = husk_net::IpAllocator::new(base, prefix_len);

        // Clean up any stale bridge from a previous run
        let _ = husk_net::delete_bridge(&config.bridge_name).await;

        husk_net::create_bridge(&config.bridge_name, ip_allocator.gateway(), prefix_len)
            .await
            .context("creating bridge")?;

        husk_net::init_nat(
            &config.bridge_name,
            &config.bridge_subnet,
            &config.host_interface,
        )
        .await
        .context("initializing nftables")?;

        let core = Arc::new(husk_core::HuskCore::new(
            vmm,
            state,
            ip_allocator,
            storage,
            config.bridge_name.clone(),
            config.dns_servers,
            runtime_dir.clone(),
        ));

        let restored = core.reconcile_port_forwards_from_state().await;
        if restored > 0 {
            tracing::info!(restored, "restored persisted port-forward nftables rules");
        }

        spawn_log_rotation(Arc::clone(&core));
        husk_api::serve_with_auth(Arc::clone(&core), listen, api_token.clone()).await?;
        drain_vms_on_shutdown(&core).await;

        // Network cleanup after VM drain. If the process is killed
        // (SIGKILL, panic, OOM), the stale bridge cleanup at startup above
        // handles the next launch.
        let _ = husk_net::cleanup_nat().await;
        let _ = husk_net::delete_bridge(&config.bridge_name).await;
        Ok(())
    }

    #[cfg(not(feature = "linux-net"))]
    {
        let vmm = husk_vmm::apple_vz::AppleVzBackend::new(&runtime_dir);

        let core = Arc::new(husk_core::HuskCore::new(
            vmm,
            state,
            storage,
            runtime_dir.clone(),
        ));

        spawn_log_rotation(Arc::clone(&core));
        husk_api::serve_with_auth(Arc::clone(&core), listen, api_token).await?;
        drain_vms_on_shutdown(&core).await;
        Ok(())
    }
}

/// Spawn a background task that rotates oversized serial logs every hour.
fn spawn_log_rotation<B: husk_vmm::VmmBackend + 'static>(core: Arc<husk_core::HuskCore<B>>) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(3600));
        interval.tick().await; // first tick fires immediately, skip it
        loop {
            interval.tick().await;
            let count = core.rotate_serial_logs().await;
            if count > 0 {
                tracing::info!(count, "rotated serial logs");
            }
        }
    });
}

/// Drain all running/paused VMs with a 30-second timeout.
async fn drain_vms_on_shutdown<B: husk_vmm::VmmBackend>(core: &husk_core::HuskCore<B>) {
    tracing::info!("shutting down, draining VMs");
    match tokio::time::timeout(std::time::Duration::from_secs(30), core.drain_vms()).await {
        Ok(count) => {
            if count > 0 {
                tracing::info!(count, "drained VMs on shutdown");
            }
        }
        Err(_) => {
            tracing::warn!("VM drain timed out after 30s");
        }
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

    #[test]
    fn daemon_bind_loopback_allowed_without_flag() {
        let listen: SocketAddr = "127.0.0.1:7777".parse().unwrap();
        assert!(validate_daemon_bind(listen, false).is_ok());
    }

    #[test]
    fn daemon_bind_non_loopback_requires_allow_remote() {
        let listen: SocketAddr = "0.0.0.0:7777".parse().unwrap();
        assert!(validate_daemon_bind(listen, false).is_err());
        assert!(validate_daemon_bind(listen, true).is_ok());
    }

    #[test]
    fn env_override_data_dir() {
        // SAFETY: test is single-threaded; no other threads read this env var.
        unsafe { std::env::set_var("HUSK_DATA_DIR", "/tmp/husk-env-test") };
        let config = load_config(None);
        assert_eq!(config.data_dir, PathBuf::from("/tmp/husk-env-test"));
        unsafe { std::env::remove_var("HUSK_DATA_DIR") };
    }

    #[test]
    fn env_override_default_kernel() {
        // SAFETY: test is single-threaded; no other threads read this env var.
        unsafe { std::env::set_var("HUSK_DEFAULT_KERNEL", "/tmp/custom-kernel") };
        let config = load_config(None);
        assert_eq!(config.default_kernel, PathBuf::from("/tmp/custom-kernel"));
        unsafe { std::env::remove_var("HUSK_DEFAULT_KERNEL") };
    }

    #[test]
    fn env_override_api_token() {
        // SAFETY: test is single-threaded; no other threads read this env var.
        unsafe { std::env::set_var("HUSK_API_TOKEN", "test-token") };
        let config = load_config(None);
        assert_eq!(config.api_token.as_deref(), Some("test-token"));
        unsafe { std::env::remove_var("HUSK_API_TOKEN") };
    }

    #[cfg(feature = "linux-net")]
    #[test]
    fn env_override_dns_servers_comma_separated() {
        // SAFETY: test is single-threaded; no other threads read this env var.
        unsafe { std::env::set_var("HUSK_DNS_SERVERS", "1.1.1.1, 8.8.4.4, 9.9.9.9") };
        let config = load_config(None);
        assert_eq!(config.dns_servers, vec!["1.1.1.1", "8.8.4.4", "9.9.9.9"]);
        unsafe { std::env::remove_var("HUSK_DNS_SERVERS") };
    }

    #[cfg(feature = "linux-net")]
    mod cidr_tests {
        use super::super::parse_cidr;
        use std::net::Ipv4Addr;

        #[test]
        fn valid_cidr() {
            let (base, prefix) = parse_cidr("172.20.0.0/24").unwrap();
            assert_eq!(base, Ipv4Addr::new(172, 20, 0, 0));
            assert_eq!(prefix, 24);
        }

        #[test]
        fn valid_cidr_slash_16() {
            let (base, prefix) = parse_cidr("10.0.0.0/16").unwrap();
            assert_eq!(base, Ipv4Addr::new(10, 0, 0, 0));
            assert_eq!(prefix, 16);
        }

        #[test]
        fn valid_cidr_slash_30() {
            let (base, prefix) = parse_cidr("10.0.0.0/30").unwrap();
            assert_eq!(base, Ipv4Addr::new(10, 0, 0, 0));
            assert_eq!(prefix, 30);
        }

        #[test]
        fn missing_slash() {
            let err = parse_cidr("172.20.0.0").unwrap_err();
            assert!(err.to_string().contains("missing '/'"));
        }

        #[test]
        fn invalid_base_address() {
            assert!(parse_cidr("not.an.ip/24").is_err());
        }

        #[test]
        fn invalid_prefix_not_number() {
            assert!(parse_cidr("172.20.0.0/abc").is_err());
        }

        #[test]
        fn prefix_too_large() {
            let err = parse_cidr("172.20.0.0/31").unwrap_err();
            assert!(err.to_string().contains("1..=30"));
        }

        #[test]
        fn prefix_zero() {
            let err = parse_cidr("0.0.0.0/0").unwrap_err();
            assert!(err.to_string().contains("1..=30"));
        }

        #[test]
        fn base_not_network_aligned() {
            let err = parse_cidr("172.20.0.5/24").unwrap_err();
            let msg = err.to_string();
            assert!(msg.contains("not network-aligned"), "got: {msg}");
            // Should suggest the correct network address
            assert!(msg.contains("172.20.0.0/24"), "got: {msg}");
        }

        #[test]
        fn base_not_aligned_slash_16() {
            let err = parse_cidr("10.0.1.0/16").unwrap_err();
            let msg = err.to_string();
            assert!(msg.contains("not network-aligned"), "got: {msg}");
            assert!(msg.contains("10.0.0.0/16"), "got: {msg}");
        }
    }
}
