use std::{
    io,
    path::{Component, Path, PathBuf},
};

pub fn resolve_server_path(root: &Path, relative: &str) -> io::Result<PathBuf> {
    let relative_path = validate_relative(relative)?;
    let mut joined = root.to_path_buf();
    for component in relative_path.components() {
        joined.push(component);
        if std::fs::symlink_metadata(&joined)?.file_type().is_symlink() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "symbolic links are not shared",
            ));
        }
    }
    let canonical = joined.canonicalize()?;
    if !canonical.starts_with(root) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "path escapes the shared directory",
        ));
    }
    Ok(canonical)
}

pub fn safe_local_path(root: &Path, relative: &str) -> io::Result<PathBuf> {
    let relative = validate_relative(relative)?;
    let mut target = root.to_path_buf();
    for component in relative.components() {
        let Component::Normal(value) = component else {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "unsafe path"));
        };
        let value = value
            .to_str()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "path is not UTF-8"))?;
        target.push(portable_component(value));
    }
    if !target.starts_with(root) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "target path escapes the download directory",
        ));
    }
    #[cfg(windows)]
    if target.to_string_lossy().encode_utf16().count() > 240 {
        let hash = &blake3::hash(relative.as_bytes()).to_hex()[..16];
        let name = target
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("file");
        return Ok(root.join("__ut_long_paths").join(format!("{name}~{hash}")));
    }
    Ok(target)
}

pub fn validate_relative(relative: &str) -> io::Result<&Path> {
    if relative.contains('\0') || relative.contains('\\') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid protocol path",
        ));
    }
    let path = Path::new(relative);
    if path.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "absolute paths are not allowed",
        ));
    }
    for component in path.components() {
        if !matches!(component, Component::Normal(_)) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "path contains a forbidden component",
            ));
        }
    }
    Ok(path)
}

fn portable_component(value: &str) -> String {
    let invalid = value.chars().any(|character| {
        character.is_control()
            || matches!(
                character,
                '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*'
            )
    });
    let trailing = value.ends_with(' ') || value.ends_with('.');
    let stem = value.split('.').next().unwrap_or(value);
    let reserved = matches!(
        stem.to_ascii_uppercase().as_str(),
        "CON"
            | "PRN"
            | "AUX"
            | "NUL"
            | "COM1"
            | "COM2"
            | "COM3"
            | "COM4"
            | "COM5"
            | "COM6"
            | "COM7"
            | "COM8"
            | "COM9"
            | "LPT1"
            | "LPT2"
            | "LPT3"
            | "LPT4"
            | "LPT5"
            | "LPT6"
            | "LPT7"
            | "LPT8"
            | "LPT9"
    );
    let too_long = value.chars().count() > 120;
    if !(invalid || trailing || reserved || too_long) {
        return value.to_string();
    }

    let hash = &blake3::hash(value.as_bytes()).to_hex()[..10];
    let readable = value
        .chars()
        .map(|character| {
            if character.is_control()
                || matches!(
                    character,
                    '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*'
                )
            {
                '_'
            } else {
                character
            }
        })
        .take(80)
        .collect::<String>()
        .trim_end_matches([' ', '.'])
        .to_string();
    format!("{readable}~{hash}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_parent_and_absolute_paths() {
        assert!(validate_relative("../secret").is_err());
        assert!(validate_relative("/etc/passwd").is_err());
        assert!(validate_relative("ok/file.txt").is_ok());
    }

    #[test]
    fn maps_windows_reserved_names() {
        assert!(portable_component("CON.txt").starts_with("CON.txt~"));
        assert!(portable_component("normal.txt").eq("normal.txt"));
        assert!(!portable_component("a:b.txt").contains(':'));
    }

    #[cfg(unix)]
    #[test]
    fn server_paths_reject_symbolic_links() {
        use std::os::unix::fs::symlink;

        let root = std::env::temp_dir().join(format!("ut-path-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join("real")).unwrap();
        std::fs::write(root.join("real/file.txt"), b"data").unwrap();
        symlink(root.join("real"), root.join("linked")).unwrap();

        assert!(resolve_server_path(&root, "linked/file.txt").is_err());
        std::fs::remove_dir_all(root).unwrap();
    }
}
