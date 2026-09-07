use clap::Parser;
use std::{
    fs::File,
    io::{self, Read},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
};

/// Copy file contents, or standard input when no file is supplied.
#[derive(Parser, Debug)]
#[command(
    name = "clipboard",
    version,
    about = "Copy file or standard input contents to the system clipboard"
)]
pub struct Args {
    /// Input file, or - to read from stdin; stdin is used when omitted
    #[arg(value_name = "FILE")]
    pub file: Option<PathBuf>,
}

#[derive(Clone, Copy)]
struct ClipboardBackend {
    name: &'static str,
    program: &'static str,
    args: &'static [&'static str],
}

struct CopySummary {
    source: String,
    bytes: u64,
    backend: &'static str,
}

#[cfg(target_os = "macos")]
const CLIPBOARD_BACKENDS: &[ClipboardBackend] = &[ClipboardBackend {
    name: "pbcopy",
    program: "pbcopy",
    args: &[],
}];

#[cfg(target_os = "windows")]
const CLIPBOARD_BACKENDS: &[ClipboardBackend] = &[ClipboardBackend {
    name: "clip",
    program: "clip",
    args: &[],
}];

#[cfg(all(unix, not(target_os = "macos")))]
const CLIPBOARD_BACKENDS: &[ClipboardBackend] = &[
    ClipboardBackend {
        name: "wl-copy",
        program: "wl-copy",
        args: &[],
    },
    ClipboardBackend {
        name: "xclip",
        program: "xclip",
        args: &["-selection", "clipboard"],
    },
    ClipboardBackend {
        name: "xsel",
        program: "xsel",
        args: &["--clipboard", "--input"],
    },
];

#[cfg(not(any(unix, target_os = "windows")))]
const CLIPBOARD_BACKENDS: &[ClipboardBackend] = &[];

pub fn run(args: Args) -> i32 {
    match copy_to_clipboard(args) {
        Ok(summary) => {
            println!(
                "Copied successfully: {} -> system clipboard ({}; {}; backend: {})",
                summary.source,
                format_bytes(summary.bytes),
                format_number(summary.bytes, "byte", "bytes"),
                summary.backend,
            );
            0
        }
        Err(error) => {
            eprintln!("clipboard: {error}");
            1
        }
    }
}

fn copy_to_clipboard(args: Args) -> io::Result<CopySummary> {
    let (mut input, source) = open_input(args.file.as_deref())?;
    let (mut child, backend) = start_clipboard()?;

    let copy_result = {
        let stdin = child.stdin.as_mut().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::BrokenPipe,
                format!("{} did not accept input", backend.name),
            )
        })?;
        io::copy(input.as_mut(), stdin)
    };

    drop(child.stdin.take());
    let output = child.wait_with_output()?;
    let bytes = copy_result.map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("failed to send content to {}: {error}", backend.name),
        )
    })?;

    if !output.status.success() {
        let details = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let message = if details.is_empty() {
            format!("{} exited with status {}", backend.name, output.status)
        } else {
            format!(
                "{} exited with status {}: {details}",
                backend.name, output.status
            )
        };
        return Err(io::Error::new(io::ErrorKind::Other, message));
    }

    Ok(CopySummary {
        source,
        bytes,
        backend: backend.name,
    })
}

fn open_input(file: Option<&Path>) -> io::Result<(Box<dyn Read>, String)> {
    match file {
        None => Ok((Box::new(io::stdin()), "stdin".to_string())),
        Some(path) if path == Path::new("-") => Ok((Box::new(io::stdin()), "stdin".to_string())),
        Some(path) => Ok((Box::new(File::open(path)?), path.display().to_string())),
    }
}

fn start_clipboard() -> io::Result<(Child, ClipboardBackend)> {
    for &backend in CLIPBOARD_BACKENDS {
        let result = Command::new(backend.program)
            .args(backend.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn();

        match result {
            Ok(child) => return Ok((child, backend)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(io::Error::new(
                    error.kind(),
                    format!("failed to start {}: {error}", backend.name),
                ));
            }
        }
    }

    Err(io::Error::new(
        io::ErrorKind::NotFound,
        missing_backend_message(),
    ))
}

fn missing_backend_message() -> String {
    let names = CLIPBOARD_BACKENDS
        .iter()
        .map(|backend| backend.name)
        .collect::<Vec<_>>()
        .join(", ");

    if names.is_empty() {
        "clipboard is not supported on this operating system".to_string()
    } else if cfg!(target_os = "linux") {
        format!("no clipboard backend found ({names}); install wl-clipboard, xclip, or xsel")
    } else {
        format!("no clipboard backend found ({names})")
    }
}

fn format_number(bytes: u64, singular: &str, plural: &str) -> String {
    let unit = if bytes == 1 { singular } else { plural };
    format!("{} {unit}", format_integer(bytes))
}

fn format_integer(value: u64) -> String {
    let digits = value.to_string();
    let first_group_len = match digits.len() % 3 {
        0 => 3,
        remainder => remainder,
    };
    let mut formatted = String::with_capacity(digits.len() + digits.len() / 3);
    formatted.push_str(&digits[..first_group_len]);
    for chunk in digits[first_group_len..].as_bytes().chunks(3) {
        formatted.push(',');
        formatted.push_str(std::str::from_utf8(chunk).expect("digits are valid UTF-8"));
    }
    formatted
}

fn format_bytes(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }

    if unit == 0 {
        format!("{bytes} B")
    } else if value < 10.0 {
        format!("{value:.1} {}", UNITS[unit])
    } else {
        format!("{value:.0} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn parses_file_and_stdin_inputs() {
        let args = Args::try_parse_from(["clipboard", "notes.txt"]).unwrap();
        assert_eq!(args.file.as_deref(), Some(Path::new("notes.txt")));

        let args = Args::try_parse_from(["clipboard", "-"]).unwrap();
        assert_eq!(args.file.as_deref(), Some(Path::new("-")));
    }

    #[test]
    fn formats_summary_sizes_for_humans() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(1024), "1.0 KiB");
        assert_eq!(format_bytes(12_345), "12 KiB");
        assert_eq!(format_number(1, "byte", "bytes"), "1 byte");
        assert_eq!(format_number(1_234, "byte", "bytes"), "1,234 bytes");
    }

    #[test]
    fn missing_backend_message_explains_linux_setup() {
        if cfg!(target_os = "linux") {
            let message = missing_backend_message();
            assert!(message.contains("wl-clipboard"));
            assert!(message.contains("xclip"));
            assert!(message.contains("xsel"));
        }
    }
}
