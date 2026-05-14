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
    "hostname",
    "user",
    "home_dir",
    "config_dir",
    "cache_dir",
    "data_dir",
    "state_dir",
    "runtime_dir",
    "split",
    "join",
    "contains",
    "replace",
    "trim",
    "len",
    "list_contains",
    "first",
    "last",
    "keys",
    "values",
    "get",
    "map_contains",
    "path_join",
    "path_parent",
    "path_basename",
    "path_extension",
    "path_is_absolute",
    "path_exists",
    "path_is_dir",
    "path_is_file",
    "read_file",
];

pub fn imports() -> ImportedSymbols {
    debug_assert!(RESERVED_OR_BUILTIN_NAMES.contains(&"symlink"));
    let mut imp = ImportedSymbols::default();
    insert_fs(&mut imp);
    insert_keron(&mut imp);
    insert_env(&mut imp);
    insert_secrets(&mut imp);
    insert_packages(&mut imp);
    let shell_kind = insert_shell(&mut imp);
    let (os_type, os_arch) = insert_os(&mut imp);
    insert_host(&mut imp);
    insert_string(&mut imp);
    insert_list(&mut imp);
    insert_map(&mut imp);
    insert_path(&mut imp);
    insert_file(&mut imp);
    insert_numeric(&mut imp);
    insert_named_types(&mut imp, shell_kind, os_type, os_arch);
    imp
}

