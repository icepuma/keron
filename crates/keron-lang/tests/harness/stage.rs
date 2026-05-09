//! Stage classification + per-stage snapshot production.

use std::path::Path;

use keron_lang::{Diagnostic, FnSig, ImportedSymbols, ParamSig, Type, check_module, parse};

use super::render;

/// Pre-resolved imported symbols mirroring what `from "std:fs" use
/// symlink, file, directory` brings in. Fixtures may include the
/// `use` line for fidelity with real source (the parser accepts it,
/// the checker treats it as inert), but the harness independently
/// seeds these signatures so checking proceeds without going through
/// the full module resolver.
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

#[derive(Debug, Clone, Copy)]
pub enum Stage {
    /// Source must parse cleanly. Snapshot = AST.
    Parse,
    /// Source must parse AND typecheck. Snapshot = AST.
    Check,
    /// Source must fail to parse. Snapshot = rendered diagnostics.
    ErrorParse,
    /// Source must parse but fail typecheck. Snapshot = rendered diagnostics.
    ErrorCheck,
}

impl Stage {
    pub fn from_path(root: &Path, path: &Path) -> Option<Self> {
        let rel = path.strip_prefix(root).ok()?;
        let mut comps = rel.components();
        let first = comps.next()?.as_os_str().to_str()?;
        match first {
            "parse" => Some(Self::Parse),
            "check" => Some(Self::Check),
            "errors" => match comps.next()?.as_os_str().to_str()? {
                "parse" => Some(Self::ErrorParse),
                "check" => Some(Self::ErrorCheck),
                _ => None,
            },
            _ => None,
        }
    }

    pub fn run(self, src: &str) -> String {
        match self {
            Self::Parse => match parse(src) {
                Ok(prog) => format!("{prog:#?}\n"),
                Err(errs) => panic!("expected parse to succeed; got:\n{}", join(&errs, src)),
            },
            Self::Check => match parse(src) {
                Ok(prog) => match check_module(&prog, &fs_imports()) {
                    Ok(()) => format!("{prog:#?}\n"),
                    Err(errs) => panic!("expected check to succeed; got:\n{}", join(&errs, src)),
                },
                Err(errs) => panic!("expected parse to succeed; got:\n{}", join(&errs, src)),
            },
            Self::ErrorParse => match parse(src) {
                Ok(prog) => panic!("expected parse to fail; produced AST:\n{prog:#?}"),
                Err(errs) => render::diagnostics(src, &errs),
            },
            Self::ErrorCheck => {
                let prog = parse(src).unwrap_or_else(|errs| {
                    panic!("expected parse to succeed; got:\n{}", join(&errs, src))
                });
                match check_module(&prog, &fs_imports()) {
                    Ok(()) => panic!("expected check to fail; got Ok"),
                    Err(errs) => render::diagnostics(src, &errs),
                }
            }
        }
    }
}

fn join(errs: &[Diagnostic], src: &str) -> String {
    render::diagnostics(src, errs)
}
