use keron_lang::{FnSig, ImportedSymbols, ParamSig, Type};

pub const RESERVED_OR_BUILTIN_NAMES: &[&str] = &[
    "val",
    "fn",
    "reconcile",
    "if",
    "else",
    "for",
    "in",
    "match",
    "struct",
    "type",
    "true",
    "false",
    "null",
    "String",
    "Int",
    "Boolean",
    "Double",
    "List",
    "Map",
    "Void",
    "Symlink",
    "Template",
    "Resource",
    "Secret",
    "Package",
    "Shell",
    "ShellKind",
    "OsType",
    "OsArch",
    "symlink",
    "template",
    "shell",
    "keron_root",
    "env",
    "secret",
    "unwrap_secret",
    "brew",
    "cargo",
    "winget",
    "os_type",
    "os_arch",
];

pub fn imports() -> ImportedSymbols {
    debug_assert!(RESERVED_OR_BUILTIN_NAMES.contains(&"symlink"));
    let mut imp = ImportedSymbols::default();
    insert_fn(
        &mut imp,
        "symlink",
        &[("source", Type::String), ("target", Type::String)],
        Type::Symlink,
    );
    insert_fn(
        &mut imp,
        "template",
        &[
            ("source", Type::String),
            ("target", Type::String),
            (
                "vars",
                Type::Map(Box::new(Type::String), Box::new(Type::String)),
            ),
        ],
        Type::Template,
    );
    insert_fn(&mut imp, "keron_root", &[], Type::String);
    insert_fn(
        &mut imp,
        "env",
        &[("name", Type::String)],
        Type::Nullable(Box::new(Type::String)),
    );
    insert_fn(&mut imp, "secret", &[("uri", Type::String)], Type::Secret);
    insert_fn(
        &mut imp,
        "unwrap_secret",
        &[("s", Type::Secret)],
        Type::String,
    );
    for name in ["brew", "cargo", "winget"] {
        insert_fn(&mut imp, name, &[("name", Type::String)], Type::Package);
    }
    let shell_kind = string_union("ShellKind", &["sh", "bash", "zsh", "pwsh", "powershell"]);
    insert_fn(
        &mut imp,
        "shell",
        &[
            ("kind", shell_kind.clone()),
            ("name", Type::String),
            ("script", Type::String),
        ],
        Type::Shell,
    );

    let os_type = string_union("OsType", &["Linux", "Macos", "Windows", "Unknown"]);
    let os_arch = string_union("OsArch", &["x86_64", "aarch64", "arm", "x86", "Unknown"]);
    insert_fn(&mut imp, "os_type", &[], os_type.clone());
    insert_fn(&mut imp, "os_arch", &[], os_arch.clone());

    for (name, ty) in [
        ("Symlink", Type::Symlink),
        ("Template", Type::Template),
        ("Resource", Type::Resource),
        ("Secret", Type::Secret),
        ("Package", Type::Package),
        ("Shell", Type::Shell),
        ("ShellKind", shell_kind),
        ("OsType", os_type),
        ("OsArch", os_arch),
    ] {
        imp.types.insert(name.into(), ty);
        imp.builtins.insert(name.into());
    }
    imp
}

fn insert_fn(imp: &mut ImportedSymbols, name: &str, params: &[(&str, Type)], return_type: Type) {
    imp.fns.insert(
        name.into(),
        FnSig {
            params: params
                .iter()
                .map(|(name, ty)| ParamSig {
                    name: (*name).into(),
                    ty: ty.clone(),
                    has_default: false,
                })
                .collect(),
            return_type,
        },
    );
    imp.builtins.insert(name.into());
}

fn string_union(name: &str, variants: &[&str]) -> Type {
    Type::StringUnion {
        name: name.into(),
        variants: variants.iter().map(|variant| (*variant).into()).collect(),
    }
}
