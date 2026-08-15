use super::Cli;
use clap::{Args as ClapArgs, CommandFactory, Subcommand, ValueEnum};
use clap_complete::{generate, Shell};
use std::{
    env, fs, io,
    path::{Path, PathBuf},
};

const BLOCK_START: &str = "# >>> ut shell completions >>>";
const BLOCK_END: &str = "# <<< ut shell completions <<<";

#[derive(ClapArgs, Debug)]
pub(crate) struct Args {
    #[command(subcommand)]
    action: Action,
}

#[derive(Subcommand, Debug)]
enum Action {
    /// Print Bash completion code to stdout
    Bash,
    /// Print Zsh completion code to stdout
    Zsh,
    /// Print Fish completion code to stdout
    Fish,
    /// Print PowerShell completion code to stdout
    #[command(name = "powershell", alias = "pwsh")]
    PowerShell,
    /// Detect the current shell and enable completion on future shell startups
    Install {
        /// Override automatic shell detection
        #[arg(long, value_enum)]
        shell: Option<CompletionShell>,
    },
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum CompletionShell {
    Bash,
    Zsh,
    Fish,
    #[value(name = "powershell", alias = "pwsh")]
    PowerShell,
}

pub(crate) fn run(args: Args) -> io::Result<()> {
    match args.action {
        Action::Bash => print(Shell::Bash),
        Action::Zsh => print(Shell::Zsh),
        Action::Fish => print(Shell::Fish),
        Action::PowerShell => print(Shell::PowerShell),
        Action::Install { shell } => install(shell.unwrap_or(detect_shell()?)),
    }
}

fn print(shell: Shell) -> io::Result<()> {
    let mut command = Cli::command();
    generate(shell, &mut command, "ut", &mut io::stdout());
    Ok(())
}

fn detect_shell() -> io::Result<CompletionShell> {
    if let Some(shell) = env::var_os("SHELL") {
        let name = Path::new(&shell)
            .file_stem()
            .and_then(|name| name.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        if name.contains("bash") {
            return Ok(CompletionShell::Bash);
        }
        if name.contains("zsh") {
            return Ok(CompletionShell::Zsh);
        }
        if name.contains("fish") {
            return Ok(CompletionShell::Fish);
        }
        if name.contains("pwsh") || name.contains("powershell") {
            return Ok(CompletionShell::PowerShell);
        }
    }
    if cfg!(windows) {
        return Ok(CompletionShell::PowerShell);
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        "could not detect the current shell; use `ut completions install --shell <bash|zsh|fish|powershell>`",
    ))
}

fn install(shell: CompletionShell) -> io::Result<()> {
    let home = dirs::home_dir().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "could not determine the home directory",
        )
    })?;
    let (profile, body) = install_target(shell, &home)?;
    if !write_managed_profile(&profile, body)? {
        println!("Tab completion is already installed for {shell:?}");
        println!("  Profile: {}", profile.display());
        return Ok(());
    }
    println!("Installed tab completion for {shell:?}");
    println!("  Profile: {}", profile.display());
    println!("Restart the shell or source the profile to activate it.");
    if executable_on_path("ut").is_none() {
        println!("Note: keep `ut` on PATH so the completion hook can find it.");
    }
    Ok(())
}

fn write_managed_profile(profile: &Path, body: &str) -> io::Result<bool> {
    let existing = fs::read_to_string(profile).unwrap_or_default();
    let updated = upsert_managed_block(&existing, body);
    if updated == existing {
        return Ok(false);
    }
    if let Some(parent) = profile.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(profile, updated)?;
    Ok(true)
}

fn install_target(shell: CompletionShell, home: &Path) -> io::Result<(PathBuf, &'static str)> {
    match shell {
        CompletionShell::Bash => Ok((
            if cfg!(target_os = "macos") {
                home.join(".bash_profile")
            } else {
                home.join(".bashrc")
            },
            "if command -v ut >/dev/null 2>&1; then\n  source <(ut completions bash)\nfi",
        )),
        CompletionShell::Zsh => Ok((
            home.join(".zshrc"),
            "if command -v ut >/dev/null 2>&1; then\n  autoload -Uz compinit\n  (( $+functions[compdef] )) || compinit\n  source <(ut completions zsh)\nfi",
        )),
        CompletionShell::Fish => Ok((
            xdg_config_home(home).join("fish/config.fish"),
            "if type -q ut\n    ut completions fish | source\nend",
        )),
        CompletionShell::PowerShell => Ok((
            powershell_profile(home)?,
            "if (Get-Command ut -ErrorAction SilentlyContinue) {\n    ut completions powershell | Out-String | Invoke-Expression\n}",
        )),
    }
}

