use clap::{Parser, ValueEnum};
use digest::{Digest, DynDigest};
use std::{
    fs::File,
    io::{self, BufRead, BufReader, Read},
    path::Path,
};

/// Hash algorithm to use
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum Algorithm {
    /// MD5 (128-bit)
    Md5,
    /// SHA-1 (160-bit)
    Sha1,
    /// SHA-2 224-bit
    Sha224,
    /// SHA-2 256-bit
    Sha256,
    /// SHA-2 384-bit
    Sha384,
    /// SHA-2 512-bit
    Sha512,
    /// SHA-3 224-bit
    Sha3_224,
    /// SHA-3 256-bit
    Sha3_256,
    /// SHA-3 384-bit
    Sha3_384,
    /// SHA-3 512-bit
    Sha3_512,
    /// BLAKE2b-512
    Blake2b,
    /// BLAKE2s-256
    Blake2s,
    /// BLAKE3 (256-bit)
    Blake3,
}

/// Compute file hashes (MD5, SHA-1, SHA-2, SHA-3, BLAKE2, BLAKE3)
#[derive(Parser, Debug)]
#[command(
    name = "file-hash",
    version,
    about = "Compute file hashes (MD5, SHA-1, SHA-2, SHA-3, BLAKE2, BLAKE3)"
)]
pub struct Args {
    /// Hash algorithm
    #[arg(short, long, value_enum, default_value = "sha256")]
    pub algorithm: Algorithm,

    /// Files to hash; pass "-" to read from stdin (stdin is used when omitted)
    #[arg(value_name = "FILE")]
    pub files: Vec<String>,

    /// Print uppercase hex digits
    #[arg(short, long)]
    pub uppercase: bool,

    /// Verify files against the given checksum files instead of hashing
    #[arg(short, long)]
    pub check: bool,

    /// Check mode: print only failures; normal mode: print hashes only
    #[arg(short, long)]
    pub quiet: bool,
}

/// A hasher for any supported algorithm.
///
/// BLAKE3 has its own API, so it is held separately from the RustCrypto
/// `DynDigest`-based algorithms.
enum Hasher {
    Digest(Box<dyn DynDigest>),
    Blake3(Box<blake3::Hasher>),
}

impl Hasher {
    fn new(algorithm: Algorithm) -> Self {
        match algorithm {
            Algorithm::Blake3 => Self::Blake3(Box::new(blake3::Hasher::new())),
            other => Self::Digest(other.digest()),
        }
    }

    fn update(&mut self, data: &[u8]) {
        match self {
            Self::Digest(hasher) => hasher.update(data),
            Self::Blake3(hasher) => {
                let _ = hasher.update(data);
            }
        }
    }

    fn finalize(self) -> Vec<u8> {
        match self {
            Self::Digest(hasher) => hasher.finalize().to_vec(),
            Self::Blake3(hasher) => hasher.finalize().as_bytes().to_vec(),
        }
    }
}

impl Algorithm {
    fn digest(self) -> Box<dyn DynDigest> {
        match self {
            Algorithm::Md5 => Box::new(md5::Md5::new()),
            Algorithm::Sha1 => Box::new(sha1::Sha1::new()),
            Algorithm::Sha224 => Box::new(sha2::Sha224::new()),
            Algorithm::Sha256 => Box::new(sha2::Sha256::new()),
            Algorithm::Sha384 => Box::new(sha2::Sha384::new()),
            Algorithm::Sha512 => Box::new(sha2::Sha512::new()),
            Algorithm::Sha3_224 => Box::new(sha3::Sha3_224::new()),
            Algorithm::Sha3_256 => Box::new(sha3::Sha3_256::new()),
            Algorithm::Sha3_384 => Box::new(sha3::Sha3_384::new()),
            Algorithm::Sha3_512 => Box::new(sha3::Sha3_512::new()),
            Algorithm::Blake2b => Box::new(blake2::Blake2b512::new()),
            Algorithm::Blake2s => Box::new(blake2::Blake2s256::new()),
            Algorithm::Blake3 => unreachable!("BLAKE3 is not a DynDigest"),
        }
    }
}

pub fn run(args: Args) -> i32 {
    if args.check {
        run_check(&args)
    } else {
        run_hash(&args)
    }
}

fn to_hex(bytes: &[u8], uppercase: bool) -> String {
    const LOWER: &[u8; 16] = b"0123456789abcdef";
    const UPPER: &[u8; 16] = b"0123456789ABCDEF";
    let table = if uppercase { UPPER } else { LOWER };

    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(table[(byte >> 4) as usize] as char);
        out.push(table[(byte & 0x0f) as usize] as char);
    }
    out
}

fn hash_reader(mut reader: impl Read, algorithm: Algorithm) -> io::Result<Vec<u8>> {
    let mut hasher = Hasher::new(algorithm);
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize())
}

fn hash_path(path: &Path, algorithm: Algorithm) -> io::Result<Vec<u8>> {
    if path.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "is a directory",
        ));
    }
    let file = File::open(path)?;
    hash_reader(BufReader::with_capacity(1 << 20, file), algorithm)
}

