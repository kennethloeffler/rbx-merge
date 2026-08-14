//! Git integration setup: `rbx-merge install`, `doctor`, and `uninstall`.
//!
//! Git forbids a repository from *defining* the command a merge/diff driver
//! runs (only from *naming* one via committed `.gitattributes`), because that
//! would let a hostile clone execute code. So the driver definitions must live
//! in a config file Git trusts, and the attribute override that activates the
//! semantic driver must live in `.git/info/attributes`, both of which are
//! per-clone and not transferred by `git clone`.
//!
//! The safe posture is therefore:
//!
//! * The committed `.gitattributes` marks the Roblox formats as `binary`. A
//!   collaborator who has *not* run `install` gets a loud conflict that keeps
//!   their own side, never a silent line-merge of structured data.
//! * `install` writes the driver definitions to Git config and a
//!   higher-precedence `*.ext merge=rbxdom diff=rbxdom` override into
//!   `.git/info/attributes`, switching that clone over to the semantic driver.
//! * `doctor` reports whether the current clone is in the safe-but-inactive
//!   state, fully active, or misconfigured.
//! * `uninstall` reverts the per-clone pieces, leaving the committed safe
//!   default in place.

use std::{
    fs,
    ops::Range,
    path::{Path, PathBuf},
    process::{Command, ExitCode},
};

use anyhow::{Context, Result, bail};

/// The Roblox file extensions the drivers apply to.
pub const EXTENSIONS: [&str; 4] = ["rbxl", "rbxlx", "rbxm", "rbxmx"];

/// The XML formats: text files Git would happily (and wrongly) line-merge.
/// The binary formats are at least caught by Git's binary-content heuristic.
const XML_EXTENSIONS: [&str; 2] = ["rbxlx", "rbxmx"];

/// The Git driver name used in both config and attributes (`merge=rbxdom`).
const DRIVER: &str = "rbxdom";

/// Markers delimiting the region rbx-merge manages inside a file it did not
/// create exclusively (`.git/info/attributes`, the repo `.gitattributes`).
/// Re-running `install` rewrites only the text between them.
const BEGIN: &str =
    "# BEGIN rbx-merge (managed) — edit install flags and re-run, or delete to uninstall";
const END: &str = "# END rbx-merge";

/// Options for `rbx-merge install`.
pub struct InstallOptions {
    /// Write the driver definitions to `--global` config instead of this
    /// repo's `--local` config. The `.git/info/attributes` override is always
    /// per-clone regardless, so `install` must still be run once per clone.
    pub global: bool,
    /// The executable name or path to invoke in the driver commands. Defaults
    /// to `rbx-merge` (resolved on PATH). Shell-quoted as needed, so spaced
    /// paths are fine.
    pub driver_path: String,
    /// Use the stash-based merge driver (the default) so conflicts survive Git
    /// discarding its temporaries, and locally ignore the resulting
    /// `.rbxmerge/` directory. Disabled by `--no-stash`.
    pub stash: bool,
    /// Also write the safe `binary` defaults into the repo's committed
    /// `.gitattributes` (creating or updating a managed block).
    pub write_gitattributes: bool,
}

