use std::{
    fs,
    path::{Path, PathBuf},
    process::ExitCode,
};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use rbx_merge::{
    Conflict, ConflictKind, Diagnostic, FileInput, MergeSettings, Resolutions, Side, merge_files,
    textconv,
};

#[derive(Debug, Clone, Copy, ValueEnum)]
enum TakeSide {
    Base,
    Ours,
    Theirs,
}

impl From<TakeSide> for Side {
    fn from(side: TakeSide) -> Self {
        match side {
            TakeSide::Base => Side::Base,
            TakeSide::Ours => Side::Ours,
            TakeSide::Theirs => Side::Theirs,
        }
    }
}

#[derive(Debug, Parser)]
#[command(name = "rbx-merge")]
#[command(about = "Semantic diff and three-way merge prototype for Roblox files")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    Textconv {
        path: PathBuf,
    },
    Merge {
        #[arg(long)]
        base: PathBuf,
        #[arg(long)]
        ours: PathBuf,
        #[arg(long)]
        theirs: PathBuf,
        #[arg(long)]
        out: PathBuf,
        #[arg(long)]
        path: Option<PathBuf>,
        /// Resolve every otherwise-unresolved conflict by taking this side.
        #[arg(long, value_enum)]
        take: Option<TakeSide>,
        /// Apply per-conflict choices from an edited conflict report file.
        #[arg(long)]
        resolutions: Option<PathBuf>,
        /// On conflict, write an editable conflict report to this path.
        #[arg(long)]
        conflicts_out: Option<PathBuf>,
        /// On conflict, stash base/ours/theirs and the report in this directory
        /// so the merge can be resolved later with `rbx-merge resolve`. Suitable
        /// for the Git merge driver, which discards its base/theirs temporaries.
        #[arg(long)]
        stash_dir: Option<PathBuf>,
    },
    /// Re-merge a conflict stashed by `merge --stash-dir`, applying the choices
    /// edited into its conflict report, and write the result to `--out`.
    Resolve {
        #[arg(long)]
        stash_dir: PathBuf,
        #[arg(long)]
        out: PathBuf,
    },
    Diff {
        old: PathBuf,
        new: PathBuf,
    },
}

fn main() -> ExitCode {
    env_logger::init();

    match run() {
        Ok(code) => code,
        Err(error) => {
            eprintln!("error: {error:?}");
            ExitCode::from(1)
        }
    }
}

fn run() -> Result<ExitCode> {
    let cli = Cli::parse();
    match cli.command {
        Command::Textconv { path } => {
            let bytes =
                fs::read(&path).with_context(|| format!("failed to read {}", path.display()))?;
            let output = textconv(&bytes, Some(&path))?;
            print!("{output}");
            Ok(ExitCode::SUCCESS)
        }
        Command::Merge {
            base,
            ours,
            theirs,
            out,
            path,
            take,
            resolutions: resolutions_path,
            conflicts_out,
            stash_dir,
        } => {
            let base_bytes =
                fs::read(&base).with_context(|| format!("failed to read {}", base.display()))?;
            let ours_bytes =
                fs::read(&ours).with_context(|| format!("failed to read {}", ours.display()))?;
            let theirs_bytes = fs::read(&theirs)
                .with_context(|| format!("failed to read {}", theirs.display()))?;
            // The in-repo path carries the real extension; the %O/%A/%B temp
            // files passed by Git often do not, so it is the better format hint.
            let hint = path.as_deref().unwrap_or(out.as_path());

            // A bulk --take sets the default; a --resolutions file layers
            // per-conflict overrides on top of it.
            let mut resolutions = match take {
                Some(side) => Resolutions::take(side.into()),
                None => Resolutions::none(),
            };
            if let Some(file) = &resolutions_path {
                let text = fs::read_to_string(file)
                    .with_context(|| format!("failed to read {}", file.display()))?;
                resolutions = parse_resolutions(resolutions, &text).with_context(|| {
                    format!("failed to parse resolutions in {}", file.display())
                })?;
            }

            let report = merge_files(
                FileInput::new(&base_bytes).with_path_hint(hint),
                FileInput::new(&ours_bytes).with_path_hint(hint),
                FileInput::new(&theirs_bytes).with_path_hint(hint),
                MergeSettings {
                    resolutions,
                    ..Default::default()
                },
            )?;

            print_diagnostics(&report.diagnostics);

            match report.merged {
                Some(merged) if report.conflicts.is_empty() => {
                    fs::write(&out, merged)
                        .with_context(|| format!("failed to write {}", out.display()))?;
                    Ok(ExitCode::SUCCESS)
                }
                _ => {
                    print_conflicts(&report.conflicts);
                    if let Some(report_path) = &conflicts_out {
                        fs::write(report_path, write_conflict_report(&report.conflicts))
                            .with_context(|| {
                                format!("failed to write {}", report_path.display())
                            })?;
                        eprintln!(
                            "wrote conflict report to {}; edit `resolution` values and re-run with --resolutions {}",
                            report_path.display(),
                            report_path.display()
                        );
                    }
                    if let Some(dir) = &stash_dir {
                        stash_conflict(
                            dir,
                            &base_bytes,
                            &ours_bytes,
                            &theirs_bytes,
                            hint,
                            &report.conflicts,
                        )?;
                        // Suggest the in-repo path for --out, not Git's %A
                        // temporary, so the resolved file lands where it belongs.
                        eprintln!(
                            "stashed merge inputs to {}; edit {}/conflicts.txt then run: rbx-merge resolve --stash-dir {} --out {}",
                            dir.display(),
                            dir.display(),
                            dir.display(),
                            hint.display(),
                        );
                    }
                    Ok(ExitCode::from(1))
                }
            }
        }
        Command::Resolve { stash_dir, out } => run_resolve(&stash_dir, &out),
        Command::Diff { old, new } => {
            let old_bytes =
                fs::read(&old).with_context(|| format!("failed to read {}", old.display()))?;
            let new_bytes =
                fs::read(&new).with_context(|| format!("failed to read {}", new.display()))?;
            let old_text = textconv(&old_bytes, Some(&old))?;
            let new_text = textconv(&new_bytes, Some(&new))?;
            println!("--- {}", old.display());
            print!("{old_text}");
            if !old_text.ends_with('\n') {
                println!();
            }
            println!("+++ {}", new.display());
            print!("{new_text}");
            Ok(ExitCode::SUCCESS)
        }
    }
}

