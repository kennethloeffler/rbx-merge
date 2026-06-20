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
    assert!(report_text.contains("kind = PropertyValue"), "{report_text}");
    assert!(report_text.contains("resolution = unresolved"), "{report_text}");
    assert!(!out.exists(), "no output should be written while conflicted");

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
    assert!(rendered.contains("IntValue \"Counter\""), "got:\n{rendered}");
    assert!(rendered.contains("Value = Int64(42)"), "got:\n{rendered}");
}