/// Configure the current clone to use the rbx-merge diff and merge drivers.
pub fn install(options: &InstallOptions) -> Result<ExitCode> {
    // Fail early, with Git's own message, when not run inside a repository.
    absolute_git_dir()?;

    if options.driver_path.is_empty() {
        bail!("--driver-path must not be empty");
    }
    // Quoting cannot protect '%': Git expands placeholders across the whole
    // driver command before the shell ever sees it.
    if options.driver_path.contains('%') {
        bail!(
            "--driver-path {:?} contains '%', which Git expands as a placeholder \
             in driver commands; rename or move the executable",
            options.driver_path
        );
    }
    if !runnable(&options.driver_path) {
        eprintln!(
            "warning: {} is not runnable from here — Git will fail to invoke the \
             drivers until the rbx-merge binary is installed (doctor re-checks this)",
            options.driver_path
        );
    }

    let scope = if options.global {
        "--global"
    } else {
        "--local"
    };
    let exe = sh_quote(&options.driver_path);
    set_config(
        scope,
        "merge.rbxdom.name",
        "Roblox semantic merge (rbx-merge)",
    )?;
    set_config(
        scope,
        "merge.rbxdom.driver",
        &merge_driver(&options.driver_path, options.stash),
    )?;
    set_config(scope, "diff.rbxdom.textconv", &format!("{exe} textconv"))?;
    set_config(scope, "diff.rbxdom.cachetextconv", "true")?;
    println!("configured merge.rbxdom / diff.rbxdom in {scope} Git config");

    let attributes = git_path("info/attributes")?;
    if upsert_block(&attributes, &attributes_body())? {
        println!("wrote semantic-driver override to {}", attributes.display());
    } else {
        println!("{} already up to date", attributes.display());
    }

    let exclude = git_path("info/exclude")?;
    if options.stash {
        if append_line_if_missing(&exclude, ".rbxmerge/")? {
            println!("locally ignored .rbxmerge/ via {}", exclude.display());
        }
    } else if remove_line_if_present(&exclude, ".rbxmerge/")? {
        println!("removed stale .rbxmerge/ ignore from {}", exclude.display());
    }

    if options.write_gitattributes {
        let top = top_level()?;
        let gitattributes = top.join(".gitattributes");
        if upsert_block(&gitattributes, &gitattributes_body())? {
            println!(
                "wrote safe `binary` defaults to {} — commit it so collaborators \
                 fail safe until they run `rbx-merge install`",
                gitattributes.display()
            );
        } else {
            println!("{} already up to date", gitattributes.display());
        }
    }

    println!("done — run `rbx-merge doctor` to verify");
    Ok(ExitCode::SUCCESS)
}

/// Remove the driver config and per-clone attribute override written by
/// `install`. The committed `.gitattributes` safe default is left in place.
pub fn uninstall(global: bool) -> Result<ExitCode> {
    absolute_git_dir()?;

    let scope = if global { "--global" } else { "--local" };
    for section in ["merge.rbxdom", "diff.rbxdom"] {
        if git_ok(&["config", scope, "--remove-section", section]).is_some() {
            println!("removed {section} from {scope} Git config");
        } else {
            println!("{section} not present in {scope} Git config");
        }
    }

    let attributes = git_path("info/attributes")?;
    if remove_block(&attributes)? {
        println!(
            "removed semantic-driver override from {}",
            attributes.display()
        );
    }
    let exclude = git_path("info/exclude")?;
    if remove_line_if_present(&exclude, ".rbxmerge/")? {
        println!("removed .rbxmerge/ ignore from {}", exclude.display());
    }

    if let Ok(top) = top_level()
        && fs::read_to_string(top.join(".gitattributes"))
            .unwrap_or_default()
            .contains(BEGIN)
    {
        println!(
            "left the committed .gitattributes safe default in place — remove its \
             managed block manually if you no longer want it"
        );
    }

    println!("done — this clone no longer uses the rbx-merge drivers");
    Ok(ExitCode::SUCCESS)
}

