use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

static BUILD_KERON: OnceLock<Result<(), String>> = OnceLock::new();

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunResult {
    pub command_line: String,
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
}

impl RunResult {
    #[must_use]
    pub fn transcript(&self) -> String {
        format!(
            "$ {}\n[exit: {}]\n[stdout]\n{}[stderr]\n{}",
            self.command_line, self.exit_code, self.stdout, self.stderr
        )
    }
}

/// Run `keron apply` as an external process.
///
/// `NO_PAGER=1` is always set to keep output deterministic for assertions.
///
/// # Errors
///
/// Returns an error if building/running the `keron` binary fails.
pub fn run_apply(
    root: &Path,
    flags: &[&str],
    env_overrides: &[(String, String)],
) -> Result<RunResult, String> {
    ensure_keron_built()?;
    let bin = keron_bin()?;

    let mut command = Command::new(bin);
    command.env("NO_PAGER", "1");
    command.arg("apply");
    command.arg(root);
    command.args(flags);

    let mut command_parts = vec![
        "keron".to_string(),
        "apply".to_string(),
        root.display().to_string(),
    ];
    command_parts.extend(flags.iter().map(|flag| (*flag).to_string()));

    for (name, value) in env_overrides {
        command.env(name, value);
    }

    let output = command
        .output()
        .map_err(|error| format!("failed to run keron apply: {error}"))?;

    Ok(RunResult {
        command_line: command_parts.join(" "),
        exit_code: output.status.code().unwrap_or(1),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

/// Write a text file, creating parent directories if needed.
///
/// # Errors
///
/// Returns an error if directories or file contents cannot be written.
pub fn write_file(path: &Path, content: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, content)
}

#[must_use]
pub fn to_lua_path(path: &Path) -> String {
    path.to_string_lossy()
        .replace('\\', "/")
        .replace('"', "\\\"")
}

fn ensure_keron_built() -> Result<(), String> {
    match BUILD_KERON.get_or_init(|| {
        let status = Command::new("cargo")
            .arg("build")
            .arg("-q")
            .arg("-p")
            .arg("keron")
            .status()
            .map_err(|error| format!("failed to build keron binary: {error}"))?;

        if status.success() {
            Ok(())
        } else {
            Err(format!(
                "failed to build keron binary: cargo exited with status {status}"
            ))
        }
    }) {
        Ok(()) => Ok(()),
        Err(error) => Err(error.clone()),
    }
}

fn keron_bin() -> Result<PathBuf, String> {
    let mut path = std::env::current_exe()
        .map_err(|error| format!("failed to determine current executable: {error}"))?;
    if !path.pop() {
        return Err("failed to resolve test executable directory".to_string());
    }
    if path.ends_with("deps") {
        let _ = path.pop();
    }
    Ok(path.join(format!("keron{}", std::env::consts::EXE_SUFFIX)))
}
