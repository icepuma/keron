//! Stage classification + per-stage snapshot production.

use std::path::Path;

use keron_lang::{
    Diagnostic, FnSig, ImportedSymbols, ParamSig, Type, check_module, parse, resolve_type_names,
};

use super::render;

/// Pre-resolved imported symbols mirroring the implicit stdlib
/// builtins (`symlink`, `file`, `directory`, plus the resource type
/// names). Mirrors `keron-modules::stdlib::fs` by hand because this
/// harness lives in `keron-lang` and can't depend on
/// `keron-modules`. Keep in sync if the registry grows.
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
    imp.builtins.insert("symlink".into());
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
    imp.builtins.insert("file".into());
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
    imp.builtins.insert("directory".into());
    for (name, ty) in [
        ("Symlink", Type::Symlink),
        ("File", Type::File),
        ("Directory", Type::Directory),
        ("Resource", Type::Resource),
    ] {
        imp.types.insert(name.into(), ty);
        imp.builtins.insert(name.into());
    }
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
                Ok(mut prog) => {
                    let imp = fs_imports();
                    if let Err(errs) = resolve_type_names(&mut prog, &imp) {
                        panic!(
                            "expected type resolution to succeed; got:\n{}",
                            join(&errs, src)
                        );
                    }
                    match check_module(&prog, &imp) {
                        Ok(()) => format!("{prog:#?}\n"),
                        Err(errs) => {
                            panic!("expected check to succeed; got:\n{}", join(&errs, src))
                        }
                    }
                }
                Err(errs) => panic!("expected parse to succeed; got:\n{}", join(&errs, src)),
            },
            Self::ErrorParse => match parse(src) {
                Ok(prog) => panic!("expected parse to fail; produced AST:\n{prog:#?}"),
                Err(errs) => render::diagnostics(src, &errs),
            },
            Self::ErrorCheck => {
                let mut prog = parse(src).unwrap_or_else(|errs| {
                    panic!("expected parse to succeed; got:\n{}", join(&errs, src))
                });
                let imp = fs_imports();
                // Type-resolution errors (`unknown type Foo`) are
                // surfaced through the same diagnostic channel as
                // checker errors; the corpus treats both as expected
                // failures of the "check" stage.
                if let Err(errs) = resolve_type_names(&mut prog, &imp) {
                    return render::diagnostics(src, &errs);
                }
                match check_module(&prog, &imp) {
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
