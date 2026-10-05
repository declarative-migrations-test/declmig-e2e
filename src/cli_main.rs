use oreslang_format::{format_source, is_formatted};
use std::env;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

const HELP: &str = r#"oresfmt - the canonical Oreslang formatter

USAGE:
    oresfmt [--dry-run | --write | --check | --stdout] [safety flags] <path>...
    oresfmt [--check] -

MODES:
    --dry-run   Preview changes without modifying files (default)
    --write     Rewrite files in place after Git safety checks
    --check     Do not write files; exit 1 if any input is not canonical
    --stdout    Print one formatted file to stdout instead of writing it

WRITE SAFETY:
    --ok-to-mod-dirty-files
                Allow --write to modify tracked files with staged/unstaged changes
    --ok-to-mod-untracked-files
                Allow --write to modify untracked or ignored files
    --ok-to-mod-outside-git
                Allow --write outside a Git worktree

OTHER:
    -h, --help  Print help
    -V, --version
                Print version

PATHS:
    Files are formatted directly. Directories are traversed recursively and
    only *.ores files are selected. '-' reads stdin and writes stdout.

There are intentionally no style flags or configuration files.
"#;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    DryRun,
    Write,
    Check,
    Stdout,
}

#[derive(Debug, Clone, Copy, Default)]
struct WritePolicy {
    allow_dirty_tracked: bool,
    allow_untracked: bool,
    allow_outside_git: bool,
}

#[derive(Debug)]
enum GitFileState {
    CleanTracked,
    DirtyTracked(String),
    UntrackedOrIgnored,
    OutsideGit,
}

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(err) => {
            eprintln!("oresfmt: {err}");
            ExitCode::from(2)
        }
    }
}

fn run() -> Result<ExitCode, Box<dyn std::error::Error>> {
    let mut mode = Mode::DryRun;
    let mut explicit_mode: Option<&'static str> = None;
    let mut policy = WritePolicy::default();
    let mut paths = Vec::new();

    for arg in env::args().skip(1) {
        match arg.as_str() {
            "--dry-run" => select_mode(&mut mode, &mut explicit_mode, Mode::DryRun, "--dry-run")?,
            "--write" => select_mode(&mut mode, &mut explicit_mode, Mode::Write, "--write")?,
            "--check" => select_mode(&mut mode, &mut explicit_mode, Mode::Check, "--check")?,
            "--stdout" => select_mode(&mut mode, &mut explicit_mode, Mode::Stdout, "--stdout")?,
            "--ok-to-mod-dirty-files" => policy.allow_dirty_tracked = true,
            "--ok-to-mod-untracked-files" => policy.allow_untracked = true,
            "--ok-to-mod-outside-git" => policy.allow_outside_git = true,
            "-h" | "--help" => {
                print!("{HELP}");
                return Ok(ExitCode::SUCCESS);
            }
            "-V" | "--version" => {
                println!("oresfmt {}", env!("CARGO_PKG_VERSION"));
                return Ok(ExitCode::SUCCESS);
            }
            _ if arg.starts_with('-') && arg != "-" => {
                return Err(format!("unknown option: {arg}").into());
            }
            _ => paths.push(PathBuf::from(arg)),
        }
    }

    if paths.is_empty() {
        return Err(
            "no input paths; pass one or more .ores files/directories, or '-' for stdin".into(),
        );
    }

    let has_stdin = paths
        .iter()
        .any(|path| path.as_os_str() == std::ffi::OsStr::new("-"));
    if has_stdin && paths.len() != 1 {
        return Err("'-' cannot be combined with filesystem paths".into());
    }

    if has_stdin {
        if mode == Mode::Write {
            return Err("--write cannot be used with stdin".into());
        }

        let mut input = String::new();
        io::stdin().read_to_string(&mut input)?;
        let formatted = format_source(&input)?;

        if mode == Mode::Check {
            return Ok(if formatted == input {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(1)
            });
        }

        io::stdout().write_all(formatted.as_bytes())?;
        return Ok(ExitCode::SUCCESS);
    }

    if mode == Mode::Stdout {
        if paths.len() != 1 || !paths[0].is_file() {
            return Err("--stdout requires exactly one filesystem file".into());
        }
        let input = fs::read_to_string(&paths[0])?;
        let formatted = format_source(&input)?;
        print!("{formatted}");
        return Ok(ExitCode::SUCCESS);
    }

    let files = collect_files(&paths)?;

    match mode {
        Mode::DryRun => {
            for path in files {
                let input = fs::read_to_string(&path)?;
                if !is_formatted(&input)? {
                    println!("would format {}", path.display());
                }
            }
            Ok(ExitCode::SUCCESS)
        }
        Mode::Check => {
            let mut dirty = false;
            for path in files {
                let input = fs::read_to_string(&path)?;
                if !is_formatted(&input)? {
                    eprintln!("needs formatting: {}", path.display());
                    dirty = true;
                }
            }
            Ok(if dirty {
                ExitCode::from(1)
            } else {
                ExitCode::SUCCESS
            })
        }
        Mode::Write => write_files(files, policy),
        Mode::Stdout => unreachable!("handled above"),
    }
}

