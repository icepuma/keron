//! Rust-level stdlib registry.
//!
//! Stdlib items are not authored as `.keron` source — they live in
//! this module as Rust data. The resolver injects every entry into
//! every user module's [`ImportedSymbols`] as a builtin (no `from
//! "std:..."` import line required); the type checker sees the
//! `FnDecl`s as ordinary functions (with signatures only), and the
//! evaluator dispatches them via the [`IntrinsicId`] tag.
//!
//! [`ImportedSymbols`]: keron_lang::ImportedSymbols

use std::collections::BTreeMap;
use std::sync::OnceLock;

use keron_lang::{Block, FnDecl, IntrinsicId, Item, Param, Program, Spanned, Type};

#[derive(Debug)]
pub struct StdModule {
    /// Stored in a [`BTreeMap`] so [`Self::synth_program`] yields a
    /// deterministic, alphabetically-ordered `Program` without needing
    /// an explicit sort step.
    pub fns: BTreeMap<String, FnDecl>,
    /// Named types this module exports. The resolver makes these
    /// implicitly available as builtins; user code references them
    /// directly (e.g. `val s: Symlink = ...`) and the module loader
    /// rewrites `Type::Named(name)` to the canonical [`Type`] variant.
    pub types: BTreeMap<String, Type>,
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

/// Process-wide stdlib registry. Each entry's items are injected as
/// builtins into every user module's `ImportedSymbols`. The map key
/// is purely organizational — users never name modules directly.
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

/// `std:fs` builtins — the resource constructors plus the
/// `Resource`/`Symlink`/`Template`/`Directory` types they produce.
///
/// `template(path, source, vars)` is the only file-producing form:
/// `source` is a path to an external template file (resolved
/// relative to the importing module's directory at apply time);
/// `${name}` placeholders are substituted from `vars`, and the
/// rendered text is written to `path`. A "plain" file with no
/// substitutions is just a `template` whose `vars` map is empty.
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
        "template".into(),
        intrinsic_fn(
            "template",
            &[
                ("path", Type::String),
                ("source", Type::String),
                (
                    "vars",
                    Type::Map(Box::new(Type::String), Box::new(Type::String)),
                ),
            ],
            Type::Template,
            IntrinsicId::Template,
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
    let mut types = BTreeMap::new();
    types.insert("Symlink".into(), Type::Symlink);
    types.insert("Template".into(), Type::Template);
    types.insert("Directory".into(), Type::Directory);
    types.insert("Resource".into(), Type::Resource);
    StdModule { fns, types }
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
        assert_eq!(names, vec!["directory", "symlink", "template"]);
    }

    #[test]
    fn registry_exposes_fs() {
        let reg = registry();
        let fs = reg.get("fs").expect("fs module present");
        assert!(fs.fns.contains_key("symlink"));
        assert!(fs.fns.contains_key("template"));
        assert!(fs.fns.contains_key("directory"));
        assert!(!fs.fns.contains_key("file"));
    }

    #[test]
    fn fs_intrinsics_are_tagged() {
        let fs = build_fs();
        assert_eq!(fs.fns["symlink"].intrinsic, Some(IntrinsicId::Symlink));
        assert_eq!(fs.fns["template"].intrinsic, Some(IntrinsicId::Template));
        assert_eq!(fs.fns["directory"].intrinsic, Some(IntrinsicId::Directory));
    }

    #[test]
    fn fs_exports_template_type_not_file() {
        let fs = build_fs();
        assert!(fs.types.contains_key("Template"));
        assert!(!fs.types.contains_key("File"));
    }
}
