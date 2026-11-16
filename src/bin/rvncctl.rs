use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};
use rustvnc_agent::client::{self, AudioOptions, PairOptions, PingOptions};
use rustvnc_agent::paths;

#[derive(Debug, Parser)]
#[command(name = "rvncctl", version, about = "RustVNC Agent control client (A1)")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Pair with an agent using a one-time pairing code.
    Pair(PairArgs),
    /// Connect with the paired client certificate and measure Ping/Pong RTT.
    Ping(PingArgs),
    /// Connect and count/decode Opus audio datagrams.
    Audio(AudioArgs),
}

#[derive(Debug, Args)]
struct PairArgs {
    #[arg(long, default_value = "127.0.0.1:8443")]
    addr: String,
    #[arg(long, help = "8-digit pairing code printed by the agent")]
    code: String,
    #[arg(long, default_value = "rvncctl")]
    name: String,
    #[arg(long, help = "Directory for ca.pem/client-cert.pem/client-key.pem")]
    state_dir: Option<PathBuf>,
    #[arg(long, help = "Fail if the server fingerprint does not match")]
    expect_fingerprint: Option<String>,
    #[arg(long, help = "TLS server name (defaults to the host part of --addr)")]
    server_name: Option<String>,
    #[arg(long, help = "Store the client key in a 0600 file instead of Keychain")]
    no_keychain: bool,
}

#[derive(Debug, Args)]
struct AudioArgs {
    #[arg(long, default_value = "127.0.0.1:8443")]
    addr: String,
    #[arg(long, default_value_t = 5)]
    seconds: u64,
    #[arg(long)]
    state_dir: Option<PathBuf>,
    #[arg(long, help = "TLS server name (defaults to the host part of --addr)")]
    server_name: Option<String>,
    #[arg(
        long,
        help = "Read the client key from a 0600 file instead of Keychain"
    )]
    no_keychain: bool,
}

#[derive(Debug, Args)]
struct PingArgs {
    #[arg(long, default_value = "127.0.0.1:8443")]
    addr: String,
    #[arg(long, default_value_t = 3)]
    count: u32,
    #[arg(long)]
    state_dir: Option<PathBuf>,
    #[arg(long, help = "TLS server name (defaults to the host part of --addr)")]
    server_name: Option<String>,
    #[arg(
        long,
        help = "Read the client key from a 0600 file instead of Keychain"
    )]
    no_keychain: bool,
    #[arg(long, help = "Send this text to the agent clipboard after connecting")]
    clipboard_text: Option<String>,
    #[arg(long, default_value_t = 0, help = "Delay between pings (milliseconds)")]
    interval_ms: u64,
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let result = match cli.command {
        Command::Pair(args) => {
            client::pair(PairOptions {
                addr: args.addr,
                code: args.code,
                name: args.name,
                state_dir: args.state_dir.unwrap_or_else(paths::client_dir),
                expect_fingerprint: args.expect_fingerprint,
                server_name: args.server_name,
                keychain: !args.no_keychain,
            })
            .await
        }
        Command::Audio(args) => {
            client::audio(AudioOptions {
                addr: args.addr,
                seconds: args.seconds,
                state_dir: args.state_dir.unwrap_or_else(paths::client_dir),
                server_name: args.server_name,
                keychain: !args.no_keychain,
            })
            .await
        }
        Command::Ping(args) => {
            client::ping(PingOptions {
                addr: args.addr,
                count: args.count,
                state_dir: args.state_dir.unwrap_or_else(paths::client_dir),
                server_name: args.server_name,
                keychain: !args.no_keychain,
                clipboard_text: args.clipboard_text,
                interval_ms: args.interval_ms,
            })
            .await
        }
    };

    if let Err(err) = result {
        eprintln!("error: {err:#}");
        std::process::exit(1);
    }
}
