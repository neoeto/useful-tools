use clap::Parser;

#[tokio::main]
async fn main() {
    if let Err(error) = file_transfer::run(file_transfer::Args::parse()).await {
        eprintln!("file-transfer: {error}");
        std::process::exit(1);
    }
}
