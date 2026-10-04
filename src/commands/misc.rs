use clap::CommandFactory;
use clap_complete::Shell as CShell;

use crate::cli::{Cli, CompletionsArgs, ManArgs, Shell};
use crate::error::{AppError, Result};

pub fn completions(a: CompletionsArgs) -> Result<()> {
    let mut cmd = Cli::command();
    let out = &mut std::io::stdout();
    match a.shell {
        Shell::Bash => clap_complete::generate(CShell::Bash, &mut cmd, "genome", out),
        Shell::Zsh => clap_complete::generate(CShell::Zsh, &mut cmd, "genome", out),
        Shell::Fish => clap_complete::generate(CShell::Fish, &mut cmd, "genome", out),
        Shell::Elvish => clap_complete::generate(CShell::Elvish, &mut cmd, "genome", out),
        Shell::Powershell => clap_complete::generate(CShell::PowerShell, &mut cmd, "genome", out),
        Shell::Nushell => clap_complete::generate(clap_complete_nushell::Nushell, &mut cmd, "genome", out),
    }
    Ok(())
}

fn render(cmd: clap::Command) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    clap_mangen::Man::new(cmd).render(&mut buf).map_err(|e| AppError::io(e.to_string()))?;
    Ok(buf)
}

/// Collect (page name, command) for a command and all its subcommands.
fn pages(prefix: &str, cmd: &clap::Command) -> Vec<(String, clap::Command)> {
    let name = if prefix.is_empty() { cmd.get_name().to_string() } else { format!("{prefix}-{}", cmd.get_name()) };
    std::iter::once((name.clone(), cmd.clone().name(name.clone())))
        .chain(cmd.get_subcommands().filter(|s| s.get_name() != "help").flat_map(|s| pages(&name, s)))
        .collect()
}

pub fn man(a: ManArgs) -> Result<()> {
    let mut cmd = Cli::command();
    cmd.build();
    match a.dir {
        None => {
            use std::io::Write;
            std::io::stdout().write_all(&render(cmd)?).map_err(AppError::from)
        }
        Some(dir) => {
            std::fs::create_dir_all(&dir)?;
            pages("", &cmd).into_iter().try_for_each(|(name, c)| {
                let path = dir.join(format!("{name}.1"));
                std::fs::write(&path, render(c)?).map_err(|e| AppError::io(format!("{}: {e}", path.display())))
            })?;
            eprintln!("wrote man pages to {}", dir.display());
            Ok(())
        }
    }
}
