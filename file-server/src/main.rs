use clap::Parser;

#[tokio::main]
async fn main() {
    let args = file_server::Args::parse();
    file_server::run(args).await;
}
