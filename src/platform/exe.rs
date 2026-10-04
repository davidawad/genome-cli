//! Locating external tools on `PATH` portably (`PATHEXT`/`.exe` on Windows).

use std::path::PathBuf;

/// Full path of `tool` as the OS would resolve it from `PATH`.
pub fn which(tool: &str) -> Option<PathBuf> {
    which_in(tool, std::env::var_os("PATH")?)
}

fn which_in(tool: &str, path: impl AsRef<std::ffi::OsStr>) -> Option<PathBuf> {
    let cwd = std::env::current_dir().ok()?;
    let find = |name: &str| which::which_in(name, Some(&path), &cwd).ok();
    // `which` appends `PATHEXT` suffixes on Windows; an empty environment has none.
    find(tool).or_else(|| if cfg!(windows) { find(&format!("{tool}.exe")) } else { None })
}

/// A one-line install hint for `tool` on this OS.
pub fn install_hint(tool: &str) -> &'static str {
    if cfg!(windows) {
        windows_hint(tool)
    } else if cfg!(target_os = "macos") {
        brew_hint(tool)
    } else {
        linux_hint(tool)
    }
}

fn brew_hint(tool: &str) -> &'static str {
    match tool {
        "minimap2" => "brew install minimap2",
        "bwa-mem2" => "brew install bwa-mem2",
        "samtools" => "brew install samtools",
        "bcftools" => "brew install bcftools",
        "bgzip" | "tabix" => "brew install htslib",
        "docker" => "brew install --cask docker",
        "podman" => "brew install podman",
        "wgsim" => "conda install -c bioconda wgsim (optional; tests simulate reads natively)",
        _ => "see docs/pipeline.md",
    }
}

fn linux_hint(tool: &str) -> &'static str {
    match tool {
        "minimap2" => "apt install minimap2 (or conda install -c bioconda minimap2)",
        "bwa-mem2" => "conda install -c bioconda bwa-mem2",
        "samtools" => "apt install samtools (or conda install -c bioconda samtools)",
        "bcftools" => "apt install bcftools (or conda install -c bioconda bcftools)",
        "bgzip" | "tabix" => "apt install tabix (or conda install -c bioconda htslib)",
        "docker" => "install Docker Engine: https://docs.docker.com/engine/install/",
        "podman" => "apt install podman",
        "wgsim" => "conda install -c bioconda wgsim (optional; tests simulate reads natively)",
        _ => "see docs/pipeline.md",
    }
}

fn windows_hint(tool: &str) -> &'static str {
    match tool {
        "docker" => "install Docker Desktop; the deepvariant caller needs WSL (see docs/pipeline.md)",
        "podman" => "winget install RedHat.Podman; the deepvariant caller needs WSL (see docs/pipeline.md)",
        _ => "no native Windows build: run the pipeline inside WSL (see docs/pipeline.md)",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_executables_on_a_search_path() {
        let d = tempfile::tempdir().unwrap();
        let exe = d.path().join(if cfg!(windows) { "fake-tool.exe" } else { "fake-tool" });
        std::fs::write(&exe, "").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        // Bare names resolve with the platform's executable suffix.
        assert_eq!(which_in("fake-tool", d.path()), Some(exe));
        assert_eq!(which_in("genome-cli-no-such-tool", d.path()), None);
        assert!(!install_hint("samtools").is_empty());
    }
}
