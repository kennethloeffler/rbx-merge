use std::{fs, path::PathBuf, process::ExitCode};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use rbx_merge::{Conflict, MergeInput, MergeOptions, MergeResult, merge, textconv};

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
            let path_hint = Some(path.as_deref().unwrap_or(out.as_path()));

            match merge(
                MergeInput {
                    base: &base_bytes,
                    ours: &ours_bytes,
                    theirs: &theirs_bytes,
                    path_hint,
                },
                MergeOptions::default(),
            )? {
                MergeResult::Clean {
                    merged,
                    diagnostics,
                } => {
                    for diagnostic in diagnostics {
                        eprintln!(
                            "diagnostic {:?} {}: {}",
                            diagnostic.severity, diagnostic.code, diagnostic.message
                        );
                    }
                    fs::write(&out, merged)
                        .with_context(|| format!("failed to write {}", out.display()))?;
                    Ok(ExitCode::SUCCESS)
                }
                MergeResult::Conflicted {
                    conflicts,
                    diagnostics,
                } => {
                    for diagnostic in diagnostics {
                        eprintln!(
                            "diagnostic {:?} {}: {}",
                            diagnostic.severity, diagnostic.code, diagnostic.message
                        );
                    }
                    print_conflicts(&conflicts);
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
