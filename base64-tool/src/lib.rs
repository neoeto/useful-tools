use base64::{
    engine::general_purpose::{
        GeneralPurpose, STANDARD, STANDARD_NO_PAD, URL_SAFE, URL_SAFE_NO_PAD,
    },
    Engine,
};
use clap::{Args as ClapArgs, Parser, Subcommand};
use std::{
    fs::File,
    io::{self, Read, Write},
    path::Path,
};

/// Encode and decode Base64 data.
#[derive(Parser, Debug)]
#[command(name = "base64", version, about = "Encode and decode Base64 data")]
pub struct Args {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Encode data as Base64
    Encode(EncodeArgs),
    /// Decode Base64 data
    Decode(DecodeArgs),
}

#[derive(ClapArgs, Debug)]
pub struct EncodeArgs {
    #[command(flatten)]
    pub input: InputArgs,

    /// Wrap encoded output at this many columns; use 0 to disable wrapping
    #[arg(short = 'w', long, default_value_t = 76)]
    pub wrap: usize,
}

#[derive(ClapArgs, Debug)]
pub struct DecodeArgs {
    #[command(flatten)]
    pub input: InputArgs,
}

#[derive(ClapArgs, Debug)]
pub struct InputArgs {
    /// Input file, or - to read from stdin; stdin is used when omitted
    #[arg(value_name = "FILE", conflicts_with = "text")]
    pub file: Option<String>,

    /// Use literal text instead of reading a file or stdin
    #[arg(short = 't', long, conflicts_with = "file")]
    pub text: Option<String>,

    /// Use the URL-safe Base64 alphabet (- and _)
    #[arg(short = 'u', long)]
    pub url_safe: bool,

    /// Omit padding when encoding and accept unpadded input when decoding
    #[arg(long)]
    pub no_padding: bool,
}

pub fn run(args: Args) -> i32 {
    let result = match args.command {
        Command::Encode(args) => encode(args),
        Command::Decode(args) => decode(args),
    };

    match result {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("base64: {error}");
            1
        }
    }
}

fn encode(args: EncodeArgs) -> io::Result<()> {
    let input = read_input(&args.input)?;
    let encoded = engine(args.input.url_safe, args.input.no_padding).encode(input);
    let mut stdout = io::BufWriter::new(io::stdout().lock());

    if args.wrap == 0 {
        stdout.write_all(encoded.as_bytes())?;
    } else {
        for chunk in encoded.as_bytes().chunks(args.wrap) {
            stdout.write_all(chunk)?;
            stdout.write_all(b"\n")?;
        }
    }
    stdout.flush()
}

fn decode(args: DecodeArgs) -> io::Result<()> {
    let input = read_input(&args.input)?;
    let compacted: Vec<u8> = input
        .into_iter()
        .filter(|byte| !byte.is_ascii_whitespace())
        .collect();
    let decoded = engine(args.input.url_safe, args.input.no_padding)
        .decode(compacted)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    io::stdout().lock().write_all(&decoded)
}

fn read_input(args: &InputArgs) -> io::Result<Vec<u8>> {
    if let Some(text) = &args.text {
        return Ok(text.as_bytes().to_vec());
    }

    let mut input = Vec::new();
    match args.file.as_deref() {
        None | Some("-") => io::stdin().lock().read_to_end(&mut input)?,
        Some(path) => File::open(Path::new(path))?.read_to_end(&mut input)?,
    };
    Ok(input)
}

fn engine(url_safe: bool, no_padding: bool) -> &'static GeneralPurpose {
    match (url_safe, no_padding) {
        (false, false) => &STANDARD,
        (false, true) => &STANDARD_NO_PAD,
        (true, false) => &URL_SAFE,
        (true, true) => &URL_SAFE_NO_PAD,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_standard_and_url_safe_variants() {
        assert_eq!(engine(false, false).encode(b"hello"), "aGVsbG8=");
        assert_eq!(engine(false, true).encode(b"hello"), "aGVsbG8");
        assert_eq!(engine(true, false).encode([0xfb, 0xff]), "-_8=");
        assert_eq!(engine(true, true).encode([0xfb, 0xff]), "-_8");
    }

    #[test]
    fn decodes_wrapped_input() {
        let input = b"aGVs\n bG8=";
        let compacted: Vec<u8> = input
            .iter()
            .copied()
            .filter(|byte| !byte.is_ascii_whitespace())
            .collect();
        assert_eq!(engine(false, false).decode(compacted).unwrap(), b"hello");
    }
}
