//! Corpus harness: walks fixtures, classifies them by stage, and produces
//! sidecar snapshots via insta.

mod render;
mod stage;

use std::{
    path::{Path, PathBuf},
    sync::LazyLock,
};

use libtest_mimic::{Failed, Trial};

pub use stage::Stage;

pub static CORPUS_ROOT: LazyLock<PathBuf> =
    LazyLock::new(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/corpus"));

pub fn collect_trials(root: &Path) -> Vec<Trial> {
    let mut trials = Vec::new();
    walk(root, root, &mut trials);
    trials.sort_by(|a, b| a.name().cmp(b.name()));
    trials
}

fn walk(root: &Path, dir: &Path, out: &mut Vec<Trial>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk(root, &path, out);
        } else if path.extension().is_some_and(|e| e == "keron")
            && let Some(trial) = build_trial(root, path)
        {
            out.push(trial);
        }
    }
}

fn build_trial(root: &Path, path: PathBuf) -> Option<Trial> {
    let stage = Stage::from_path(root, &path)?;
    let name = trial_name(root, &path);
    Some(Trial::test(name, move || run_case(&path, stage)))
}

fn trial_name(root: &Path, path: &Path) -> String {
    let rel = path.strip_prefix(root).unwrap_or(path);
    let mut name = rel.with_extension("").to_string_lossy().into_owned();
    if cfg!(windows) {
        name = name.replace('\\', "/");
    }
    name
}

fn run_case(path: &Path, stage: Stage) -> Result<(), Failed> {
    let src = std::fs::read_to_string(path).map_err(|e| Failed::from(e.to_string()))?;
    let snapshot = stage.run(&src);
    let snapshot_name = path
        .file_stem()
        .and_then(|s| s.to_str())
        .ok_or_else(|| Failed::from("invalid fixture filename"))?
        .to_string();
    let snapshot_dir = path
        .parent()
        .ok_or_else(|| Failed::from("fixture has no parent"))?
        .to_path_buf();

    let mut settings = insta::Settings::clone_current();
    settings.set_snapshot_path(snapshot_dir);
    settings.set_prepend_module_to_snapshot(false);
    settings.set_omit_expression(true);
    settings.bind(|| {
        insta::assert_snapshot!(snapshot_name, snapshot);
    });
    Ok(())
}