/// Report whether this clone is set up correctly, and exit non-zero if a
/// critical piece is missing or unsafe.
pub fn doctor() -> Result<ExitCode> {
    let mut report = Report::default();

    match git_ok(&["--version"]) {
        Some(version) => report.ok(version.trim_start_matches("git version ")),
        None => {
            report.fail("git not found on PATH");
            report.print();
            return Ok(report.exit_code());
        }
    }

    match absolute_git_dir() {
        Ok(dir) => report.ok(&format!("inside Git repository ({})", dir.display())),
        Err(_) => {
            report.fail("not inside a Git repository — run this from your repo");
            report.print();
            return Ok(report.exit_code());
        }
    }

    // The driver command's executable — parsed from config so we probe the
    // exact binary Git will invoke, falling back to the default name.
    let configured_driver = git_ok(&["config", "--get", "merge.rbxdom.driver"]);
    let exe = configured_driver
        .as_deref()
        .and_then(first_shell_word)
        .unwrap_or_else(|| "rbx-merge".to_owned());
    if runnable(&exe) {
        report.ok(&format!("{exe} executable found"));
    } else {
        report.fail(&format!(
            "{exe} not runnable — install the binary or fix --driver-path"
        ));
    }

    match &configured_driver {
        Some(_) => report.ok("merge driver configured (merge.rbxdom.driver)"),
        None => report.fail("merge driver not configured — run `rbx-merge install`"),
    }
    match git_ok(&["config", "--get", "diff.rbxdom.textconv"]) {
        Some(_) => report.ok("diff textconv configured (diff.rbxdom.textconv)"),
        None => report.warn("diff textconv not configured — semantic diffs disabled"),
    }

    // The decisive check: does Git actually resolve `merge=rbxdom` for these
    // paths? This reflects real attribute precedence, so it catches a clone
    // that has the config but is missing the `.git/info/attributes` override.
    for ext in EXTENSIONS {
        let probe = format!("probe.{ext}");
        let attrs = git_ok(&["check-attr", "merge", "diff", "--", &probe]).unwrap_or_default();
        match attr_value(&attrs, "merge").as_deref() {
            Some(value) if value == DRIVER => {
                report.ok(&format!("semantic merge active for *.{ext}"));
            }
            // `binary` sets -merge -> "unset": safe (Git refuses line-merge)
            // but the semantic driver is not running.
            Some("unset") => report.warn(&format!(
                "*.{ext} falls back to binary (safe, but semantic merge inactive) — run `rbx-merge install`"
            )),
            Some(other) if other != "unspecified" => report.fail(&format!(
                "*.{ext} is assigned merge driver `{other}`, not `{DRIVER}` — run `rbx-merge install`"
            )),
            _ if XML_EXTENSIONS.contains(&ext) => report.fail(&format!(
                "*.{ext} would be line-merged by Git (silent corruption risk) — run `rbx-merge install`"
            )),
            _ => report.fail(&format!(
                "*.{ext} has no merge attribute — only Git's binary-content heuristic \
                 prevents a line merge — run `rbx-merge install`"
            )),
        }
    }

    check_committed_gitattributes(&mut report);

    report.print();
    Ok(report.exit_code())
}

/// Warn about a committed `.gitattributes` whose posture is unsafe or missing.
fn check_committed_gitattributes(report: &mut Report) {
    let Ok(top) = top_level() else { return };
    let contents = fs::read_to_string(top.join(".gitattributes")).unwrap_or_default();
    let postures: Vec<(&str, ExtPosture)> = EXTENSIONS
        .iter()
        .map(|&ext| (ext, ext_posture(&contents, ext)))
        .collect();

    if postures.iter().any(|(_, p)| *p == ExtPosture::Driver) {
        report.warn(
            "committed .gitattributes names merge=rbxdom directly — collaborators who \
             have not run `install` will silently line-merge; prefer the `binary` default",
        );
    } else if postures.iter().all(|(_, p)| *p == ExtPosture::Safe) {
        report.ok("committed .gitattributes marks Roblox files `binary` (safe default)");
    } else if postures.iter().all(|(_, p)| *p == ExtPosture::Unmentioned) {
        report.warn(
            "no committed .gitattributes for Roblox files — run \
             `rbx-merge install --write-gitattributes` and commit it",
        );
    } else {
        let unprotected: Vec<String> = postures
            .iter()
            .filter(|(_, p)| *p != ExtPosture::Safe)
            .map(|(ext, _)| format!("*.{ext}"))
            .collect();
        report.warn(&format!(
            "committed .gitattributes leaves {} without a safe default — re-run \
             `rbx-merge install --write-gitattributes`",
            unprotected.join(", ")
        ));
    }
}

/// The merge posture a `.gitattributes` file leaves one extension in.
#[derive(Debug, Clone, Copy, PartialEq)]
enum ExtPosture {
    /// No rule mentions the extension.
    Unmentioned,
    /// `binary` or `-merge`: Git refuses to line-merge.
    Safe,
    /// Names `merge=rbxdom` directly (unsafe for un-installed clones).
    Driver,
    /// Names some other merge driver.
    Other,
}

