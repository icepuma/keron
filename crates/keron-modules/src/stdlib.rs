//! Rust-level stdlib registry.
//!
//! Stdlib modules are not authored as `.keron` source — they are
//! synthesized at runtime from data in this module. Each entry
//! produces a [`StdModule`] carrying `FnDecl`s that the type checker
//! sees as ordinary functions (with signatures only) and that the
//! evaluator dispatches via the [`IntrinsicId`] tag rather than by
//! name.

use std::collections::BTreeMap;
use std::sync::OnceLock;

use keron_lang::{Block, FnDecl, IntrinsicId, Item, Param, Program, Spanned, Type};

#[derive(Debug)]
pub struct StdModule {
    /// Stored in a [`BTreeMap`] so [`Self::synth_program`] yields a
    /// deterministic, alphabetically-ordered `Program` without needing
    /// an explicit sort step.
    pub fns: BTreeMap<String, FnDecl>,
}

impl StdModule {
    /// Synthesize a [`Program`] AST so the standard module pipeline
    /// (type-check + graph insertion) treats this stdlib module
    /// uniformly with user modules.
    #[must_use]
    pub fn synth_program(&self) -> Program {
        let items: Vec<Item> = self.fns.values().cloned().map(Item::Fn).collect();
        Program { items }
    }
}

/// Process-wide stdlib registry. Modules are keyed by the substring
/// after `std:` in import paths (so `from "std:fs" use ...` looks up
/// `"fs"`).
#[must_use]
pub fn registry() -> &'static BTreeMap<&'static str, StdModule> {
    static REG: OnceLock<BTreeMap<&'static str, StdModule>> = OnceLock::new();
    REG.get_or_init(build_registry)
}

fn build_registry() -> BTreeMap<&'static str, StdModule> {
    let mut reg = BTreeMap::new();
    reg.insert("fs", build_fs());
    reg
}

fn build_fs() -> StdModule {
    let mut fns = BTreeMap::new();
    fns.insert(
        "symlink".into(),
        intrinsic_fn(
            "symlink",
            &[("from", Type::String), ("to", Type::String)],
            Type::Symlink,
            IntrinsicId::Symlink,
        ),
    );
    fns.insert(
        "file".into(),
        intrinsic_fn(
            "file",
            &[("path", Type::String), ("content", Type::String)],
            Type::File,
            IntrinsicId::File,
        ),
    );
    fns.insert(
        "directory".into(),
        intrinsic_fn(
            "directory",
            &[("path", Type::String)],
            Type::Directory,
            IntrinsicId::Directory,
        ),
    );
    StdModule { fns }
}

fn intrinsic_fn(
    name: &str,
    params: &[(&str, Type)],
    return_type: Type,
    intrinsic: IntrinsicId,
) -> FnDecl {
    FnDecl {
        name: spanned(name.to_string()),
        params: params
            .iter()
            .map(|(n, t)| Param {
                name: spanned((*n).to_string()),
                ty: spanned(t.clone()),
                default: None,
                span: 0..0,
            })
            .collect(),
        return_type: spanned(return_type),
        body: Block {
            stmts: Vec::new(),
            trailing: None,
            span: 0..0,
        },
        span: 0..0,
        intrinsic: Some(intrinsic),
    }
}

const fn spanned<T>(node: T) -> Spanned<T> {
    Spanned { node, span: 0..0 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn synth_program_orders_fns_alphabetically() {
        let module = build_fs();
        let prog = module.synth_program();
        let names: Vec<String> = prog
            .items
            .iter()
            .map(|item| match item {
                Item::Fn(f) => f.name.node.clone(),
                _ => String::new(),
            })
            .collect();
        assert_eq!(names, vec!["directory", "file", "symlink"]);
    }

    #[test]
    fn registry_exposes_fs() {
        let reg = registry();
        let fs = reg.get("fs").expect("fs module present");
        assert!(fs.fns.contains_key("symlink"));
        assert!(fs.fns.contains_key("file"));
        assert!(fs.fns.contains_key("directory"));
    }

    #[test]
    fn fs_intrinsics_are_tagged() {
        let fs = build_fs();
        assert_eq!(fs.fns["symlink"].intrinsic, Some(IntrinsicId::Symlink));
        assert_eq!(fs.fns["file"].intrinsic, Some(IntrinsicId::File));
        assert_eq!(fs.fns["directory"].intrinsic, Some(IntrinsicId::Directory));
    }
}