fn run_hash(args: &Args) -> i32 {
    let names: Vec<&str> = if args.files.is_empty() {
        vec!["-"]
    } else {
        args.files.iter().map(String::as_str).collect()
    };

    let mut exit_code = 0;
    let mut stdin_hash: Option<String> = None;

    for name in names {
        let digest = if name == "-" {
            // Hash stdin once even when "-" is passed multiple times.
            if let Some(hash) = &stdin_hash {
                Some(hash.clone())
            } else {
                let hash = match hash_reader(io::stdin().lock(), args.algorithm) {
                    Ok(bytes) => to_hex(&bytes, args.uppercase),
                    Err(error) => {
                        eprintln!("file-hash: -: {error}");
                        exit_code = 1;
                        String::new()
                    }
                };
                stdin_hash = Some(hash.clone());
                Some(hash)
            }
        } else {
            match hash_path(Path::new(name), args.algorithm) {
                Ok(bytes) => Some(to_hex(&bytes, args.uppercase)),
                Err(error) => {
                    eprintln!("file-hash: {name}: {error}");
                    exit_code = 1;
                    None
                }
            }
        };

        if let Some(digest) = digest {
            if args.quiet {
                println!("{digest}");
            } else {
                println!("{digest}  {name}");
            }
        }
    }

    exit_code
}

#[derive(Debug)]
struct ParsedEntry {
    name: String,
    expected: String,
    algorithm: Option<Algorithm>,
}

#[derive(Debug)]
enum ParsedLine {
    Blank,
    Entry(ParsedEntry),
    Malformed(String),
}

/// Parse one line of a checksum file in either GNU or BSD format.
fn parse_check_line(line: &str) -> ParsedLine {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return ParsedLine::Blank;
    }

    if let Some(entry) = parse_bsd_line(trimmed) {
        return ParsedLine::Entry(entry);
    }

    if let Some(entry) = parse_gnu_line(trimmed) {
        return ParsedLine::Entry(entry);
    }

    ParsedLine::Malformed(trimmed.to_string())
}

/// BSD format: `SHA256 (file name) = hex` (also tolerates `SHA256(file)=hex`).
fn parse_bsd_line(line: &str) -> Option<ParsedEntry> {
    let (algorithm_name, rest) = line.split_once('(')?;
    let algorithm_name = algorithm_name.trim_end();
    if !is_algorithm_name(algorithm_name) {
        return None;
    }
    let (name, after) = rest.split_once(')')?;
    let expected = after.trim_start().strip_prefix('=')?.trim();
    if !is_plausible_hex(expected) {
        return None;
    }
    Some(ParsedEntry {
        name: name.to_string(),
        expected: expected.to_string(),
        algorithm: Algorithm::from_str(algorithm_name, true).ok(),
    })
}

/// GNU format: `hex  name` (optionally `hex *name` for binary mode,
/// with a leading backslash escaping the file name).
fn parse_gnu_line(line: &str) -> Option<ParsedEntry> {
    let mut parts = line.splitn(2, char::is_whitespace);
    let expected = parts.next()?.trim();
    let mut name = parts.next()?.trim_start();
    if expected.is_empty() || name.is_empty() || !is_plausible_hex(expected) {
        return None;
    }
    if let Some(stripped) = name.strip_prefix('*') {
        name = stripped.trim_start();
    }
    if let Some(stripped) = name.strip_prefix('\\') {
        name = stripped;
    }
    Some(ParsedEntry {
        name: name.to_string(),
        expected: expected.to_string(),
        algorithm: None,
    })
}

