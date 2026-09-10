use clap::Parser;
use std::{
    ffi::OsString,
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    process,
    sync::atomic::{AtomicU64, Ordering},
};

const BUFFER_SIZE: usize = 1024 * 1024;
const TEMP_ATTEMPTS: u32 = 256;
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Generate a file of an exact logical size.
#[derive(Parser, Debug)]
#[command(
    name = "file-gen",
    version,
    about = "Generate a file with a requested size"
)]
pub struct Args {
    /// Path for the generated file
    #[arg(value_name = "OUTPUT")]
    pub output: PathBuf,

    /// Size in bytes, or with B/KB/MB/GB/TB/KiB/MiB/GiB/TiB units
    #[arg(value_name = "SIZE")]
    pub size: String,

    /// Fill the file with random bytes instead of zero bytes
    #[arg(long)]
    pub random: bool,

    /// Replace an existing output file after successful generation
    #[arg(long)]
    pub force: bool,
}

pub fn run(args: Args) -> i32 {
    let size = match parse_size(&args.size) {
        Ok(size) => size,
        Err(error) => {
            eprintln!("file-gen: {error}");
            return 1;
        }
    };

    let result = create_file_with(&args.output, size, args.random, args.force, |buffer| {
        getrandom::fill(buffer).map_err(io::Error::other)
    });

    match result {
        Ok(()) => {
            let mode = if args.random { "random" } else { "zero-filled" };
            println!("Created {} ({} bytes, {mode})", args.output.display(), size);
            0
        }
        Err(error) => {
            eprintln!("file-gen: {}: {error}", args.output.display());
            1
        }
    }
}

/// Parse a byte count with optional decimal or binary unit suffix.
pub fn parse_size(value: &str) -> Result<u64, String> {
    let value = value.trim();
    if value.is_empty() {
        return Err("size must be a non-empty unsigned integer with an optional unit".to_string());
    }

    let split = value
        .find(|character: char| !character.is_ascii_digit())
        .unwrap_or(value.len());
    let (digits, suffix) = value.split_at(split);
    if digits.is_empty()
        || !suffix
            .chars()
            .all(|character| character.is_ascii_alphabetic())
    {
        return Err(format!("invalid size {value:?}"));
    }

    let number = digits
        .parse::<u64>()
        .map_err(|_| format!("size {value:?} is outside the supported range"))?;
    let multiplier = match suffix.to_ascii_lowercase().as_str() {
        "" | "b" => 1,
        "kb" => 1_000,
        "mb" => 1_000_000,
        "gb" => 1_000_000_000,
        "tb" => 1_000_000_000_000,
        "kib" => 1 << 10,
        "mib" => 1 << 20,
        "gib" => 1 << 30,
        "tib" => 1 << 40,
        _ => return Err(format!("unknown size unit in {value:?}")),
    };
    number
        .checked_mul(multiplier)
        .ok_or_else(|| format!("size {value:?} is outside the supported range"))
}

fn create_file_with<F>(
    output: &Path,
    size: u64,
    random: bool,
    force: bool,
    mut fill_random: F,
) -> io::Result<()>
where
    F: FnMut(&mut [u8]) -> io::Result<()>,
{
    let existing = fs::symlink_metadata(output);
    match existing {
        Ok(metadata) if metadata.is_dir() => {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "output path is a directory",
            ));
        }
        Ok(_) if !force => {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "output file already exists (use --force to replace it)",
            ));
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }

    let (temporary, mut file) = create_temporary_file(output)?;
    let write_result =
        write_contents(&mut file, size, random, &mut fill_random).and_then(|()| file.sync_all());
    drop(file);

    if let Err(error) = write_result {
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }

    let persist_result = if force {
        replace_file(&temporary, output)
    } else {
        // Linking is a same-directory, no-clobber publish: it cannot replace a
        // file that appeared after the initial existence check.
        fs::hard_link(&temporary, output).and_then(|()| fs::remove_file(&temporary))
    };

    if let Err(error) = persist_result {
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }
    Ok(())
}

fn write_contents<F>(
    file: &mut File,
    size: u64,
    random: bool,
    fill_random: &mut F,
) -> io::Result<()>
where
    F: FnMut(&mut [u8]) -> io::Result<()>,
{
    let mut buffer = vec![0; BUFFER_SIZE];
    let mut remaining = size;
    while remaining > 0 {
        let length = remaining.min(buffer.len() as u64) as usize;
        let chunk = &mut buffer[..length];
        if random {
            fill_random(chunk)?;
        }
        file.write_all(chunk)?;
        remaining -= length as u64;
    }
    file.flush()
}

