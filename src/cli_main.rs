use oreslang_format::{format_source, is_formatted};
use std::env;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

const HELP: &str = r#"oresfmt - the canonical Oreslang formatter

USAGE:
    oresfmt [--check] [--stdout] <path>...
    oresfmt [--check] -

OPTIONS:
    --check     Do not write files; exit 1 if any input is not canonical
    --stdout    Print one formatted file to stdout instead of writing it
    -h, --help  Print help
    -V, --version
                Print version

PATHS:
    Files are formatted directly. Directories are traversed recursively and
    only *.ores files are selected. '-' reads stdin and writes stdout.

There are intentionally no style flags or configuration files.
"#;

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
    let mut check = false;
    let mut stdout = false;
    let mut paths = Vec::new();

    for arg in env::args().skip(1) {
        match arg.as_str() {
            "--check" => check = true,
            "--stdout" => stdout = true,
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
    if stdout
        && (paths.len() != 1 || paths[0].as_os_str() == std::ffi::OsStr::new("-"))
    {
        return Err("--stdout requires exactly one filesystem file".into());
    }

    if paths.len() == 1 && paths[0].as_os_str() == std::ffi::OsStr::new("-") {
        let mut input = String::new();
        io::stdin().read_to_string(&mut input)?;
        let formatted = format_source(&input)?;
        if check {
            return Ok(if formatted == input {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(1)
            });
        }
        io::stdout().write_all(formatted.as_bytes())?;
        return Ok(ExitCode::SUCCESS);
    }

    let files = collect_files(&paths)?;
    if stdout {
        let input = fs::read_to_string(&files[0])?;
        let formatted = format_source(&input)?;
        print!("{formatted}");
        return Ok(ExitCode::SUCCESS);
    }

    let mut dirty = false;
    for path in files {
        let input = fs::read_to_string(&path)?;
        if check {
            if !is_formatted(&input)? {
                eprintln!("needs formatting: {}", path.display());
                dirty = true;
            }
            continue;
        }

        let formatted = format_source(&input)?;
        if formatted != input {
            fs::write(&path, formatted)?;
            println!("formatted {}", path.display());
        }
    }

    Ok(if dirty {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    })
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