/// Parse the final merge posture for `*.{ext}`, honoring gitattributes
/// semantics: `#` lines are comments, fields are whitespace-separated, and
/// the last matching rule wins.
fn ext_posture(contents: &str, ext: &str) -> ExtPosture {
    let pattern = format!("*.{ext}");
    let driver_attr = format!("merge={DRIVER}");
    let mut state = ExtPosture::Unmentioned;
    for line in contents.lines() {
        let line = line.trim();
        if line.starts_with('#') {
            continue;
        }
        let mut fields = line.split_whitespace();
        if fields.next() != Some(pattern.as_str()) {
            continue;
        }
        for attr in fields {
            state = match attr {
                "binary" | "-merge" => ExtPosture::Safe,
                _ if attr == driver_attr => ExtPosture::Driver,
                _ if attr.starts_with("merge=") => ExtPosture::Other,
                _ => state,
            };
        }
    }
    state
}

/// The value of `attr` in `git check-attr` output (`<path>: <attr>: <value>`).
fn attr_value(output: &str, attr: &str) -> Option<String> {
    output.lines().find_map(|line| {
        let mut fields = line.rsplitn(3, ": ");
        let value = fields.next()?;
        (fields.next()? == attr).then(|| value.to_owned())
    })
}

/// Build the merge driver command line stored in config.
fn merge_driver(driver_path: &str, stash: bool) -> String {
    let exe = sh_quote(driver_path);
    let base = format!("{exe} merge --base %O --ours %A --theirs %B --out %A --path %P");
    if stash {
        format!("{base} --stash-dir .rbxmerge/%P")
    } else {
        base
    }
}

/// Quote `path` for the `sh` command line Git runs driver commands with.
/// Simple paths pass through untouched so the common config stays readable;
/// anything else (spaces, backslashes, quotes) is single-quoted. `~` is left
/// bare so a literal `~/bin/rbx-merge` still tilde-expands.
fn sh_quote(path: &str) -> String {
    let simple = !path.is_empty()
        && path.chars().all(|c| {
            c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '/' | '+' | ':' | '@' | '~')
        });
    if simple {
        path.to_owned()
    } else {
        format!("'{}'", path.replace('\'', r"'\''"))
    }
}

/// Best-effort parse of the first `sh` word of a driver command — the inverse
/// of [`sh_quote`], tolerating hand-written double quotes and escapes too.
fn first_shell_word(command: &str) -> Option<String> {
    let mut word = String::new();
    let mut chars = command.trim_start().chars();
    let mut quote: Option<char> = None;
    while let Some(c) = chars.next() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => word.push(c),
            None => match c {
                '\'' | '"' => quote = Some(c),
                '\\' => {
                    if let Some(escaped) = chars.next() {
                        word.push(escaped);
                    }
                }
                c if c.is_whitespace() => break,
                c => word.push(c),
            },
        }
    }
    (!word.is_empty()).then_some(word)
}

/// The body written into `.git/info/attributes` to activate the drivers.
fn attributes_body() -> String {
    let mut body = String::from(
        "# Higher precedence than the committed .gitattributes; enables the\n\
         # semantic diff and merge drivers for this clone.\n",
    );
    for ext in EXTENSIONS {
        body.push_str(&format!("*.{ext} merge={DRIVER} diff={DRIVER}\n"));
    }
    body.pop(); // upsert_block re-adds the trailing newline
    body
}

/// The body written into the committed `.gitattributes` as the safe default.
fn gitattributes_body() -> String {
    let mut body = String::from(
        "# Safe default: a clone that has not run `rbx-merge install` treats these\n\
         # as binary, so Git refuses to line-merge them (conflict + keep ours)\n\
         # instead of silently corrupting structured Roblox files. `install`\n\
         # writes a higher-precedence override into .git/info/attributes.\n",
    );
    for ext in EXTENSIONS {
        body.push_str(&format!("*.{ext} binary\n"));
    }
    body.pop();
    body
}