fn xdg_config_home(home: &Path) -> PathBuf {
    env::var_os("XDG_CONFIG_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".config"))
}

fn powershell_profile(home: &Path) -> io::Result<PathBuf> {
    if cfg!(windows) {
        let documents = dirs::document_dir().unwrap_or_else(|| home.join("Documents"));
        let legacy = env::var_os("PSModulePath")
            .map(|paths| paths.to_string_lossy().to_ascii_lowercase())
            .is_some_and(|paths| paths.contains("windowspowershell"));
        let directory = if legacy {
            "WindowsPowerShell"
        } else {
            "PowerShell"
        };
        Ok(documents
            .join(directory)
            .join("Microsoft.PowerShell_profile.ps1"))
    } else {
        Ok(xdg_config_home(home)
            .join("powershell")
            .join("Microsoft.PowerShell_profile.ps1"))
    }
}

fn upsert_managed_block(existing: &str, body: &str) -> String {
    let block = format!("{BLOCK_START}\n{body}\n{BLOCK_END}");
    if let Some(start) = existing.find(BLOCK_START) {
        if let Some(relative_end) = existing[start..].find(BLOCK_END) {
            let end = start + relative_end + BLOCK_END.len();
            let mut updated = String::with_capacity(existing.len() + block.len());
            updated.push_str(&existing[..start]);
            updated.push_str(&block);
            updated.push_str(&existing[end..]);
            return updated;
        }
    }
    let mut updated = existing.trim_end_matches(['\r', '\n']).to_string();
    if !updated.is_empty() {
        updated.push_str("\n\n");
    }
    updated.push_str(&block);
    updated.push('\n');
    updated
}

fn executable_on_path(name: &str) -> Option<PathBuf> {
    let paths = env::var_os("PATH")?;
    for directory in env::split_paths(&paths) {
        #[cfg(windows)]
        let candidates = [
            format!("{name}.exe"),
            format!("{name}.cmd"),
            name.to_string(),
        ];
        #[cfg(not(windows))]
        let candidates = [name.to_string()];
        for candidate in candidates {
            let path = directory.join(candidate);
            if path.is_file() {
                return Some(path);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn parses_generation_and_install_commands() {
        assert!(Cli::try_parse_from(["ut", "completions", "zsh"]).is_ok());
        assert!(
            Cli::try_parse_from(["ut", "completions", "install", "--shell", "powershell"]).is_ok()
        );
    }

    #[test]
    fn managed_profile_block_is_idempotent_and_replaceable() {
        let first = upsert_managed_block("export KEEP=1\n", "old hook");
        let same = upsert_managed_block(&first, "old hook");
        assert_eq!(same, first);

        let replaced = upsert_managed_block(&first, "new hook");
        assert!(replaced.contains("export KEEP=1"));
        assert!(replaced.contains("new hook"));
        assert!(!replaced.contains("old hook"));
        assert_eq!(replaced.matches(BLOCK_START).count(), 1);
    }

    #[test]
    fn profile_install_preserves_existing_content_and_is_idempotent() {
        let directory = std::env::temp_dir().join(format!(
            "ut-completion-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let profile = directory.join(".zshrc");
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(&profile, "export KEEP=1\n").unwrap();

        assert!(write_managed_profile(&profile, "completion hook").unwrap());
        assert!(!write_managed_profile(&profile, "completion hook").unwrap());
        let content = std::fs::read_to_string(&profile).unwrap();
        assert!(content.contains("export KEEP=1"));
        assert!(content.contains("completion hook"));

        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn generated_completions_contain_public_commands_only() {
        for shell in [Shell::Bash, Shell::Zsh, Shell::Fish, Shell::PowerShell] {
            let mut command = Cli::command();
            let mut output = Vec::new();
            generate(shell, &mut command, "ut", &mut output);
            let output = String::from_utf8(output).unwrap();
            assert!(output.contains("ut"));
            assert!(output.contains("file-transfer"));
            assert!(!output.contains("control-stdin"));
        }
    }
}
