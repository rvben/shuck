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
    }
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
