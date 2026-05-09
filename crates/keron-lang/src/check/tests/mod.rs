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
    check::{FnSig, ImportedSymbols, ParamSig, check_module},
    diagnostic::Diagnostic,
    parser::parse,
};

/// Type-check a snippet with the `std:fs` resource constructors
/// (`symlink`, `file`, `directory`) pre-imported. Unit tests in this
/// module exercise checker behavior, not the import system, so we
/// inject these signatures rather than threading them through every
/// source string. The corpus tests verify the explicit-import
/// requirement at the language-integration level.
pub(super) fn check_src(src: &str) -> Result<(), Vec<Diagnostic>> {
    let prog = parse(src).expect("parse should succeed");
    check_module(&prog, &fs_imports())
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
        "file".into(),
        FnSig {
            params: vec![
                ParamSig {
                    name: "path".into(),
                    ty: Type::String,
                    has_default: false,
                },
                ParamSig {
                    name: "content".into(),
                    ty: Type::String,
                    has_default: false,
                },
            ],
            return_type: Type::File,
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
    imp
}
