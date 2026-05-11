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
    reg.insert("keron", build_keron());
    reg.insert("os", build_os());
    reg.insert("env", build_env());
    reg.insert("secrets", build_secrets());
    reg.insert("packages", build_packages());
    reg
}

/// Names of every variant of the `OsType` string-union type.
///
/// The intrinsic dispatcher in `keron-apply` matches host detection
/// against this exact list — keep them in sync. `"Unknown"` is the
/// fallback any unrecognized host falls through to.
pub const OS_TYPE_VARIANTS: &[&str] = &["Linux", "Macos", "Windows", "Unknown"];

/// Names of every variant of the `OsArch` string-union type. Same
/// fallback rule as [`OS_TYPE_VARIANTS`]: anything not enumerated
/// here collapses to `"Unknown"` when the intrinsic runs.
pub const OS_ARCH_VARIANTS: &[&str] = &["x86_64", "aarch64", "arm", "x86", "Unknown"];

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

/// `std:keron` builtins — environmental info about the running
/// `keron apply` invocation. `keron_root()` returns the absolute path
/// the user pointed at (canonicalized) so configs can interpolate it
/// into resource paths without hard-coding their install location.
fn build_keron() -> StdModule {
    let mut fns = BTreeMap::new();
    fns.insert(
        "keron_root".into(),
        intrinsic_fn("keron_root", &[], Type::String, IntrinsicId::KeronRoot),
    );
    StdModule {
        fns,
        types: BTreeMap::new(),
    }
}

/// `std:env` builtins — read process environment variables.
/// `env(name)` returns `String?`: `Some(value)` when the var is set
/// (including the empty string), `null` when it is unset. The
/// nullable return type is the whole point — a config that needs a
/// fallback can `match` on it, and a config that strictly requires
/// the var can fail loudly through the type system instead of
/// silently using an empty string.
fn build_env() -> StdModule {
    let mut fns = BTreeMap::new();
    fns.insert(
        "env".into(),
        intrinsic_fn(
            "env",
            &[("name", Type::String)],
            Type::Nullable(Box::new(Type::String)),
            IntrinsicId::Env,
        ),
    );
    StdModule {
        fns,
        types: BTreeMap::new(),
    }
}

/// `std:secrets` builtins — `secret(uri)` resolves an external
/// secret store URI eagerly at plan-build time and returns a
/// `Secret`. `unwrap_secret(s)` is the only legal way to convert a
/// `Secret` to a `String`; every call site is an audit breadcrumb
/// for "here is where the secret leaves the marker type."
///
/// `Secret` is **not** a subtype of `String`. Interpolation, concat,
/// cross-type equality with strings, and Map keys are rejected by
/// the type checker — a secret can only land in a sink via an
/// explicit `unwrap_secret(...)`.
fn build_secrets() -> StdModule {
    let mut fns = BTreeMap::new();
    fns.insert(
        "secret".into(),
        intrinsic_fn(
            "secret",
            &[("uri", Type::String)],
            Type::Secret,
            IntrinsicId::Secret,
        ),
    );
    fns.insert(
        "unwrap_secret".into(),
        intrinsic_fn(
            "unwrap_secret",
            &[("s", Type::Secret)],
            Type::String,
            IntrinsicId::UnwrapSecret,
        ),
    );
    let mut types = BTreeMap::new();
    types.insert("Secret".into(), Type::Secret);
    StdModule { fns, types }
}

/// `std:packages` builtins — `brew`, `cargo`, and `winget`
/// constructors for the unified `Package` resource. Each returns a
/// `Package` (which widens to `Resource`, so a list / reconcile arm
/// can mix them with files and symlinks). The manager identity is
/// preserved on the produced value so the executor picks the right
/// CLI at apply time; the user-facing type system sees one shape.
///
/// v1 carries only the package name. Version pinning, taps, sources,
/// and feature flags can be added as a second positional arg later
/// without changing the existing signatures.
fn build_packages() -> StdModule {
    let mut fns = BTreeMap::new();
    for name in ["brew", "cargo", "winget"] {
        let id = match name {
            "brew" => IntrinsicId::Brew,
            "cargo" => IntrinsicId::Cargo,
            "winget" => IntrinsicId::Winget,
            _ => unreachable!(),
        };
        fns.insert(
            name.into(),
            intrinsic_fn(name, &[("name", Type::String)], Type::Package, id),
        );
    }
    let mut types = BTreeMap::new();
    types.insert("Package".into(), Type::Package);
    StdModule { fns, types }
}

/// `std:os` builtins — host OS / architecture detection exposed as
/// string-union types so configs can `match` on them. The intrinsic
/// dispatcher maps `os_info`'s richer enums onto our small fixed
/// variant lists (see [`OS_TYPE_VARIANTS`] / [`OS_ARCH_VARIANTS`]),
/// with `"Unknown"` as the fallback for any host we don't enumerate.
fn build_os() -> StdModule {
    let os_type = string_union("OsType", OS_TYPE_VARIANTS);
    let os_arch = string_union("OsArch", OS_ARCH_VARIANTS);
    let mut fns = BTreeMap::new();
    fns.insert(
        "os_type".into(),
        intrinsic_fn("os_type", &[], os_type.clone(), IntrinsicId::OsType),
    );
    fns.insert(
        "os_arch".into(),
        intrinsic_fn("os_arch", &[], os_arch.clone(), IntrinsicId::OsArch),
    );
    let mut types = BTreeMap::new();
    types.insert("OsType".into(), os_type);
    types.insert("OsArch".into(), os_arch);
    StdModule { fns, types }
}

