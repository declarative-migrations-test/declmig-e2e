use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

struct TempRepo {
    root: PathBuf,
}

impl TempRepo {
    fn new(label: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "oresfmt-{label}-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&root).unwrap();
        git(&root, &["init", "-q"]);
        git(&root, &["config", "user.name", "oresfmt-tests"]);
        git(&root, &["config", "user.email", "oresfmt-tests@example.invalid"]);
        Self { root }
    }

    fn write(&self, name: &str, content: &str) -> PathBuf {
        let path = self.root.join(name);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&path, content).unwrap();
        path
    }

    fn commit_all(&self, message: &str) {
        git(&self.root, &["add", "."]);
        git(&self.root, &["commit", "-qm", message]);
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_oresfmt"))
            .current_dir(&self.root)
            .args(args)
            .output()
            .unwrap()
    }
}

impl Drop for TempRepo {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn git(root: &Path, args: &[&str]) {
    let output = Command::new("git")
        .current_dir(root)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
}

const UNFORMATTED: &str = "fnc answer() => int {\nreturn 42;\n}\n";
const FORMATTED: &str = "fnc answer() -> int {\n  return 42;\n}\n";

#[test]
fn dry_run_is_default_and_never_writes() {
    let repo = TempRepo::new("dry-run");
    let path = repo.write("sample.ores", UNFORMATTED);

    let output = repo.run(&["sample.ores"]);

    assert!(output.status.success());
    assert_eq!(fs::read_to_string(path).unwrap(), UNFORMATTED);
    assert!(String::from_utf8_lossy(&output.stdout).contains("would format sample.ores"));
}

#[test]
fn check_is_nonzero_but_does_not_write() {
    let repo = TempRepo::new("check");
    let path = repo.write("sample.ores", UNFORMATTED);

    let output = repo.run(&["--check", "sample.ores"]);

    assert_eq!(output.status.code(), Some(1));
    assert_eq!(fs::read_to_string(path).unwrap(), UNFORMATTED);
    assert!(String::from_utf8_lossy(&output.stderr).contains("needs formatting"));
}

#[test]
fn write_allows_clean_tracked_file() {
    let repo = TempRepo::new("clean-tracked");
    let path = repo.write("sample.ores", UNFORMATTED);
    repo.commit_all("baseline");

    let output = repo.run(&["--write", "sample.ores"]);

    assert!(output.status.success());
    assert_eq!(fs::read_to_string(path).unwrap(), FORMATTED);
}

#[test]
fn write_refuses_dirty_tracked_file_by_default() {
    let repo = TempRepo::new("dirty-tracked");
    let path = repo.write("sample.ores", UNFORMATTED);
    repo.commit_all("baseline");
    repo.write("sample.ores", "fnc answer() => int {\nreturn 43;\n}\n");
    let before = fs::read_to_string(&path).unwrap();

    let output = repo.run(&["--write", "sample.ores"]);

    assert_eq!(output.status.code(), Some(2));
    assert_eq!(fs::read_to_string(path).unwrap(), before);
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("refusing to modify dirty tracked file")
    );
}

#[test]
fn dirty_tracked_override_is_explicit() {
    let repo = TempRepo::new("dirty-override");
    let path = repo.write("sample.ores", UNFORMATTED);
    repo.commit_all("baseline");
    repo.write("sample.ores", "fnc answer() => int {\nreturn 43;\n}\n");

    let output = repo.run(&[
        "--write",
        "--ok-to-mod-dirty-files",
        "sample.ores",
    ]);

    assert!(output.status.success());
    assert_eq!(
        fs::read_to_string(path).unwrap(),
        "fnc answer() -> int {\n  return 43;\n}\n"
    );
}

#[test]
fn write_refuses_untracked_file_unless_explicitly_allowed() {
    let repo = TempRepo::new("untracked");
    let path = repo.write("sample.ores", UNFORMATTED);

    let refused = repo.run(&["--write", "sample.ores"]);
    assert_eq!(refused.status.code(), Some(2));
    assert_eq!(fs::read_to_string(&path).unwrap(), UNFORMATTED);
    assert!(
        String::from_utf8_lossy(&refused.stderr)
            .contains("refusing to modify untracked or ignored file")
    );

    let allowed = repo.run(&[
        "--write",
        "--ok-to-mod-untracked-files",
        "sample.ores",
    ]);
    assert!(allowed.status.success());
    assert_eq!(fs::read_to_string(path).unwrap(), FORMATTED);
}

#[test]
fn write_preflights_every_change_before_touching_any_file() {
    let repo = TempRepo::new("atomic-preflight");
    let tracked = repo.write("a.ores", UNFORMATTED);
    repo.commit_all("tracked baseline");
    let untracked = repo.write("b.ores", UNFORMATTED);

    let output = repo.run(&["--write", "."]);

    assert_eq!(output.status.code(), Some(2));
    assert_eq!(fs::read_to_string(tracked).unwrap(), UNFORMATTED);
    assert_eq!(fs::read_to_string(untracked).unwrap(), UNFORMATTED);
}

#[test]
fn write_outside_git_fails_closed_without_override() {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!(
        "oresfmt-outside-git-{}-{nonce}",
        std::process::id()
    ));
    fs::create_dir_all(&root).unwrap();
    let path = root.join("sample.ores");
    fs::write(&path, UNFORMATTED).unwrap();

    let refused = Command::new(env!("CARGO_BIN_EXE_oresfmt"))
        .current_dir(&root)
        .args(["--write", "sample.ores"])
        .output()
        .unwrap();
    assert_eq!(refused.status.code(), Some(2));
    assert_eq!(fs::read_to_string(&path).unwrap(), UNFORMATTED);

    let allowed = Command::new(env!("CARGO_BIN_EXE_oresfmt"))
        .current_dir(&root)
        .args(["--write", "--ok-to-mod-outside-git", "sample.ores"])
        .output()
        .unwrap();
    assert!(allowed.status.success());
    assert_eq!(fs::read_to_string(&path).unwrap(), FORMATTED);

    let _ = fs::remove_dir_all(root);
}