fn insert_fs(imp: &mut ImportedSymbols) {
    insert_fn(
        imp,
        "symlink",
        &[("source", Type::String), ("target", Type::String)],
        Type::Symlink,
    );
    insert_fn(
        imp,
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
}

fn insert_keron(imp: &mut ImportedSymbols) {
    insert_fn(imp, "keron_root", &[], Type::String);
}

fn insert_env(imp: &mut ImportedSymbols) {
    insert_fn(
        imp,
        "env",
        &[("name", Type::String)],
        Type::Nullable(Box::new(Type::String)),
    );
}

fn insert_secrets(imp: &mut ImportedSymbols) {
    insert_fn(imp, "secret", &[("uri", Type::String)], Type::Secret);
    insert_fn(imp, "unwrap_secret", &[("s", Type::Secret)], Type::String);
}

fn insert_packages(imp: &mut ImportedSymbols) {
    for name in ["brew", "cargo", "winget"] {
        insert_fn(imp, name, &[("name", Type::String)], Type::Package);
    }
}

fn insert_shell(imp: &mut ImportedSymbols) -> Type {
    let shell_kind = string_union("ShellKind", &["sh", "bash", "zsh", "pwsh", "powershell"]);
    insert_fn(
        imp,
        "shell",
        &[
            ("kind", shell_kind.clone()),
            ("name", Type::String),
            ("script", Type::String),
        ],
        Type::Shell,
    );
    shell_kind
}

fn insert_os(imp: &mut ImportedSymbols) -> (Type, Type) {
    let os_type = string_union("OsType", &["Linux", "Macos", "Windows", "Unknown"]);
    let os_arch = string_union("OsArch", &["x86_64", "aarch64", "arm", "x86", "Unknown"]);
    insert_fn(imp, "os_type", &[], os_type.clone());
    insert_fn(imp, "os_arch", &[], os_arch.clone());
    (os_type, os_arch)
}

/// `std:host` — universally-available signals return `String`;
/// Linux-only XDG dirs (`state`, `runtime`) return `String?`.
fn insert_host(imp: &mut ImportedSymbols) {
    for name in [
        "hostname",
        "user",
        "home_dir",
        "config_dir",
        "cache_dir",
        "data_dir",
    ] {
        insert_fn(imp, name, &[], Type::String);
    }
    for name in ["state_dir", "runtime_dir"] {
        insert_fn(imp, name, &[], Type::Nullable(Box::new(Type::String)));
    }
}

/// `std:string` — pure string operations.
fn insert_string(imp: &mut ImportedSymbols) {
    insert_fn(
        imp,
        "split",
        &[("s", Type::String), ("sep", Type::String)],
        Type::List(Box::new(Type::String)),
    );
    insert_fn(
        imp,
        "join",
        &[
            ("xs", Type::List(Box::new(Type::String))),
            ("sep", Type::String),
        ],
        Type::String,
    );
    insert_fn(
        imp,
        "contains",
        &[("haystack", Type::String), ("needle", Type::String)],
        Type::Boolean,
    );
    insert_fn(
        imp,
        "replace",
        &[
            ("s", Type::String),
            ("from", Type::String),
            ("to", Type::String),
        ],
        Type::String,
    );
    insert_fn(imp, "trim", &[("s", Type::String)], Type::String);
}

/// `std:list` — generic over `T`. Signatures use `Type::Generic("T")`
/// which `check_call` binds at every call site.
fn insert_list(imp: &mut ImportedSymbols) {
    let t = Type::Generic("T".into());
    insert_fn(
        imp,
        "len",
        &[("xs", Type::List(Box::new(t.clone())))],
        Type::Int,
    );
    insert_fn(
        imp,
        "list_contains",
        &[("xs", Type::List(Box::new(t.clone()))), ("x", t.clone())],
        Type::Boolean,
    );
    insert_fn(
        imp,
        "first",
        &[("xs", Type::List(Box::new(t.clone())))],
        Type::Nullable(Box::new(t.clone())),
    );
    insert_fn(
        imp,
        "last",
        &[("xs", Type::List(Box::new(t.clone())))],
        Type::Nullable(Box::new(t.clone())),
    );
    insert_fn(
        imp,
        "sort",
        &[("xs", Type::List(Box::new(Type::String)))],
        Type::List(Box::new(Type::String)),
    );
    insert_fn(
        imp,
        "unique",
        &[("xs", Type::List(Box::new(t.clone())))],
        Type::List(Box::new(t.clone())),
    );
    insert_fn(
        imp,
        "index_of",
        &[("xs", Type::List(Box::new(t.clone()))), ("x", t)],
        Type::Nullable(Box::new(Type::Int)),
    );
}

/// `std:map` — generic over `K` and `V`.
fn insert_map(imp: &mut ImportedSymbols) {
    let k = Type::Generic("K".into());
    let v = Type::Generic("V".into());
    let map_kv = Type::Map(Box::new(k.clone()), Box::new(v.clone()));
    insert_fn(
        imp,
        "keys",
        &[("m", map_kv.clone())],
        Type::List(Box::new(k.clone())),
    );
    insert_fn(
        imp,
        "values",
        &[("m", map_kv.clone())],
        Type::List(Box::new(v.clone())),
    );
    insert_fn(
        imp,
        "get",
        &[
            ("m", map_kv.clone()),
            ("k", k.clone()),
            ("default", v.clone()),
        ],
        v.clone(),
    );
    insert_fn(
        imp,
        "map_contains",
        &[("m", map_kv.clone()), ("k", k.clone())],
        Type::Boolean,
    );
    insert_fn(
        imp,
        "merge",
        &[("a", map_kv.clone()), ("b", map_kv.clone())],
        map_kv.clone(),
    );
    insert_fn(
        imp,
        "without",
        &[("m", map_kv.clone()), ("k", k.clone())],
        map_kv.clone(),
    );
    insert_fn(
        imp,
        "with",
        &[("m", map_kv.clone()), ("k", k), ("v", v)],
        map_kv,
    );
}

/// `std:path` — path manipulation and FS probes on `String` paths.
fn insert_path(imp: &mut ImportedSymbols) {
    insert_fn(
        imp,
        "path_join",
        &[("p", Type::String), ("segment", Type::String)],
        Type::String,
    );
    insert_fn(
        imp,
        "path_parent",
        &[("p", Type::String)],
        Type::Nullable(Box::new(Type::String)),
    );
    for name in ["path_basename", "path_extension"] {
        insert_fn(imp, name, &[("p", Type::String)], Type::String);
    }
    for name in [
        "path_is_absolute",
        "path_exists",
        "path_is_dir",
        "path_is_file",
    ] {
        insert_fn(imp, name, &[("p", Type::String)], Type::Boolean);
    }
}

/// `std:file` — `read_file(path) -> String?`, keron-root-confined.
fn insert_file(imp: &mut ImportedSymbols) {
    insert_fn(
        imp,
        "read_file",
        &[("path", Type::String)],
        Type::Nullable(Box::new(Type::String)),
    );
}

/// `std:numeric` — strict string-to-number parsers; nullable result.
fn insert_numeric(imp: &mut ImportedSymbols) {
    insert_fn(
        imp,
        "parse_int",
        &[("s", Type::String)],
        Type::Nullable(Box::new(Type::Int)),
    );
    insert_fn(
        imp,
        "parse_double",
        &[("s", Type::String)],
        Type::Nullable(Box::new(Type::Double)),
    );
}

fn insert_named_types(imp: &mut ImportedSymbols, shell_kind: Type, os_type: Type, os_arch: Type) {
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