fn is_algorithm_name(name: &str) -> bool {
    !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

/// A checksum hex string must have the length of one of the supported
/// algorithms (16/20/28/32/48/64 bytes) and consist of hex digits only.
fn is_plausible_hex(s: &str) -> bool {
    matches!(s.len(), 32 | 40 | 56 | 64 | 96 | 128) && s.chars().all(|c| c.is_ascii_hexdigit())
}

fn verify_entry(name: &str, expected: &str, algorithm: Algorithm, uppercase: bool) -> bool {
    match hash_path(Path::new(name), algorithm) {
        Ok(bytes) => {
            let actual = to_hex(&bytes, uppercase);
            if actual.eq_ignore_ascii_case(expected) {
                true
            } else {
                eprintln!("{name}: FAILED (hash mismatch)");
                false
            }
        }
        Err(error) => {
            eprintln!("{name}: FAILED ({error})");
            false
        }
    }
}

fn run_check(args: &Args) -> i32 {
    let sources: Vec<&str> = if args.files.is_empty() {
        vec!["-"]
    } else {
        args.files.iter().map(String::as_str).collect()
    };

    let mut failed = 0usize;
    let mut checked = 0usize;
    let mut malformed = false;

    for source in sources {
        let reader: Box<dyn BufRead> = if source == "-" {
            Box::new(io::stdin().lock())
        } else {
            match File::open(source) {
                Ok(file) => Box::new(BufReader::new(file)),
                Err(error) => {
                    eprintln!("file-hash: {source}: {error}");
                    failed += 1;
                    continue;
                }
            }
        };

        for line in reader.lines() {
            let line = match line {
                Ok(line) => line,
                Err(error) => {
                    eprintln!("file-hash: {source}: {error}");
                    failed += 1;
                    continue;
                }
            };

            match parse_check_line(&line) {
                ParsedLine::Blank => {}
                ParsedLine::Malformed(text) => {
                    eprintln!("file-hash: {source}: improperly formatted checksum line: {text}");
                    malformed = true;
                }
                ParsedLine::Entry(entry) => {
                    checked += 1;
                    let ParsedEntry {
                        name,
                        expected,
                        algorithm,
                    } = entry;
                    let algorithm = match algorithm.or(Some(args.algorithm)) {
                        Some(algorithm) => algorithm,
                        None => {
                            eprintln!("{name}: FAILED (unknown algorithm)");
                            failed += 1;
                            continue;
                        }
                    };
                    if verify_entry(&name, &expected, algorithm, args.uppercase) {
                        if !args.quiet {
                            println!("{name}: OK");
                        }
                    } else {
                        failed += 1;
                    }
                }
            }
        }
    }

    if checked == 0 {
        eprintln!("file-hash: no properly formatted checksum lines found");
        return 1;
    }

    if failed > 0 || malformed {
        1
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn digest_hex(algorithm: Algorithm, data: &[u8]) -> String {
        let mut hasher = Hasher::new(algorithm);
        hasher.update(data);
        to_hex(&hasher.finalize(), false)
    }

    #[test]
    fn known_answer_vectors() {
        assert_eq!(
            digest_hex(Algorithm::Md5, b"abc"),
            "900150983cd24fb0d6963f7d28e17f72"
        );
        assert_eq!(
            digest_hex(Algorithm::Sha1, b"abc"),
            "a9993e364706816aba3e25717850c26c9cd0d89d"
        );
        assert_eq!(
            digest_hex(Algorithm::Sha256, b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            digest_hex(Algorithm::Sha256, b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            digest_hex(Algorithm::Sha512, b"abc"),
            "ddaf35a193617abacc417349ae20413112e6fa4e89a97ea20a9eeee64b55d39a\
             2192992a274fc1a836ba3c23a3feebbd454d4423643ce80e2a9ac94fa54ca49f"
        );
        assert_eq!(
            digest_hex(Algorithm::Sha3_256, b"abc"),
            "3a985da74fe225b2045c172d6bd390bd855f086e3e9d525b46bfe24511431532"
        );
        assert_eq!(
            digest_hex(Algorithm::Blake3, b""),
            "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262"
        );
    }

    #[test]
    fn uppercase_hex() {
        assert_eq!(to_hex(&[0xab, 0x0f], true), "AB0F");
        assert_eq!(to_hex(&[0xab, 0x0f], false), "ab0f");
    }

    #[test]
    fn parses_gnu_lines() {
        let hash = "d41d8cd98f00b204e9800998ecf8427e";
        match parse_check_line(&format!("{hash}  empty file.txt")) {
            ParsedLine::Entry(entry) => {
                assert_eq!(entry.name, "empty file.txt");
                assert_eq!(entry.expected, hash);
                assert_eq!(entry.algorithm, None);
            }
            other => panic!("expected GNU entry, got {other:?}"),
        }

        let hash = "0000000000000000000000000000000000000000000000000000000000000000";
        match parse_check_line(&format!("{hash} *binary.bin")) {
            ParsedLine::Entry(entry) => assert_eq!(entry.name, "binary.bin"),
            other => panic!("expected GNU binary entry, got {other:?}"),
        }

        match parse_check_line(&format!("{hash} \\\\escaped")) {
            ParsedLine::Entry(entry) => assert_eq!(entry.name, "\\escaped"),
            other => panic!("expected GNU escaped entry, got {other:?}"),
        }
    }

    #[test]
    fn parses_bsd_lines() {
        let hash = "0000000000000000000000000000000000000000000000000000000000000000";
        match parse_check_line(&format!("SHA256 (some file.txt) = {hash}")) {
            ParsedLine::Entry(entry) => {
                assert_eq!(entry.name, "some file.txt");
                assert_eq!(entry.algorithm, Some(Algorithm::Sha256));
            }
            other => panic!("expected BSD entry, got {other:?}"),
        }

        let hash = "d41d8cd98f00b204e9800998ecf8427e";
        match parse_check_line(&format!("MD5(a)={hash}")) {
            ParsedLine::Entry(entry) => {
                assert_eq!(entry.name, "a");
                assert_eq!(entry.algorithm, Some(Algorithm::Md5));
            }
            other => panic!("expected BSD entry, got {other:?}"),
        }
    }

    #[test]
    fn rejects_malformed_lines() {
        assert!(matches!(parse_check_line("   "), ParsedLine::Blank));
        assert!(matches!(
            parse_check_line("not a checksum line"),
            ParsedLine::Malformed(_)
        ));
        // Unknown algorithm with a non-hex checksum is not a valid line.
        assert!(matches!(
            parse_check_line("CRC32 (file) = 1234"),
            ParsedLine::Malformed(_)
        ));
    }
}