fn create_temporary_file(output: &Path) -> io::Result<(PathBuf, File)> {
    let parent = output
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let file_name = output
        .file_name()
        .filter(|name| !name.is_empty())
        .ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "output path must name a file")
        })?;

    for _ in 0..TEMP_ATTEMPTS {
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let mut name = OsString::from(".");
        name.push(file_name);
        name.push(format!(".ut-file-gen-{}-{sequence}.tmp", process::id()));
        let path = parent.join(name);
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => return Ok((path, file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }

    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not reserve a temporary output path",
    ))
}

#[cfg(not(windows))]
fn replace_file(temporary: &Path, output: &Path) -> io::Result<()> {
    // POSIX rename atomically replaces an existing file on the same filesystem.
    fs::rename(temporary, output)
}

#[cfg(windows)]
fn replace_file(temporary: &Path, output: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{ReplaceFileW, REPLACEFILE_WRITE_THROUGH};

    if fs::symlink_metadata(output).is_err_and(|error| error.kind() == io::ErrorKind::NotFound) {
        return fs::rename(temporary, output);
    }

    let output_wide = output
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let temporary_wide = temporary
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let replaced = unsafe {
        ReplaceFileW(
            output_wide.as_ptr(),
            temporary_wide.as_ptr(),
            std::ptr::null(),
            REPLACEFILE_WRITE_THROUGH,
            std::ptr::null(),
            std::ptr::null(),
        )
    };
    if replaced == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        io::Read,
        sync::atomic::{AtomicU64, Ordering},
    };

    static TEST_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let sequence = TEST_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let path =
                std::env::temp_dir().join(format!("file-gen-test-{}-{sequence}", process::id()));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn parses_bytes_and_common_units() {
        assert_eq!(parse_size("0").unwrap(), 0);
        assert_eq!(parse_size("1024").unwrap(), 1024);
        assert_eq!(parse_size("10KB").unwrap(), 10_000);
        assert_eq!(parse_size("10mIb").unwrap(), 10 * 1024 * 1024);
        assert_eq!(parse_size("1GiB").unwrap(), 1024 * 1024 * 1024);
    }

    #[test]
    fn rejects_invalid_and_overflowing_sizes() {
        for value in [
            "",
            "-1",
            "1.5MiB",
            "1XB",
            "MiB",
            "18446744073709551616",
            "18446744073709551615KiB",
        ] {
            assert!(parse_size(value).is_err(), "{value} should be rejected");
        }
    }

    #[test]
    fn writes_zero_and_random_files_with_requested_size() {
        let directory = TestDirectory::new();
        let zero = directory.0.join("zero.bin");
        create_file_with(&zero, 4097, false, false, |_| Ok(())).unwrap();
        let zero_contents = fs::read(&zero).unwrap();
        assert_eq!(zero_contents.len(), 4097);
        assert!(zero_contents.iter().all(|byte| *byte == 0));

        let random = directory.0.join("random.bin");
        create_file_with(&random, 4097, true, false, |buffer| {
            getrandom::fill(buffer).map_err(io::Error::other)
        })
        .unwrap();
        let random_contents = fs::read(&random).unwrap();
        assert_eq!(random_contents.len(), 4097);
        assert!(random_contents.iter().any(|byte| *byte != 0));
    }

    #[test]
    fn protects_existing_file_unless_forced() {
        let directory = TestDirectory::new();
        let output = directory.0.join("existing.bin");
        fs::write(&output, b"keep me").unwrap();

        assert!(create_file_with(&output, 16, false, false, |_| Ok(())).is_err());
        assert_eq!(fs::read(&output).unwrap(), b"keep me");

        create_file_with(&output, 16, false, true, |_| Ok(())).unwrap();
        assert_eq!(fs::read(&output).unwrap(), vec![0; 16]);
    }

    #[test]
    fn failure_cleans_up_temporary_file_and_preserves_target() {
        let directory = TestDirectory::new();
        let output = directory.0.join("target.bin");
        fs::write(&output, b"original").unwrap();

        let error = create_file_with(&output, 128, true, true, |_| {
            Err(io::Error::other("random source failed"))
        })
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Other);
        assert_eq!(fs::read(&output).unwrap(), b"original");
        let names = fs::read_dir(&directory.0)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>();
        assert_eq!(names, vec![OsString::from("target.bin")]);
    }

    #[test]
    fn no_force_publish_does_not_clobber_a_new_target() {
        let directory = TestDirectory::new();
        let output = directory.0.join("target.bin");
        let target_created_during_generation = output.clone();
        assert!(create_file_with(&output, 8, true, false, move |_| {
            fs::write(&target_created_during_generation, b"new target")
        })
        .is_err());
        let mut contents = Vec::new();
        File::open(&output)
            .unwrap()
            .read_to_end(&mut contents)
            .unwrap();
        assert_eq!(contents, b"new target");
    }
}
