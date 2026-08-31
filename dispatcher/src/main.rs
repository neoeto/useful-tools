use clap::{Parser, Subcommand};

mod completions;
mod tui;

/// Unified entry for useful tools
#[derive(Parser, Debug)]
#[command(name = "ut", version, about = "Unified useful tools")]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Generate or install shell tab completions
    Completions(completions::Args),

    /// Batch generate UUIDs
    #[command(name = "uuid-gen")]
    UuidGen(uuid_gen::Args),

    /// A simple file server that serves files from a directory
    #[command(name = "file-server")]
    FileServer(file_server::Args),

    /// Echo received HTTP requests as raw request text
    #[command(name = "http-echo")]
    HttpEcho(http_echo::Args),

    /// Start an HTTP, SSE, and WebSocket test server
    #[command(name = "network-server")]
    NetworkServer(network_server::Args),

    /// Compute file hashes (MD5, SHA-1, SHA-2, SHA-3, BLAKE2, BLAKE3)
    #[command(name = "file-hash")]
    FileHash(file_hash::Args),

    /// Transfer files over TCP with resumable downloads
    #[command(name = "file-transfer")]
    FileTransfer(file_transfer::Args),

    /// Encode and decode Base64 data
    #[command(name = "base64")]
    Base64(base64_tool::Args),
}

#[tokio::main]
async fn main() {
    match Cli::parse().command {
        Some(Commands::Completions(args)) => {
            if let Err(error) = completions::run(args) {
                eprintln!("ut completions: {error}");
                std::process::exit(1);
            }
        }
        Some(Commands::UuidGen(args)) => uuid_gen::run(args),
        Some(Commands::FileServer(args)) => file_server::run(args).await,
        Some(Commands::HttpEcho(args)) => http_echo::run(args).await,
        Some(Commands::NetworkServer(args)) => network_server::run(args).await,
        Some(Commands::FileHash(args)) => std::process::exit(file_hash::run(args)),
        Some(Commands::FileTransfer(args)) => {
            if let Err(error) = file_transfer::run(args).await {
                eprintln!("file-transfer: {error}");
                std::process::exit(1);
            }
        }
        Some(Commands::Base64(args)) => std::process::exit(base64_tool::run(args)),
        None => {
            if let Err(error) = tui::run().await {
                eprintln!("ut: failed to start TUI: {error}");
                std::process::exit(1);
            }
        }
    }
}