/// Locate the managed block in `existing`, including its trailing newline.
/// Errors when only one marker is present (or they are out of order) rather
/// than guessing and mangling the user's file.
fn find_block(existing: &str) -> Result<Option<Range<usize>>> {
    match (existing.find(BEGIN), existing.find(END)) {
        (None, None) => Ok(None),
        (Some(start), Some(end_start)) if end_start >= start => {
            let mut end = end_start + END.len();
            if existing.as_bytes().get(end) == Some(&b'\n') {
                end += 1;
            }
            Ok(Some(start..end))
        }
        _ => bail!(
            "managed rbx-merge block markers are unbalanced — restore or delete the \
             `{BEGIN}` / `{END}` lines and re-run"
        ),
    }
}

/// Insert or replace the marker-delimited managed block in `path`, preserving
/// any surrounding content. Returns whether the file changed.
fn upsert_block(path: &Path, body: &str) -> Result<bool> {
    let existing = fs::read_to_string(path).unwrap_or_default();
    let block = format!("{BEGIN}\n{body}\n{END}\n");

    let updated = match find_block(&existing).with_context(|| format!("in {}", path.display()))? {
        Some(range) => format!(
            "{}{}{}",
            &existing[..range.start],
            block,
            &existing[range.end..]
        ),
        None => append_block(&existing, &block),
    };

    if updated == existing {
        return Ok(false);
    }
    write_with_parents(path, &updated)?;
    Ok(true)
}

/// Delete the managed block from `path` if present. Returns whether the file
/// changed.
fn remove_block(path: &Path) -> Result<bool> {
    let Ok(existing) = fs::read_to_string(path) else {
        return Ok(false);
    };
    let Some(range) = find_block(&existing).with_context(|| format!("in {}", path.display()))?
    else {
        return Ok(false);
    };
    let updated = format!("{}{}", &existing[..range.start], &existing[range.end..]);
    fs::write(path, updated).with_context(|| format!("failed to write {}", path.display()))?;
    Ok(true)
}

fn append_block(existing: &str, block: &str) -> String {
    if existing.is_empty() {
        block.to_owned()
    } else if existing.ends_with('\n') {
        format!("{existing}{block}")
    } else {
        format!("{existing}\n{block}")
    }
}

/// Append `line` to `path` unless it is already present. Returns whether the
/// file changed.
fn append_line_if_missing(path: &Path, line: &str) -> Result<bool> {
    let existing = fs::read_to_string(path).unwrap_or_default();
    if existing.lines().any(|l| l.trim() == line) {
        return Ok(false);
    }
    let updated = append_block(&existing, &format!("{line}\n"));
    write_with_parents(path, &updated)?;
    Ok(true)
}

/// Remove every line equal to `line` from `path`. Returns whether the file
/// changed.
fn remove_line_if_present(path: &Path, line: &str) -> Result<bool> {
    let Ok(existing) = fs::read_to_string(path) else {
        return Ok(false);
    };
    if !existing.lines().any(|l| l.trim() == line) {
        return Ok(false);
    }
    let updated: String = existing
        .lines()
        .filter(|l| l.trim() != line)
        .map(|l| format!("{l}\n"))
        .collect();
    fs::write(path, updated).with_context(|| format!("failed to write {}", path.display()))?;
    Ok(true)
}

fn write_with_parents(path: &Path, contents: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    fs::write(path, contents).with_context(|| format!("failed to write {}", path.display()))
}

/// `git config <scope> <key> <value>`.
fn set_config(scope: &str, key: &str, value: &str) -> Result<()> {
    git(&["config", scope, key, value]).map(drop)
}

/// The absolute path to the current repository's `.git` directory.
fn absolute_git_dir() -> Result<PathBuf> {
    git(&["rev-parse", "--absolute-git-dir"]).map(PathBuf::from)
}

/// A path inside the Git directory, resolved by Git itself so files shared
/// between worktrees (`info/attributes`, `info/exclude`) land in the common
/// dir Git actually reads them from — `--absolute-git-dir` would point at the
/// per-worktree directory, which Git ignores for these files.
fn git_path(relative: &str) -> Result<PathBuf> {
    let path = git(&["rev-parse", "--git-path", relative])?;
    std::path::absolute(&path).with_context(|| format!("failed to resolve {path}"))
}

