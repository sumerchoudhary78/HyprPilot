//! `hyprpilot-mcp` binary entry. Spawned by an MCP host over stdio.

use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;
use tracing_subscriber::EnvFilter;

use hyprpilot_daemon::default_socket_path;
use hyprpilot_mcp::capability::{self, Profile};
use hyprpilot_mcp::server::Server;

#[derive(Parser, Debug)]
#[command(
    name = "hyprpilot-mcp",
    version,
    about = "HyprPilot MCP server (stdio)"
)]
struct Args {
    /// Capability profile name. Loads
    /// `$XDG_CONFIG_HOME/hyprpilot/profiles/<name>.toml`. If the file is
    /// missing, falls back to the built-in `default` (read + window +
    /// workspace + undo). Pass `--profile unrestricted` to allow everything
    /// (no built-in file required).
    #[arg(long, default_value = "default")]
    profile: String,

    /// Override the daemon socket path.
    #[arg(long, value_name = "PATH")]
    daemon_socket: Option<PathBuf>,

    /// Log filter (`RUST_LOG` style). Logs go to stderr only — stdout is
    /// reserved for the JSON-RPC stream.
    #[arg(long, default_value = "warn", value_name = "DIRECTIVE")]
    log: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(&args.log)),
        )
        .with_target(false)
        .with_writer(std::io::stderr)
        .init();

    let profile = resolve_profile(&args.profile);
    let daemon_socket = args.daemon_socket.unwrap_or_else(default_socket_path);
    tracing::info!(
        profile = %profile.name,
        daemon = %daemon_socket.display(),
        "starting MCP server"
    );

    let server = Server::new(profile, daemon_socket);
    server.run_stdio().await?;
    Ok(())
}

fn resolve_profile(name: &str) -> Profile {
    if name == "unrestricted" {
        return Profile::unrestricted();
    }
    match capability::load_named(name) {
        Ok(p) => p,
        Err(capability::LoadError::NotFound(path)) => {
            tracing::warn!(
                requested = name,
                expected = %path.display(),
                "profile file not found; falling back to built-in default"
            );
            Profile::default_safe()
        }
        Err(e) => {
            tracing::error!(error = %e, "failed to load profile; falling back to built-in default");
            Profile::default_safe()
        }
    }
}
