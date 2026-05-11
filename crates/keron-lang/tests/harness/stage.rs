//! Stage classification + per-stage snapshot production.

use std::path::Path;

use keron_lang::{Diagnostic, check_module, parse, resolve_type_names};

use super::render;

#[path = "../support/stdlib.rs"]
mod stdlib;

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
                    let imp = stdlib::imports();
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
                let imp = stdlib::imports();
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