fn select_mode(
    mode: &mut Mode,
    explicit_mode: &mut Option<&'static str>,
    next: Mode,
    flag: &'static str,
) -> Result<(), Box<dyn std::error::Error>> {
    if let Some(previous) = *explicit_mode {
        if previous != flag {
            return Err(format!("{previous} cannot be combined with {flag}").into());
        }
    }

    *mode = next;
    *explicit_mode = Some(flag);
    Ok(())
}

fn write_files(
    files: Vec<PathBuf>,
    policy: WritePolicy,
) -> Result<ExitCode, Box<dyn std::error::Error>> {
    let mut changes = Vec::new();

    for path in files {
        let input = fs::read_to_string(&path)?;
        let formatted = format_source(&input)?;
        if formatted != input {
            changes.push((path, formatted));
        }
    }

    // Validate the entire write set first. A refusal on the last file must not
    // leave the workspace half-formatted.
    for (path, _) in &changes {
        enforce_write_policy(path, policy)?;
    }

    for (path, formatted) in changes {
        fs::write(&path, formatted)?;
        println!("formatted {}", path.display());
    }

    Ok(ExitCode::SUCCESS)
}

fn enforce_write_policy(
    path: &Path,
    policy: WritePolicy,
) -> Result<(), Box<dyn std::error::Error>> {
    match git_file_state(path)? {
        GitFileState::CleanTracked => Ok(()),
        GitFileState::DirtyTracked(status) if policy.allow_dirty_tracked => {
            eprintln!(
                "warning: modifying dirty tracked file: {} ({status})",
                path.display()
            );
            Ok(())
        }
        GitFileState::DirtyTracked(status) => Err(format!(
            "refusing to modify dirty tracked file: {} ({status}); commit/stash/revert it, or pass --ok-to-mod-dirty-files",
            path.display()
        )
        .into()),
        GitFileState::UntrackedOrIgnored if policy.allow_untracked => {
            eprintln!(
                "warning: modifying untracked/ignored file: {}",
                path.display()
            );
            Ok(())
        }
        GitFileState::UntrackedOrIgnored => Err(format!(
            "refusing to modify untracked or ignored file: {}; add+commit it, or pass --ok-to-mod-untracked-files",
            path.display()
        )
        .into()),
        GitFileState::OutsideGit if policy.allow_outside_git => {
            eprintln!(
                "warning: modifying file outside a Git worktree: {}",
                path.display()
            );
            Ok(())
        }
        GitFileState::OutsideGit => Err(format!(
            "refusing to modify file outside a Git worktree: {}; pass --ok-to-mod-outside-git to override",
            path.display()
        )
        .into()),
    }
}

fn git_file_state(path: &Path) -> Result<GitFileState, Box<dyn std::error::Error>> {
    let absolute = fs::canonicalize(path)?;
    let parent = absolute
        .parent()
        .ok_or_else(|| format!("cannot determine parent directory for {}", path.display()))?;

    let root_output = Command::new("git")
        .arg("-C")
        .arg(parent)
        .args(["rev-parse", "--show-toplevel"])
        .output()?;

    if !root_output.status.success() {
        return Ok(GitFileState::OutsideGit);
    }

    let root_text = String::from_utf8(root_output.stdout)?;
    let root = fs::canonicalize(root_text.trim())?;
    let relative = absolute.strip_prefix(&root).map_err(|_| {
        format!(
            "Git reported worktree root {} but file {} is outside it",
            root.display(),
            absolute.display()
        )
    })?;

    let tracked = Command::new("git")
        .arg("-C")
        .arg(&root)
        .args(["ls-files", "--error-unmatch", "--"])
        .arg(relative)
        .output()?;

    if !tracked.status.success() {
        return Ok(GitFileState::UntrackedOrIgnored);
    }

    let status = Command::new("git")
        .arg("-C")
        .arg(&root)
        .args(["status", "--porcelain=v1", "--untracked-files=no", "--"])
        .arg(relative)
        .output()?;

    if !status.status.success() {
        return Err(format!("git status failed for {}", path.display()).into());
    }

    let status_text = String::from_utf8(status.stdout)?;
    let status_text = status_text.trim();
    if status_text.is_empty() {
        Ok(GitFileState::CleanTracked)
    } else {
        Ok(GitFileState::DirtyTracked(status_text.to_string()))
    }
}

fn collect_files(inputs: &[PathBuf]) -> Result<Vec<PathBuf>, Box<dyn std::error::Error>> {
    let mut files = Vec::new();
    for path in inputs {
        collect_path(path, &mut files)?;
    }
    files.sort();
    files.dedup();
    if files.is_empty() {
        return Err("no .ores files found".into());
    }
    Ok(files)
}

fn collect_path(path: &Path, out: &mut Vec<PathBuf>) -> Result<(), Box<dyn std::error::Error>> {
    if path.is_file() {
        out.push(path.to_path_buf());
        return Ok(());
    }
    if !path.is_dir() {
        return Err(format!("path does not exist: {}", path.display()).into());
    }

    let mut entries = fs::read_dir(path)?.collect::<Result<Vec<_>, _>>()?;
    entries.sort_by_key(|entry| entry.file_name());

    for entry in entries {
        let child = entry.path();
        let name = entry.file_name();

        if child.is_dir() && matches!(name.to_str(), Some(".git" | "target" | ".cache")) {
            continue;
        }
        if child.is_dir() {
            collect_path(&child, out)?;
        } else if child.extension().is_some_and(|ext| ext == "ores") {
            out.push(child);
        }
    }

    Ok(())
}
