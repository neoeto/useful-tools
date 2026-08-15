use clap::Parser;

fn main() {
    let args = file_hash::Args::parse();
    std::process::exit(file_hash::run(args));
}
