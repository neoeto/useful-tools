use clap::Parser;
use file_gen::Args;

fn main() {
    std::process::exit(file_gen::run(Args::parse()));
}
