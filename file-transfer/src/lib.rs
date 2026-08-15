mod client;
mod client_tui;
mod pathing;
mod protocol;
mod server;

use clap::{Args as ClapArgs, Parser, Subcommand};
use std::{io, path::PathBuf};

pub use client::{ClientArgs, Progress, ProgressState};
pub use server::ServerArgs;

/// Resumable TCP file transfer
#[derive(Parser, Debug)]
#[command(name = "file-transfer", version, about = "Resumable TCP file transfer")]
pub struct Args {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Share files from a directory
    Server(ServerArgs),
    /// Browse or download files from a server
    Client(ClientArgs),
}

pub async fn run(args: Args) -> io::Result<()> {
    match args.command {
        Command::Server(args) => server::run(args).await,
        Command::Client(args) => client::run(args).await,
    }
}

#[derive(ClapArgs, Debug, Clone)]
pub struct CommonConnectionArgs {
    /// File containing the shared access token
    #[arg(long)]
    pub token_file: Option<PathBuf>,

    /// Connection timeout in seconds
    #[arg(long, default_value_t = 10)]
    pub connect_timeout: u64,

    /// Idle I/O timeout in seconds
    #[arg(long, default_value_t = 60)]
    pub idle_timeout: u64,
}