/// The absolute path to the working tree's top level.
fn top_level() -> Result<PathBuf> {
    git(&["rev-parse", "--show-toplevel"]).map(PathBuf::from)
}

/// Run `git` and return trimmed stdout, erroring on non-zero exit.
fn git(args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .args(args)
        .output()
        .context("failed to run git; is it installed and on PATH?")?;
    if !output.status.success() {
        bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// Run `git` and return trimmed stdout, or `None` on any failure.
fn git_ok(args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// Whether `exe --version` can be spawned (i.e. the binary is on PATH).
fn runnable(exe: &str) -> bool {
    Command::new(exe)
        .arg("--version")
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}

/// Accumulates human-readable check results and the worst status seen.
#[derive(Default)]
struct Report {
    lines: Vec<String>,
    failed: bool,
}

impl Report {
    fn ok(&mut self, message: &str) {
        self.lines.push(format!("[ ok ] {message}"));
    }

    fn warn(&mut self, message: &str) {
        self.lines.push(format!("[warn] {message}"));
    }

    fn fail(&mut self, message: &str) {
        self.failed = true;
        self.lines.push(format!("[FAIL] {message}"));
    }

    fn print(&self) {
        for line in &self.lines {
            println!("{line}");
        }
    }

    fn exit_code(&self) -> ExitCode {
        if self.failed {
            ExitCode::from(1)
        } else {
            ExitCode::SUCCESS
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_driver_variants() {
        assert_eq!(
            merge_driver("rbx-merge", false),
            "rbx-merge merge --base %O --ours %A --theirs %B --out %A --path %P"
        );
        assert!(merge_driver("rbx-merge", true).ends_with("--stash-dir .rbxmerge/%P"));
    }

    #[test]
    fn sh_quote_round_trips_through_first_shell_word() {
        assert_eq!(sh_quote("rbx-merge"), "rbx-merge");
        assert_eq!(sh_quote("~/bin/rbx-merge"), "~/bin/rbx-merge");
        assert_eq!(sh_quote("/opt/rbx merge/bin"), "'/opt/rbx merge/bin'");
        assert_eq!(sh_quote("a'b"), r"'a'\''b'");
        assert_eq!(sh_quote(r"C:\bin\rbx.exe"), r"'C:\bin\rbx.exe'");

        for path in ["rbx-merge", "/opt/rbx merge/bin", "a'b", r"C:\bin\rbx.exe"] {
            let command = format!("{} textconv", sh_quote(path));
            assert_eq!(first_shell_word(&command).as_deref(), Some(path));
        }
    }

    #[test]
    fn first_shell_word_handles_quoting() {
        assert_eq!(
            first_shell_word("rbx-merge textconv").as_deref(),
            Some("rbx-merge")
        );
        assert_eq!(
            first_shell_word(r"'/a'\''b/x' merge").as_deref(),
            Some("/a'b/x")
        );
        assert_eq!(
            first_shell_word(r#""C:\Program Files\rbx.exe" merge"#).as_deref(),
            Some(r"C:\Program Files\rbx.exe")
        );
        assert_eq!(first_shell_word(""), None);
    }

    #[test]
    fn attr_value_parses_check_attr_output() {
        let output = "probe.rbxl: merge: rbxdom\nprobe.rbxl: diff: unspecified";
        assert_eq!(attr_value(output, "merge").as_deref(), Some("rbxdom"));
        assert_eq!(attr_value(output, "diff").as_deref(), Some("unspecified"));
        assert_eq!(attr_value(output, "text"), None);
        // No substring false-positives: `rbxdom2` is not our driver.
        assert_eq!(
            attr_value("p.rbxl: merge: rbxdom2", "merge").as_deref(),
            Some("rbxdom2")
        );
    }

    #[test]
    fn ext_posture_parses_gitattributes() {
        // Tabs and column alignment count as whitespace.
        assert_eq!(ext_posture("*.rbxl\t\tbinary", "rbxl"), ExtPosture::Safe);
        assert_eq!(ext_posture("*.rbxl   -merge", "rbxl"), ExtPosture::Safe);
        // Comments are ignored.
        assert_eq!(
            ext_posture("# *.rbxl binary", "rbxl"),
            ExtPosture::Unmentioned
        );
        // Naming the driver directly is flagged.
        assert_eq!(
            ext_posture("*.rbxl merge=rbxdom", "rbxl"),
            ExtPosture::Driver
        );
        assert_eq!(
            ext_posture("*.rbxl merge=custom", "rbxl"),
            ExtPosture::Other
        );
        // The last matching rule wins, as in Git.
        assert_eq!(
            ext_posture("*.rbxl merge=rbxdom\n*.rbxl binary", "rbxl"),
            ExtPosture::Safe
        );
        // Other extensions' rules do not leak.
        assert_eq!(
            ext_posture("*.rbxl binary", "rbxlx"),
            ExtPosture::Unmentioned
        );
    }

    #[test]
    fn attributes_body_covers_every_extension() {
        let body = attributes_body();
        for ext in EXTENSIONS {
            assert!(body.contains(&format!("*.{ext} merge={DRIVER} diff={DRIVER}")));
        }
    }

    #[test]
    fn gitattributes_body_is_binary() {
        let body = gitattributes_body();
        for ext in EXTENSIONS {
            assert!(body.contains(&format!("*.{ext} binary")));
        }
    }

    #[test]
    fn upsert_is_idempotent_and_preserves_surroundings() {
        let dir = std::env::temp_dir().join(format!("rbxmerge-upsert-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("attributes");
        fs::write(&path, "*.txt text\n").unwrap();

        assert!(upsert_block(&path, "*.rbxl merge=rbxdom").unwrap());
        let first = fs::read_to_string(&path).unwrap();
        assert!(first.starts_with("*.txt text\n"));
        assert!(first.contains("*.rbxl merge=rbxdom"));

        // Re-running with the same body is a no-op.
        assert!(!upsert_block(&path, "*.rbxl merge=rbxdom").unwrap());

        // A changed body replaces only the managed block.
        assert!(upsert_block(&path, "*.rbxm merge=rbxdom").unwrap());
        let second = fs::read_to_string(&path).unwrap();
        assert!(second.starts_with("*.txt text\n"));
        assert!(second.contains("*.rbxm merge=rbxdom"));
        assert!(!second.contains("*.rbxl merge=rbxdom"));
        assert_eq!(second.matches(BEGIN).count(), 1);

        // Removing the block restores the original file exactly.
        assert!(remove_block(&path).unwrap());
        assert_eq!(fs::read_to_string(&path).unwrap(), "*.txt text\n");
        assert!(!remove_block(&path).unwrap());

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn unbalanced_markers_error_instead_of_mangling() {
        let dir = std::env::temp_dir().join(format!("rbxmerge-unbalanced-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("attributes");
        fs::write(&path, format!("{BEGIN}\n# user deleted the END line\n")).unwrap();

        assert!(upsert_block(&path, "*.rbxl merge=rbxdom").is_err());
        assert!(remove_block(&path).is_err());

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn append_line_if_missing_dedupes() {
        let dir = std::env::temp_dir().join(format!("rbxmerge-exclude-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("exclude");

        assert!(append_line_if_missing(&path, ".rbxmerge/").unwrap());
        assert!(!append_line_if_missing(&path, ".rbxmerge/").unwrap());
        assert_eq!(
            fs::read_to_string(&path)
                .unwrap()
                .matches(".rbxmerge/")
                .count(),
            1
        );

        assert!(remove_line_if_present(&path, ".rbxmerge/").unwrap());
        assert!(!remove_line_if_present(&path, ".rbxmerge/").unwrap());
        assert!(!fs::read_to_string(&path).unwrap().contains(".rbxmerge/"));

        fs::remove_dir_all(&dir).ok();
    }
}
