//! `Plan` — the diffable, renderable description of what `apply` will
//! do. The evaluator that turns a parsed program into concrete
//! `ResourceState` values is not yet implemented; `build_plan` is the
//! single deferred seam.
//!
//! Variants here are constructed by the (yet-to-land) evaluator and
//! by `Plan::sample()` under `cfg(test)`, so the non-test build sees
//! them as unconstructed today.

#![allow(dead_code, clippy::redundant_pub_crate)]

use std::path::PathBuf;

use anyhow::Result;

use crate::eval;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Action {
    Create,
    Update,
    Destroy,
    NoOp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResourceKind {
    File,
    Directory,
    Symlink,
}

impl ResourceKind {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::File => "file",
            Self::Directory => "directory",
            Self::Symlink => "symlink",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ResourceState {
    File { path: PathBuf, content: String },
    Directory { path: PathBuf },
    Symlink { from: PathBuf, to: PathBuf },
}

#[derive(Debug, Clone)]
pub(crate) struct ResourceChange {
    pub(crate) address: String,
    pub(crate) kind: ResourceKind,
    pub(crate) action: Action,
    pub(crate) before: Option<ResourceState>,
    pub(crate) after: Option<ResourceState>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct Plan {
    pub(crate) changes: Vec<ResourceChange>,
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct PlanSummary {
    pub(crate) add: usize,
    pub(crate) change: usize,
    pub(crate) destroy: usize,
}

impl Plan {
    pub(crate) fn summary(&self) -> PlanSummary {
        let mut s = PlanSummary::default();
        for c in &self.changes {
            match c.action {
                Action::Create => s.add += 1,
                Action::Update => s.change += 1,
                Action::Destroy => s.destroy += 1,
                Action::NoOp => {}
            }
        }
        s
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.changes
            .iter()
            .all(|c| matches!(c.action, Action::NoOp))
    }
}

/// Build a `Plan` from a parsed and type-checked program.
///
/// Today every resource is reported as `Action::Create`. Diffing
/// against live filesystem state — to refine into Update/NoOp/Destroy
/// — lands in a follow-up.
pub(crate) fn build_plan(program: &keron_lang::Program) -> Result<Plan> {
    let resources = eval::eval_program(program)?;
    let changes = resources
        .into_iter()
        .map(|state| ResourceChange {
            address: address_for(&state),
            kind: kind_for(&state),
            action: Action::Create,
            before: None,
            after: Some(state),
        })
        .collect();
    Ok(Plan { changes })
}

fn address_for(state: &ResourceState) -> String {
    match state {
        ResourceState::File { path, .. } | ResourceState::Directory { path } => {
            path.display().to_string()
        }
        ResourceState::Symlink { from, .. } => from.display().to_string(),
    }
}

const fn kind_for(state: &ResourceState) -> ResourceKind {
    match state {
        ResourceState::File { .. } => ResourceKind::File,
        ResourceState::Directory { .. } => ResourceKind::Directory,
        ResourceState::Symlink { .. } => ResourceKind::Symlink,
    }
}

#[cfg(test)]
impl Plan {
    pub(crate) fn sample() -> Self {
        Self {
            changes: vec![
                ResourceChange {
                    address: "~/.zshrc".into(),
                    kind: ResourceKind::File,
                    action: Action::Create,
                    before: None,
                    after: Some(ResourceState::File {
                        path: PathBuf::from("~/.zshrc"),
                        content: "export PATH=...".into(),
                    }),
                },
                ResourceChange {
                    address: "~/.config/nvim".into(),
                    kind: ResourceKind::Symlink,
                    action: Action::Update,
                    before: Some(ResourceState::Symlink {
                        from: PathBuf::from("~/.config/nvim"),
                        to: PathBuf::from("/old/target"),
                    }),
                    after: Some(ResourceState::Symlink {
                        from: PathBuf::from("~/.config/nvim"),
                        to: PathBuf::from("/new/target"),
                    }),
                },
                ResourceChange {
                    address: "/tmp/scratch".into(),
                    kind: ResourceKind::Directory,
                    action: Action::Destroy,
                    before: Some(ResourceState::Directory {
                        path: PathBuf::from("/tmp/scratch"),
                    }),
                    after: None,
                },
            ],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summary_counts_each_action() {
        let plan = Plan::sample();
        let s = plan.summary();
        assert_eq!(s.add, 1);
        assert_eq!(s.change, 1);
        assert_eq!(s.destroy, 1);
    }

    #[test]
    fn is_empty_only_when_all_noop() {
        assert!(Plan::default().is_empty());
        let only_noop = Plan {
            changes: vec![ResourceChange {
                address: "x".into(),
                kind: ResourceKind::File,
                action: Action::NoOp,
                before: None,
                after: None,
            }],
        };
        assert!(only_noop.is_empty());
        assert!(!Plan::sample().is_empty());
    }
}
