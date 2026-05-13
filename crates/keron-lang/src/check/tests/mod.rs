//! Type checker unit tests, grouped by topic.

mod arithmetic;
mod comparisons;
mod conditional;
mod fns;
mod lists;
mod literals;
mod maps;
mod nullable;
mod packages;
mod reconcile;
mod resources;
mod secrets;
mod shell;
mod strings;
mod vars;

use crate::{
    ast::Type,
    check::{FnSig, ImportedSymbols, ParamSig, check_module, resolve_type_names},
    diagnostic::Diagnostic,
    parser::parse,
};

/// Type-check a snippet with the stdlib resource constructors and
/// types pre-seeded as builtins. Mirrors what the resolver injects
/// into every user module — these unit tests exercise checker
/// behavior in isolation, so they hand-seed rather than going through
/// the resolver.
pub(super) fn check_src(src: &str) -> Result<(), Vec<Diagnostic>> {
    let mut prog = parse(src).expect("parse should succeed");
    let imp = fs_imports();
    resolve_type_names(&mut prog, &imp)?;
    check_module(&prog, &imp)
}

fn fs_imports() -> ImportedSymbols {
    let mut imp = ImportedSymbols::default();
    seed_fs(&mut imp);
    seed_secrets(&mut imp);
    seed_packages(&mut imp);
    seed_shell(&mut imp);
    for name in [
        "symlink",
        "template",
        "Symlink",
        "Template",
        "Resource",
        "Shell",
        "ShellKind",
        "secret",
        "unwrap_secret",
        "Secret",
        "brew",
        "cargo",
        "winget",
        "Package",
        "shell",
    ] {
        imp.builtins.insert(name.into());
    }
    imp
}

fn param(name: &str, ty: Type) -> ParamSig {
    ParamSig {
        name: name.into(),
        ty,
        has_default: false,
    }
}

fn fn_sig(params: Vec<ParamSig>, return_type: Type) -> FnSig {
    FnSig {
        params,
        return_type,
    }
}

fn seed_fs(imp: &mut ImportedSymbols) {
    imp.fns.insert(
        "symlink".into(),
        fn_sig(
            vec![param("from", Type::String), param("to", Type::String)],
            Type::Symlink,
        ),
    );
    imp.fns.insert(
        "template".into(),
        fn_sig(
            vec![
                param("path", Type::String),
                param("source", Type::String),
                param(
                    "vars",
                    Type::Map(Box::new(Type::String), Box::new(Type::String)),
                ),
            ],
            Type::Template,
        ),
    );
    imp.types.insert("Symlink".into(), Type::Symlink);
    imp.types.insert("Template".into(), Type::Template);
    imp.types.insert("Resource".into(), Type::Resource);
}

fn seed_secrets(imp: &mut ImportedSymbols) {
    imp.fns.insert(
        "secret".into(),
        fn_sig(vec![param("uri", Type::String)], Type::Secret),
    );
    imp.fns.insert(
        "unwrap_secret".into(),
        fn_sig(vec![param("s", Type::Secret)], Type::String),
    );
    imp.types.insert("Secret".into(), Type::Secret);
}

fn seed_packages(imp: &mut ImportedSymbols) {
    // Each manager constructor takes a `name: String` and returns
    // the unified `Package` resource; the manager identity (brew /
    // cargo / winget) is carried by the `IntrinsicId` tag, not the
    // type.
    for fn_name in ["brew", "cargo", "winget"] {
        imp.fns.insert(
            fn_name.into(),
            fn_sig(vec![param("name", Type::String)], Type::Package),
        );
    }
    imp.types.insert("Package".into(), Type::Package);
}

fn seed_shell(imp: &mut ImportedSymbols) {
    let shell_kind = Type::StringUnion {
        name: "ShellKind".into(),
        variants: ["sh", "bash", "zsh", "pwsh", "powershell"]
            .into_iter()
            .map(String::from)
            .collect(),
    };
    imp.fns.insert(
        "shell".into(),
        fn_sig(
            vec![
                param("kind", shell_kind.clone()),
                param("name", Type::String),
                param("script", Type::String),
            ],
            Type::Shell,
        ),
    );
    imp.types.insert("Shell".into(), Type::Shell);
    imp.types.insert("ShellKind".into(), shell_kind);
}
