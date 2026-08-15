use clap::Parser;

#[tokio::main]
async fn main() {
    let args = http_echo::Args::parse();
    http_echo::run(args).await;
}