fn string_union(name: &str, variants: &[&str]) -> Type {
    Type::StringUnion {
        name: name.to_string(),
        variants: variants.iter().map(|v| (*v).to_string()).collect(),
    }
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

    #[test]
    fn keron_module_registers_keron_root() {
        let reg = registry();
        let keron = reg.get("keron").expect("keron module present");
        let f = keron.fns.get("keron_root").expect("keron_root fn present");
        assert_eq!(f.intrinsic, Some(IntrinsicId::KeronRoot));
        assert!(f.params.is_empty(), "keron_root takes no arguments");
        assert_eq!(f.return_type.node, Type::String);
    }

    #[test]
    fn os_module_registers_os_type_and_os_arch() {
        let reg = registry();
        let os = reg.get("os").expect("os module present");
        assert_eq!(os.fns["os_type"].intrinsic, Some(IntrinsicId::OsType));
        assert_eq!(os.fns["os_arch"].intrinsic, Some(IntrinsicId::OsArch));
        assert!(os.fns["os_type"].params.is_empty());
        assert!(os.fns["os_arch"].params.is_empty());
    }

    #[test]
    fn os_type_return_is_a_string_union_with_documented_variants() {
        // Pin the exact variant list so a typo in `build_os` (or in
        // `OS_TYPE_VARIANTS`) surfaces here rather than at apply time
        // where the diagnostic is muddier.
        let reg = registry();
        let os = reg.get("os").expect("os module present");
        let Type::StringUnion {
            ref name,
            ref variants,
        } = os.fns["os_type"].return_type.node
        else {
            panic!("expected StringUnion return type");
        };
        assert_eq!(name, "OsType");
        assert_eq!(variants, &["Linux", "Macos", "Windows", "Unknown"]);
    }

    #[test]
    fn os_arch_return_is_a_string_union_including_unknown_fallback() {
        let reg = registry();
        let os = reg.get("os").expect("os module present");
        let Type::StringUnion {
            ref name,
            ref variants,
        } = os.fns["os_arch"].return_type.node
        else {
            panic!("expected StringUnion return type");
        };
        assert_eq!(name, "OsArch");
        assert!(
            variants.contains(&"Unknown".to_string()),
            "OsArch must include Unknown fallback: {variants:?}",
        );
        assert!(variants.contains(&"x86_64".to_string()));
        assert!(variants.contains(&"aarch64".to_string()));
    }

    #[test]
    fn os_module_exports_union_types_for_user_code() {
        // Type aliases are exposed alongside the fns so a user can
        // write `val t: OsType = os_type()` (or `match os_type() { ... }`).
        let reg = registry();
        let os = reg.get("os").expect("os module present");
        assert!(os.types.contains_key("OsType"));
        assert!(os.types.contains_key("OsArch"));
    }

    #[test]
    fn packages_module_registers_all_three_managers() {
        let reg = registry();
        let p = reg.get("packages").expect("packages module present");
        for (name, intrinsic) in [
            ("brew", IntrinsicId::Brew),
            ("cargo", IntrinsicId::Cargo),
            ("winget", IntrinsicId::Winget),
        ] {
            let f = p
                .fns
                .get(name)
                .unwrap_or_else(|| panic!("{name} fn present"));
            assert_eq!(f.intrinsic, Some(intrinsic));
            assert_eq!(f.params.len(), 1);
            assert_eq!(f.params[0].name.node, "name");
            assert_eq!(f.params[0].ty.node, Type::String);
            assert_eq!(f.return_type.node, Type::Package);
        }
        assert_eq!(p.types.get("Package"), Some(&Type::Package));
    }

    #[test]
    fn secrets_module_registers_secret_and_unwrap_secret() {
        let reg = registry();
        let s = reg.get("secrets").expect("secrets module present");
        // `secret(uri: String): Secret`
        let sec = s.fns.get("secret").expect("secret fn present");
        assert_eq!(sec.intrinsic, Some(IntrinsicId::Secret));
        assert_eq!(sec.params.len(), 1);
        assert_eq!(sec.params[0].name.node, "uri");
        assert_eq!(sec.params[0].ty.node, Type::String);
        assert_eq!(sec.return_type.node, Type::Secret);
        // `unwrap_secret(s: Secret): String`
        let uw = s
            .fns
            .get("unwrap_secret")
            .expect("unwrap_secret fn present");
        assert_eq!(uw.intrinsic, Some(IntrinsicId::UnwrapSecret));
        assert_eq!(uw.params.len(), 1);
        assert_eq!(uw.params[0].name.node, "s");
        assert_eq!(uw.params[0].ty.node, Type::Secret);
        assert_eq!(uw.return_type.node, Type::String);
        // The `Secret` type is exported so users can annotate `val
        // token: Secret = secret(...)`.
        assert_eq!(s.types.get("Secret"), Some(&Type::Secret));
    }

    #[test]
    fn env_module_registers_env_with_nullable_string_return() {
        // The signature is the whole API contract for `env`: one
        // `String` parameter and a `String?` return. A drift here
        // (e.g. accidentally returning bare `String`) would make
        // unset variables silently look like empty strings.
        let reg = registry();
        let env_mod = reg.get("env").expect("env module present");
        let f = env_mod.fns.get("env").expect("env fn present");
        assert_eq!(f.intrinsic, Some(IntrinsicId::Env));
        assert_eq!(f.params.len(), 1);
        assert_eq!(f.params[0].name.node, "name");
        assert_eq!(f.params[0].ty.node, Type::String);
        assert_eq!(f.return_type.node, Type::Nullable(Box::new(Type::String)),);
    }
}