/// Write the three merge inputs, the path hint, and an editable conflict report
/// into `dir`, so a conflicted merge can be resolved after the caller's
/// temporary files (e.g. Git's %O/%B) are gone.
fn stash_conflict(
    dir: &Path,
    base: &[u8],
    ours: &[u8],
    theirs: &[u8],
    hint: &Path,
    conflicts: &[Conflict],
) -> Result<()> {
    fs::create_dir_all(dir)
        .with_context(|| format!("failed to create stash dir {}", dir.display()))?;
    fs::write(dir.join("base"), base)?;
    fs::write(dir.join("ours"), ours)?;
    fs::write(dir.join("theirs"), theirs)?;
    fs::write(dir.join("path"), hint.to_string_lossy().as_bytes())?;
    fs::write(dir.join("conflicts.txt"), write_conflict_report(conflicts))?;
    Ok(())
}

/// Re-merge a stash, applying the choices edited into its conflict report.
fn run_resolve(dir: &Path, out: &Path) -> Result<ExitCode> {
    let read = |name: &str| {
        fs::read(dir.join(name))
            .with_context(|| format!("failed to read {}", dir.join(name).display()))
    };
    let base = read("base")?;
    let ours = read("ours")?;
    let theirs = read("theirs")?;
    let hint = fs::read_to_string(dir.join("path")).map(PathBuf::from).ok();
    let hint = hint.as_deref().unwrap_or(out);
    let report_text = fs::read_to_string(dir.join("conflicts.txt"))
        .with_context(|| format!("failed to read {}", dir.join("conflicts.txt").display()))?;
    let resolutions = parse_resolutions(Resolutions::none(), &report_text)?;

    let report = merge_files(
        FileInput::new(&base).with_path_hint(hint),
        FileInput::new(&ours).with_path_hint(hint),
        FileInput::new(&theirs).with_path_hint(hint),
        MergeSettings {
            resolutions,
            ..Default::default()
        },
    )?;
    print_diagnostics(&report.diagnostics);

    match report.merged {
        Some(merged) if report.conflicts.is_empty() => {
            fs::write(out, merged).with_context(|| format!("failed to write {}", out.display()))?;
            eprintln!("resolved; wrote {}", out.display());
            Ok(ExitCode::SUCCESS)
        }
        _ => {
            eprintln!("still conflicted after applying resolutions:");
            print_conflicts(&report.conflicts);
            Ok(ExitCode::from(1))
        }
    }
}

