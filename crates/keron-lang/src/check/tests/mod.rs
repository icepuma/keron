//! Type checker unit tests, grouped by topic.

mod arithmetic;
mod comparisons;
mod conditional;
mod fns;
mod lists;
mod literals;
mod maps;
mod reconcile;
mod resources;
mod strings;
mod vars;

use crate::{
    ast::Type,
    check::{FnSig, ImportedSymbols, ParamSig, check_module, resolve_type_names},
    diagnostic::Diagnostic,
    parser::parse,
};

/// Type-check a snippet with the stdlib resource constructors and
/// types (`symlink`/`file`/`directory` plus
/// `Symlink`/`File`/`Directory`/`Resource`) pre-seeded as builtins.
/// Mirrors what the resolver injects into every user module — these
/// unit tests exercise checker behavior in isolation, so they hand-
/// seed rather than going through the resolver.
pub(super) fn check_src(src: &str) -> Result<(), Vec<Diagnostic>> {
    let mut prog = parse(src).expect("parse should succeed");
    let imp = fs_imports();
    resolve_type_names(&mut prog, &imp)?;
    check_module(&prog, &imp)
}

fn fs_imports() -> ImportedSymbols {
    let mut imp = ImportedSymbols::default();
    imp.fns.insert(
        "symlink".into(),
        FnSig {
            params: vec![
                ParamSig {
                    name: "from".into(),
                    ty: Type::String,
                    has_default: false,
                },
                ParamSig {
                    name: "to".into(),
                    ty: Type::String,
                    has_default: false,
                },
            ],
            return_type: Type::Symlink,
        },
    );
    imp.fns.insert(
        "template".into(),
        FnSig {
            params: vec![
                ParamSig {
                    name: "path".into(),
                    ty: Type::String,
                    has_default: false,
                },
                ParamSig {
                    name: "source".into(),
                    ty: Type::String,
                    has_default: false,
                },
                ParamSig {
                    name: "vars".into(),
                    ty: Type::Map(Box::new(Type::String), Box::new(Type::String)),
                    has_default: false,
                },
            ],
            return_type: Type::Template,
        },
    );
    imp.fns.insert(
        "directory".into(),
        FnSig {
            params: vec![ParamSig {
                name: "path".into(),
                ty: Type::String,
                has_default: false,
            }],
            return_type: Type::Directory,
        },
    );
    imp.types.insert("Symlink".into(), Type::Symlink);
    imp.types.insert("Template".into(), Type::Template);
    imp.types.insert("Directory".into(), Type::Directory);
    imp.types.insert("Resource".into(), Type::Resource);
    for name in [
        "symlink",
        "template",
        "directory",
        "Symlink",
        "Template",
        "Directory",
        "Resource",
    ] {
        imp.builtins.insert(name.into());
    }
    imp
}
