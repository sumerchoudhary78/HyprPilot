use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;
use tracing_subscriber::EnvFilter;

use hyprpilot_daemon::{server, socket};

#[derive(Parser, Debug)]
#[command(name = "hyprpilot-daemon", version, about = "HyprPilot RPC daemon")]
struct Args {
    /// Override the listening socket path. Default: $XDG_RUNTIME_DIR/hyprpilot.sock.
    #[arg(long, value_name = "PATH")]
    socket: Option<PathBuf>,

    /// Log filter directive (`RUST_LOG` style). Default: `info`.
    #[arg(long, default_value = "info", value_name = "DIRECTIVE")]
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
        .init();

    let path = args.socket.unwrap_or_else(socket::default_socket_path);
    server::run(path).await
}
