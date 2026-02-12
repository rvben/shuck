use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use serde::Deserialize;

#[derive(Parser)]
#[command(name = "husk", about = "An open source microVM manager", version)]
struct Cli {
    /// Path to config file
    #[arg(long, default_value = "/etc/husk/config.toml")]
    config: PathBuf,

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

        /// Number of vCPUs
        #[arg(long, default_value = "1")]
        cpus: u32,

        /// Memory in MiB
        #[arg(long, default_value = "128")]
        memory: u32,
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
}

#[derive(Debug, Deserialize)]
struct Config {
    #[serde(default = "default_firecracker_bin")]
    firecracker_bin: PathBuf,
    #[serde(default = "default_data_dir")]
    data_dir: PathBuf,
    #[serde(default = "default_kernel_path")]
    default_kernel: PathBuf,
    #[serde(default = "default_host_interface")]
    host_interface: String,
}

fn default_firecracker_bin() -> PathBuf {
    PathBuf::from("firecracker")
}

fn default_data_dir() -> PathBuf {
    PathBuf::from("/var/lib/husk")
}

fn default_kernel_path() -> PathBuf {
    PathBuf::from("/var/lib/husk/kernels/vmlinux")
}

fn default_host_interface() -> String {
    "eth0".into()
}

impl Default for Config {
    fn default() -> Self {
        Self {
            firecracker_bin: default_firecracker_bin(),
            data_dir: default_data_dir(),
            default_kernel: default_kernel_path(),
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
            let config = load_config(&cli.config);
            start_daemon(config, listen).await
        }
        Commands::Run {
            rootfs,
            name,
            kernel,
            cpus,
            memory,
        } => {
            let config = load_config(&cli.config);
            let kernel = kernel.unwrap_or(config.default_kernel);
            let name =
                name.unwrap_or_else(|| format!("vm-{}", &uuid::Uuid::new_v4().to_string()[..8]));

            let client = reqwest::Client::new();
            let resp = client
                .post(format!("{}/v1/vms", cli.api_url))
                .json(&serde_json::json!({
                    "name": name,
                    "kernel_path": kernel,
                    "rootfs_path": rootfs,
                    "vcpu_count": cpus,
                    "mem_size_mib": memory,
                }))
                .send()
                .await
                .context("connecting to daemon")?;

            if resp.status().is_success() {
                let vm: serde_json::Value = resp.json().await?;
                println!("Created VM: {}", vm["name"]);
                println!("  ID:    {}", vm["id"]);
                println!("  State: {}", vm["state"]);
                println!("  CPUs:  {}", vm["vcpu_count"]);
                println!("  RAM:   {} MiB", vm["mem_size_mib"]);
            } else {
                let err: serde_json::Value = resp.json().await?;
                eprintln!("Error: {}", err["error"]);
                std::process::exit(1);
            }
            Ok(())
        }
        Commands::List => {
            let client = reqwest::Client::new();
            let resp = client
                .get(format!("{}/v1/vms", cli.api_url))
                .send()
                .await
                .context("connecting to daemon")?;

            let vms: Vec<serde_json::Value> = resp.json().await?;
            if vms.is_empty() {
                println!("No VMs running");
            } else {
                println!(
                    "{:<20} {:<12} {:<6} {:<10} {:<16}",
                    "NAME", "STATE", "CPUS", "MEMORY", "GUEST IP"
                );
                for vm in vms {
                    println!(
                        "{:<20} {:<12} {:<6} {:<10} {:<16}",
                        vm["name"].as_str().unwrap_or("-"),
                        vm["state"].as_str().unwrap_or("-"),
                        vm["vcpu_count"],
                        format!("{} MiB", vm["mem_size_mib"]),
                        vm["guest_ip"].as_str().unwrap_or("-"),
                    );
                }
            }
            Ok(())
        }
        Commands::Info { name } => {
            let client = reqwest::Client::new();
            let resp = client
                .get(format!("{}/v1/vms/{name}", cli.api_url))
                .send()
                .await
                .context("connecting to daemon")?;

            if resp.status().is_success() {
                let vm: serde_json::Value = resp.json().await?;
                println!("Name:     {}", vm["name"]);
                println!("ID:       {}", vm["id"]);
                println!("State:    {}", vm["state"]);
                println!("vCPUs:    {}", vm["vcpu_count"]);
                println!("Memory:   {} MiB", vm["mem_size_mib"]);
                println!("CID:      {}", vm["vsock_cid"]);
                println!("Host IP:  {}", vm["host_ip"].as_str().unwrap_or("-"));
                println!("Guest IP: {}", vm["guest_ip"].as_str().unwrap_or("-"));
            } else {
                let err: serde_json::Value = resp.json().await?;
                eprintln!("Error: {}", err["error"]);
                std::process::exit(1);
            }
            Ok(())
        }
        Commands::Stop { name } => {
            let client = reqwest::Client::new();
            let resp = client
                .post(format!("{}/v1/vms/{name}/stop", cli.api_url))
                .send()
                .await
                .context("connecting to daemon")?;

            if resp.status().is_success() {
                println!("Stopped VM: {name}");
            } else {
                let err: serde_json::Value = resp.json().await?;
                eprintln!("Error: {}", err["error"]);
                std::process::exit(1);
            }
            Ok(())
        }
        Commands::Destroy { name } => {
            let client = reqwest::Client::new();
            let resp = client
                .delete(format!("{}/v1/vms/{name}", cli.api_url))
                .send()
                .await
                .context("connecting to daemon")?;

            if resp.status().is_success() {
                println!("Destroyed VM: {name}");
            } else {
                let err: serde_json::Value = resp.json().await?;
                eprintln!("Error: {}", err["error"]);
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
            let resp = client
                .post(format!("{}/v1/vms/{name}/exec", cli.api_url))
                .json(&body)
                .send()
                .await
                .context("connecting to daemon")?;

            if resp.status().is_success() {
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
            } else {
                let err: serde_json::Value = resp.json().await?;
                eprintln!("Error: {}", err["error"]);
                std::process::exit(1);
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
                    let resp = client
                        .post(format!("{}/v1/vms/{name}/files/write", cli.api_url))
                        .json(&body)
                        .send()
                        .await
                        .context("connecting to daemon")?;

                    if resp.status().is_success() {
                        let result: serde_json::Value = resp.json().await?;
                        let bytes = result["bytes_written"].as_u64().unwrap_or(0);
                        println!("{bytes} bytes copied to {name}:{path}");
                    } else {
                        let err: serde_json::Value = resp.json().await?;
                        eprintln!("Error: {}", err["error"]);
                        std::process::exit(1);
                    }
                }
                (CpPath::Vm { name, path }, CpPath::Local(local)) => {
                    let client = reqwest::Client::new();
                    let resp = client
                        .post(format!("{}/v1/vms/{name}/files/read", cli.api_url))
                        .json(&serde_json::json!({ "path": path }))
                        .send()
                        .await
                        .context("connecting to daemon")?;

                    if resp.status().is_success() {
                        let result: serde_json::Value = resp.json().await?;
                        let b64 = result["data"].as_str().unwrap_or("");
                        let data = husk_agent_proto::base64_decode(b64)
                            .map_err(|e| anyhow::anyhow!("invalid base64 from server: {e}"))?;
                        std::fs::write(&local, &data)
                            .with_context(|| format!("writing {}", local.display()))?;
                        println!("{} bytes copied from {name}:{path}", data.len());
                    } else {
                        let err: serde_json::Value = resp.json().await?;
                        eprintln!("Error: {}", err["error"]);
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

fn load_config(path: &Path) -> Config {
    match std::fs::read_to_string(path) {
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

    // Ensure directories exist
    std::fs::create_dir_all(&runtime_dir).context("creating runtime directory")?;
    std::fs::create_dir_all(config.data_dir.join("vms")).context("creating vms directory")?;

    // Initialize subsystems
    let vmm = husk_vmm::firecracker::FirecrackerBackend::new(&config.firecracker_bin, &runtime_dir);

    let state = husk_state::StateStore::open(&db_path).context("opening state database")?;

    let ip_allocator = husk_net::IpAllocator::new(std::net::Ipv4Addr::new(172, 20, 0, 0), 16);

    let storage = husk_storage::StorageConfig {
        data_dir: config.data_dir,
    };

    // Initialize nftables (non-fatal on macOS / non-root)
    if let Err(e) = husk_net::init_nat().await {
        tracing::warn!("failed to initialize nftables: {e} (VM networking may not work)");
    }

    let core = Arc::new(husk_core::HuskCore::new(
        vmm,
        state,
        ip_allocator,
        storage,
        runtime_dir.clone(),
        config.host_interface,
    ));

    husk_api::serve(core, listen).await?;
    Ok(())
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