/// Render conflicts as an editable report: each block's `resolution` starts at
/// `unresolved` and the user changes it to `ours`, `theirs`, or `base`.
fn write_conflict_report(conflicts: &[Conflict]) -> String {
    let mut out = String::new();
    out.push_str("# rbx-merge conflict report\n");
    out.push_str("# Set `resolution` for each conflict to one of: ours, theirs, base\n");
    out.push_str("# Then re-run the same merge with --resolutions <this file>.\n");
    out.push_str("# For RefTarget and UniqueIdCollision the side is not used: any value\n");
    out.push_str("# applies the fix (drop the dangling reference / the duplicate UniqueId).\n\n");
    for conflict in conflicts {
        out.push_str("[[conflict]]\n");
        out.push_str(&format!("kind = {}\n", kind_name(&conflict.kind)));
        out.push_str(&format!("path = {}\n", conflict.path));
        out.push_str(&format!(
            "property = {}\n",
            conflict.property.as_deref().unwrap_or("<none>")
        ));
        if let Some(value) = &conflict.base {
            out.push_str(&format!("base = {}\n", value.text));
        }
        if let Some(value) = &conflict.ours {
            out.push_str(&format!("ours = {}\n", value.text));
        }
        if let Some(value) = &conflict.theirs {
            out.push_str(&format!("theirs = {}\n", value.text));
        }
        out.push_str("resolution = unresolved\n\n");
    }
    out
}

/// Parse an edited conflict report, layering its per-conflict choices onto
/// `resolutions`. Lines other than `kind`/`path`/`property`/`resolution` (the
/// informational `base`/`ours`/`theirs` values and comments) are ignored.
fn parse_resolutions(mut resolutions: Resolutions, text: &str) -> Result<Resolutions> {
    for block in text.split("[[conflict]]").skip(1) {
        let mut kind = None;
        let mut path = None;
        let mut property = None;
        let mut side = None;
        for line in block.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let value = value.trim();
            match key.trim() {
                "kind" => kind = parse_kind(value),
                "path" => path = Some(value.to_owned()),
                "property" => {
                    property = (value != "<none>" && !value.is_empty()).then(|| value.to_owned())
                }
                "resolution" => side = parse_side(value),
                _ => {}
            }
        }
        if let (Some(kind), Some(path), Some(side)) = (kind, path, side) {
            resolutions = resolutions.resolve(kind, path, property, side);
        }
    }
    Ok(resolutions)
}

fn kind_name(kind: &ConflictKind) -> &'static str {
    match kind {
        ConflictKind::InstanceIdentity => "InstanceIdentity",
        ConflictKind::UniqueIdCollision => "UniqueIdCollision",
        ConflictKind::DeleteModify => "DeleteModify",
        ConflictKind::PropertyValue => "PropertyValue",
        ConflictKind::ParentMove => "ParentMove",
        ConflictKind::ParentCycle => "ParentCycle",
        ConflictKind::ChildOrder => "ChildOrder",
        ConflictKind::RefTarget => "RefTarget",
    }
}

fn parse_kind(value: &str) -> Option<ConflictKind> {
    match value {
        "InstanceIdentity" => Some(ConflictKind::InstanceIdentity),
        "UniqueIdCollision" => Some(ConflictKind::UniqueIdCollision),
        "DeleteModify" => Some(ConflictKind::DeleteModify),
        "PropertyValue" => Some(ConflictKind::PropertyValue),
        "ParentMove" => Some(ConflictKind::ParentMove),
        "ParentCycle" => Some(ConflictKind::ParentCycle),
        "ChildOrder" => Some(ConflictKind::ChildOrder),
        "RefTarget" => Some(ConflictKind::RefTarget),
        _ => None,
    }
}

fn parse_side(value: &str) -> Option<Side> {
    match value {
        "ours" => Some(Side::Ours),
        "theirs" => Some(Side::Theirs),
        "base" => Some(Side::Base),
        _ => None,
    }
}

fn print_diagnostics(diagnostics: &[Diagnostic]) {
    for diagnostic in diagnostics {
        eprintln!(
            "diagnostic {:?} {}: {}{}",
            diagnostic.severity,
            diagnostic.code,
            diagnostic.message,
            diagnostic
                .path
                .as_deref()
                .map(|path| format!(" ({path})"))
                .unwrap_or_default(),
        );
    }
}

fn print_conflicts(conflicts: &[Conflict]) {
    eprintln!("semantic merge conflicts: {}", conflicts.len());
    for conflict in conflicts {
        eprintln!(
            "- kind={:?} path={} class={} name={} property={}",
            conflict.kind,
            conflict.path,
            conflict.class,
            conflict.name,
            conflict.property.as_deref().unwrap_or("<instance>")
        );
        if let Some(value) = &conflict.base {
            eprintln!("  base: {}", value.text);
        }
        if let Some(value) = &conflict.ours {
            eprintln!("  ours: {}", value.text);
        }
        if let Some(value) = &conflict.theirs {
            eprintln!("  theirs: {}", value.text);
        }
    }
}
