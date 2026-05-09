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

/// Type-check a snippet with the `std:fs` resource constructors and
/// types (`symlink`/`file`/`directory` plus
/// `Symlink`/`File`/`Directory`/`Resource`) pre-imported. Unit tests
/// in this module exercise checker behavior, not the import system,
/// so we inject these signatures rather than threading them through
/// every source string. The corpus tests verify the explicit-import
/// requirement at the language-integration level.
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
    imp.types.insert("Symlink".into(), Type::Symlink);
    imp.types.insert("File".into(), Type::File);
    imp.types.insert("Directory".into(), Type::Directory);
    imp.types.insert("Resource".into(), Type::Resource);
    imp
}
