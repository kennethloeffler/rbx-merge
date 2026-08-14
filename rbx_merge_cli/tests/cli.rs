//! End-to-end tests for the CLI contract that Git's merge and diff drivers
//! depend on: a clean merge writes `--out` and exits 0, a conflict writes
//! nothing and exits non-zero, and `textconv` prints semantic text to stdout.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_rbx-merge");

fn intvalue_model(name: &str, value: i64) -> String {
    format!(
        "<roblox version=\"4\">\n  <Item class=\"IntValue\" referent=\"RBX0\">\n    \
         <Properties>\n      <string name=\"Name\">{name}</string>\n      \
         <int64 name=\"Value\">{value}</int64>\n    </Properties>\n  </Item>\n</roblox>\n"
    )
}

struct Scratch {
    dir: PathBuf,
}

impl Scratch {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "rbx-merge-cli-{}-{tag}-{}",
            std::process::id(),
            tag.len()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create scratch dir");
        Self { dir }
    }

    fn write(&self, name: &str, contents: &str) -> PathBuf {
        let path = self.dir.join(name);
        fs::write(&path, contents).expect("write scratch file");
        path
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn run_merge(base: &Path, ours: &Path, theirs: &Path, out: &Path) -> std::process::Output {
    Command::new(BIN)
        .args(["merge", "--base"])
        .arg(base)
        .arg("--ours")
        .arg(ours)
        .arg("--theirs")
        .arg(theirs)
        .arg("--out")
        .arg(out)
        .args(["--path", "model.rbxmx"])
        .output()
        .expect("run rbx-merge merge")
}

#[test]
fn clean_merge_writes_output_and_succeeds() {
    let scratch = Scratch::new("clean");
    // ours edits Value; theirs is unchanged from base, so the merge is clean.
    let base = scratch.write("base.rbxmx", &intvalue_model("Counter", 1));
    let ours = scratch.write("ours.rbxmx", &intvalue_model("Counter", 2));
    let theirs = scratch.write("theirs.rbxmx", &intvalue_model("Counter", 1));
    let out = scratch.path("out.rbxmx");

    let output = run_merge(&base, &ours, &theirs, &out);
    assert!(
        output.status.success(),
        "clean merge should exit 0, stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(out.exists(), "clean merge should write the output file");

    let textconv = Command::new(BIN)
        .arg("textconv")
        .arg(&out)
        .output()
        .expect("run textconv");
    let rendered = String::from_utf8_lossy(&textconv.stdout);
    assert!(
        rendered.contains("Value = Int64(2)"),
        "merged output should carry ours' edit, got:\n{rendered}"
    );
}

#[test]
fn conflicting_merge_fails_without_writing_output() {
    let scratch = Scratch::new("conflict");
    // Both sides change the same property to different values.
    let base = scratch.write("base.rbxmx", &intvalue_model("Counter", 1));
    let ours = scratch.write("ours.rbxmx", &intvalue_model("Counter", 2));
    let theirs = scratch.write("theirs.rbxmx", &intvalue_model("Counter", 3));
    let out = scratch.path("out.rbxmx");

    let output = run_merge(&base, &ours, &theirs, &out);
    assert!(
        !output.status.success(),
        "conflicting merge should exit non-zero"
    );
    assert!(
        !out.exists(),
        "conflicting merge must not write the output file"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("PropertyValue"),
        "conflict should be reported on stderr, got:\n{stderr}"
    );
}

#[test]
fn take_ours_resolves_conflict_and_writes_output() {
    let scratch = Scratch::new("take-ours");
    let base = scratch.write("base.rbxmx", &intvalue_model("Counter", 1));
    let ours = scratch.write("ours.rbxmx", &intvalue_model("Counter", 2));
    let theirs = scratch.write("theirs.rbxmx", &intvalue_model("Counter", 3));
    let out = scratch.path("out.rbxmx");

    let output = Command::new(BIN)
        .args(["merge", "--base"])
        .arg(&base)
        .arg("--ours")
        .arg(&ours)
        .arg("--theirs")
        .arg(&theirs)
        .arg("--out")
        .arg(&out)
        .args(["--path", "model.rbxmx", "--take", "ours"])
        .output()
        .expect("run rbx-merge merge --take ours");
    assert!(
        output.status.success(),
        "--take ours should resolve the conflict, stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(out.exists());

    let textconv = Command::new(BIN)
        .arg("textconv")
        .arg(&out)
        .output()
        .expect("run textconv");
    let rendered = String::from_utf8_lossy(&textconv.stdout);
    assert!(
        rendered.contains("Value = Int64(2)"),
        "resolved output should take ours, got:\n{rendered}"
    );
}

#[test]
fn conflict_report_round_trip_resolves() {
    let scratch = Scratch::new("report");
    let base = scratch.write("base.rbxmx", &intvalue_model("Counter", 1));
    let ours = scratch.write("ours.rbxmx", &intvalue_model("Counter", 2));
    let theirs = scratch.write("theirs.rbxmx", &intvalue_model("Counter", 3));
    let out = scratch.path("out.rbxmx");
    let report = scratch.path("conflicts.txt");

    // Step 1: a plain merge conflicts and writes an editable report.
    let first = Command::new(BIN)
        .args(["merge", "--base"])
        .arg(&base)
        .arg("--ours")
        .arg(&ours)
        .arg("--theirs")
        .arg(&theirs)
        .arg("--out")
        .arg(&out)
        .args(["--path", "model.rbxmx", "--conflicts-out"])
        .arg(&report)
        .output()
        .expect("run merge with --conflicts-out");
    assert!(!first.status.success(), "the first merge should conflict");
    let report_text = fs::read_to_string(&report).expect("report written");
    assert!(
        report_text.contains("kind = PropertyValue"),
        "{report_text}"
    );
    assert!(
        report_text.contains("resolution = unresolved"),
        "{report_text}"
    );
    assert!(
        !out.exists(),
        "no output should be written while conflicted"
    );

    // Step 2: the user resolves every conflict in favor of theirs.
    let edited = report_text.replace("resolution = unresolved", "resolution = theirs");
    fs::write(&report, edited).expect("write edited report");

    // Step 3: re-running with the edited report resolves cleanly.
    let second = Command::new(BIN)
        .args(["merge", "--base"])
        .arg(&base)
        .arg("--ours")
        .arg(&ours)
        .arg("--theirs")
        .arg(&theirs)
        .arg("--out")
        .arg(&out)
        .args(["--path", "model.rbxmx", "--resolutions"])
        .arg(&report)
        .output()
        .expect("run merge with --resolutions");
    assert!(
        second.status.success(),
        "resolved merge should succeed, stderr: {}",
        String::from_utf8_lossy(&second.stderr)
    );

    let textconv = Command::new(BIN)
        .arg("textconv")
        .arg(&out)
        .output()
        .expect("run textconv");
    let rendered = String::from_utf8_lossy(&textconv.stdout);
    assert!(
        rendered.contains("Value = Int64(3)"),
        "should resolve to theirs, got:\n{rendered}"
    );
}

#[test]
fn stash_and_resolve_round_trip() {
    let scratch = Scratch::new("stash");
    let base = scratch.write("base.rbxmx", &intvalue_model("Counter", 1));
    let ours = scratch.write("ours.rbxmx", &intvalue_model("Counter", 2));
    let theirs = scratch.write("theirs.rbxmx", &intvalue_model("Counter", 3));
    let out = scratch.path("out.rbxmx");
    let stash = scratch.path("stash");

    // A conflicted merge stashes its inputs and report.
    let first = Command::new(BIN)
        .args(["merge", "--base"])
        .arg(&base)
        .arg("--ours")
        .arg(&ours)
        .arg("--theirs")
        .arg(&theirs)
        .arg("--out")
        .arg(&out)
        .args(["--path", "model.rbxmx", "--stash-dir"])
        .arg(&stash)
        .output()
        .expect("run merge --stash-dir");
    assert!(!first.status.success());
    for name in ["base", "ours", "theirs", "path", "conflicts.txt"] {
        assert!(stash.join(name).exists(), "stash missing {name}");
    }

    // Resolve in favor of theirs, then re-merge from the stash.
    let report = stash.join("conflicts.txt");
    let edited = fs::read_to_string(&report)
        .unwrap()
        .replace("resolution = unresolved", "resolution = theirs");
    fs::write(&report, edited).unwrap();

    let resolved = Command::new(BIN)
        .args(["resolve", "--stash-dir"])
        .arg(&stash)
        .arg("--out")
        .arg(&out)
        .output()
        .expect("run resolve");
    assert!(
        resolved.status.success(),
        "resolve should succeed, stderr: {}",
        String::from_utf8_lossy(&resolved.stderr)
    );

    let textconv = Command::new(BIN)
        .arg("textconv")
        .arg(&out)
        .output()
        .expect("run textconv");
    assert!(
        String::from_utf8_lossy(&textconv.stdout).contains("Value = Int64(3)"),
        "resolved output should take theirs"
    );
}

/// `git init` a scratch repo with isolated global/system config, so `install`
/// and `doctor` neither read nor write the developer's real `~/.gitconfig`
/// (which may already define the `rbxdom` drivers).
fn git_init_isolated(dir: &Path) {
    let status = Command::new("git")
        .args(["init", "-q"])
        .arg(dir)
        .env("GIT_CONFIG_GLOBAL", dir.join("isolated-gitconfig"))
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .status()
        .expect("git init");
    assert!(status.success(), "git init should succeed");
}

/// A `rbx-merge` invocation scoped to `dir` with the same isolated Git config,
/// inherited by the `git` subprocesses that `install`/`doctor` spawn.
fn setup_cmd(dir: &Path) -> Command {
    let mut cmd = Command::new(BIN);
    cmd.current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", dir.join("isolated-gitconfig"))
        .env("GIT_CONFIG_SYSTEM", "/dev/null");
    cmd
}

/// A `git` invocation scoped to `dir` with the same isolated config, for test
/// steps like committing and adding worktrees.
fn git_isolated(dir: &Path) -> Command {
    let mut cmd = Command::new("git");
    cmd.current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", dir.join("isolated-gitconfig"))
        .env("GIT_CONFIG_SYSTEM", "/dev/null");
    cmd
}

#[test]
fn install_then_doctor_reports_healthy() {
    let scratch = Scratch::new("install");
    git_init_isolated(&scratch.dir);

    // Point the driver at the test binary so doctor's PATH probe resolves it.
    let install = setup_cmd(&scratch.dir)
        .args(["install", "--write-gitattributes", "--driver-path"])
        .arg(BIN)
        .output()
        .expect("run install");
    assert!(
        install.status.success(),
        "install should exit 0, stderr: {}",
        String::from_utf8_lossy(&install.stderr)
    );

    // The committed safe default and the per-clone override both landed.
    let committed =
        fs::read_to_string(scratch.path(".gitattributes")).expect("read .gitattributes");
    assert!(committed.contains("*.rbxmx binary"), "got:\n{committed}");
    let override_path = scratch.dir.join(".git/info/attributes");
    let overrides = fs::read_to_string(&override_path).expect("read info/attributes");
    assert!(
        overrides.contains("*.rbxmx merge=rbxdom diff=rbxdom"),
        "got:\n{overrides}"
    );

    let doctor = setup_cmd(&scratch.dir)
        .arg("doctor")
        .output()
        .expect("run doctor");
    let stdout = String::from_utf8_lossy(&doctor.stdout);
    assert!(
        doctor.status.success(),
        "doctor should exit 0 after install, stdout:\n{stdout}"
    );
    assert!(
        stdout.contains("semantic merge active for *.rbxmx"),
        "got:\n{stdout}"
    );
    assert!(
        stdout.contains("marks Roblox files `binary`"),
        "got:\n{stdout}"
    );

    // Re-running install is a no-op that still exits cleanly (idempotent).
    let again = setup_cmd(&scratch.dir)
        .args(["install", "--write-gitattributes", "--driver-path"])
        .arg(BIN)
        .output()
        .expect("run install again");
    assert!(again.status.success());
    assert_eq!(
        fs::read_to_string(&override_path).unwrap(),
        overrides,
        "second install must not change .git/info/attributes"
    );
}

#[test]
fn doctor_flags_uninstalled_repo() {
    let scratch = Scratch::new("uninstalled");
    git_init_isolated(&scratch.dir);

    let doctor = setup_cmd(&scratch.dir)
        .arg("doctor")
        .output()
        .expect("run doctor");
    assert!(
        !doctor.status.success(),
        "doctor should exit non-zero when nothing is installed"
    );
    let stdout = String::from_utf8_lossy(&doctor.stdout);
    assert!(
        stdout.contains("merge driver not configured"),
        "got:\n{stdout}"
    );
    assert!(stdout.contains("would be line-merged"), "got:\n{stdout}");
}

#[test]
fn install_reaches_linked_worktrees() {
    let scratch = Scratch::new("worktree");
    let main = scratch.path("main");
    fs::create_dir_all(&main).expect("create main dir");
    git_init_isolated(&main);
    let committed = git_isolated(&main)
        .args(["-c", "user.email=rbx@example.com", "-c", "user.name=rbx"])
        .args(["commit", "--allow-empty", "-qm", "init"])
        .status()
        .expect("git commit");
    assert!(committed.success(), "commit should succeed");
    let wt = scratch.path("wt");
    let added = git_isolated(&main)
        .args(["worktree", "add", "-q"])
        .arg(&wt)
        .status()
        .expect("git worktree add");
    assert!(added.success(), "worktree add should succeed");

    let install = setup_cmd(&wt)
        .args(["install", "--driver-path"])
        .arg(BIN)
        .output()
        .expect("run install");
    assert!(
        install.status.success(),
        "install should exit 0, stderr: {}",
        String::from_utf8_lossy(&install.stderr)
    );

    // The override must land in the shared common dir — the per-worktree
    // gitdir (.git/worktrees/<name>) is never consulted for info/attributes.
    let shared =
        fs::read_to_string(main.join(".git/info/attributes")).expect("read shared info/attributes");
    assert!(
        shared.contains("*.rbxmx merge=rbxdom diff=rbxdom"),
        "got:\n{shared}"
    );

    let doctor = setup_cmd(&wt).arg("doctor").output().expect("run doctor");
    let stdout = String::from_utf8_lossy(&doctor.stdout);
    assert!(
        doctor.status.success(),
        "doctor should exit 0 in the worktree, stdout:\n{stdout}"
    );
    assert!(
        stdout.contains("semantic merge active for *.rbxmx"),
        "got:\n{stdout}"
    );
}

#[test]
fn install_quotes_spaced_driver_path() {
    let scratch = Scratch::new("spaced");
    git_init_isolated(&scratch.dir);
    let spaced_dir = scratch.dir.join("spaced dir");
    fs::create_dir_all(&spaced_dir).expect("create spaced dir");
    let exe_name = if cfg!(windows) {
        "rbx-merge.exe"
    } else {
        "rbx-merge"
    };
    let spaced_bin = spaced_dir.join(exe_name);
    fs::copy(BIN, &spaced_bin).expect("copy binary into spaced dir");

    let install = setup_cmd(&scratch.dir)
        .args(["install", "--driver-path"])
        .arg(&spaced_bin)
        .output()
        .expect("run install");
    assert!(
        install.status.success(),
        "install should accept a spaced driver path, stderr: {}",
        String::from_utf8_lossy(&install.stderr)
    );

    // The stored command is shell-quoted so Git's sh parses it back whole.
    let config = git_isolated(&scratch.dir)
        .args(["config", "--get", "merge.rbxdom.driver"])
        .output()
        .expect("read merge driver config");
    let driver = String::from_utf8_lossy(&config.stdout);
    assert!(driver.starts_with('\''), "got: {driver}");

    // Doctor un-quotes the executable and finds it runnable.
    let doctor = setup_cmd(&scratch.dir)
        .arg("doctor")
        .output()
        .expect("run doctor");
    let stdout = String::from_utf8_lossy(&doctor.stdout);
    assert!(stdout.contains("executable found"), "got:\n{stdout}");
}

#[test]
fn uninstall_reverts_install() {
    let scratch = Scratch::new("uninst");
    git_init_isolated(&scratch.dir);
    let install = setup_cmd(&scratch.dir)
        .args(["install", "--driver-path"])
        .arg(BIN)
        .output()
        .expect("run install");
    assert!(install.status.success());

    let uninstall = setup_cmd(&scratch.dir)
        .arg("uninstall")
        .output()
        .expect("run uninstall");
    assert!(
        uninstall.status.success(),
        "uninstall should exit 0, stderr: {}",
        String::from_utf8_lossy(&uninstall.stderr)
    );

    let config = git_isolated(&scratch.dir)
        .args(["config", "--get", "merge.rbxdom.driver"])
        .output()
        .expect("read merge driver config");
    assert!(
        !config.status.success(),
        "merge.rbxdom.driver should be removed"
    );
    let attrs = fs::read_to_string(scratch.dir.join(".git/info/attributes")).unwrap_or_default();
    assert!(!attrs.contains("merge=rbxdom"), "got:\n{attrs}");
    let exclude = fs::read_to_string(scratch.dir.join(".git/info/exclude")).unwrap_or_default();
    assert!(
        !exclude.lines().any(|line| line.trim() == ".rbxmerge/"),
        "got:\n{exclude}"
    );
}

#[test]
fn no_stash_install_reverts_stash_pieces() {
    let scratch = Scratch::new("nostash");
    git_init_isolated(&scratch.dir);

    let read_driver = || {
        let config = git_isolated(&scratch.dir)
            .args(["config", "--get", "merge.rbxdom.driver"])
            .output()
            .expect("read merge driver config");
        String::from_utf8_lossy(&config.stdout).into_owned()
    };
    let excludes_rbxmerge = || {
        fs::read_to_string(scratch.dir.join(".git/info/exclude"))
            .unwrap_or_default()
            .lines()
            .any(|line| line.trim() == ".rbxmerge/")
    };

    // The default install uses the stash driver and ignores .rbxmerge/.
    let install = setup_cmd(&scratch.dir)
        .args(["install", "--driver-path"])
        .arg(BIN)
        .output()
        .expect("run install");
    assert!(install.status.success());
    let driver = read_driver();
    assert!(driver.contains("--stash-dir .rbxmerge/%P"), "got: {driver}");
    assert!(
        excludes_rbxmerge(),
        "default install should ignore .rbxmerge/"
    );

    // --no-stash rewrites the plain driver and drops the stale ignore line.
    let plain = setup_cmd(&scratch.dir)
        .args(["install", "--no-stash", "--driver-path"])
        .arg(BIN)
        .output()
        .expect("run install --no-stash");
    assert!(plain.status.success());
    let driver = read_driver();
    assert!(!driver.contains("--stash-dir"), "got: {driver}");
    assert!(
        !excludes_rbxmerge(),
        "--no-stash should remove the stale .rbxmerge/ ignore"
    );
}

#[test]
fn textconv_prints_semantic_text() {
    let scratch = Scratch::new("textconv");
    let model = scratch.write("model.rbxmx", &intvalue_model("Counter", 42));

    let output = Command::new(BIN)
        .arg("textconv")
        .arg(&model)
        .output()
        .expect("run textconv");
    assert!(output.status.success());
    let rendered = String::from_utf8_lossy(&output.stdout);
    assert!(
        rendered.contains("IntValue \"Counter\""),
        "got:\n{rendered}"
    );
    assert!(rendered.contains("Value = Int64(42)"), "got:\n{rendered}");
}
