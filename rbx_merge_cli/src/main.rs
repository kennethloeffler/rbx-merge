use std::{fs, path::PathBuf, process::ExitCode};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use rbx_merge::{Conflict, Diagnostic, FileInput, MergeSettings, merge_files, textconv};

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

            let report = merge_files(
                FileInput::new(&base_bytes).with_path_hint(hint),
                FileInput::new(&ours_bytes).with_path_hint(hint),
                FileInput::new(&theirs_bytes).with_path_hint(hint),
                MergeSettings::default(),
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
                    Ok(ExitCode::from(1))
                }
            }
        }
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
