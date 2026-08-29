use clap::Parser;

fn main() {
    let args = base64_tool::Args::parse();
    std::process::exit(base64_tool::run(args));
}
