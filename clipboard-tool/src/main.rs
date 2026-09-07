use clap::Parser;

fn main() {
    let args = clipboard_tool::Args::parse();
    std::process::exit(clipboard_tool::run(args));
}
