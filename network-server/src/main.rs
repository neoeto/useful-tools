use clap::Parser;

#[tokio::main]
async fn main() {
    let args = network_server::Args::parse();
    network_server::run(args).await;
}
