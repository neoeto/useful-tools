use clap::Parser;

fn main() {
    let args = uuid_gen::Args::parse();
    uuid_gen::run(args);
}
