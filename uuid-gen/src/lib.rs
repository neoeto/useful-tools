use clap::Parser;
use uuid::Uuid;

/// Batch UUID generator
#[derive(Parser, Debug)]
#[command(name = "uuid-gen", version, about = "Batch generate UUIDs")]
pub struct Args {
    /// Number of UUIDs to generate
    #[arg(short = 'n', long, default_value_t = 1)]
    pub count: u32,

    /// Strip hyphens from the output
    #[arg(short = 's', long)]
    pub no_hyphens: bool,
}

pub fn run(args: Args) {
    for _ in 0..args.count {
        let id = Uuid::new_v4();
        if args.no_hyphens {
            println!("{}", id.simple());
        } else {
            println!("{}", id);
        }
    }
}
