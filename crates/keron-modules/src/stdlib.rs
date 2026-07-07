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

use keron_lang::{Block, Expr, FnDecl, IntrinsicId, Item, Literal, Param, Program, Spanned, Type};

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
    reg.insert("host", build_host());
    reg.insert("secrets", build_secrets());
    reg.insert("packages", build_packages());
    reg.insert("shell", build_shell());
    reg.insert("keys", build_keys());
    reg.insert("string", build_string());
    reg.insert("list", build_list());
    reg.insert("collection", build_collection());
    reg.insert("map", build_map());
    reg.insert("path", build_path());
    reg.insert("file", build_file());
    reg.insert("numeric", build_numeric());
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

/// Names of every variant of the `ShellKind` string-union type.
pub const SHELL_KIND_VARIANTS: &[&str] = &["sh", "bash", "zsh", "pwsh", "powershell"];

/// `std:fs` builtins — the resource constructors plus the
/// `Resource`/`Symlink`/`Template` types they produce.
///
/// `template(source, target, vars)` is the only file-producing form:
/// `source` is a path to an external template file (resolved
/// relative to the importing module's directory at apply time);
/// Tera `{{ name }}` placeholders are substituted from `vars` (a
/// missing variable is a hard render error), and the rendered text is
/// written to `target`. A "plain" file with no substitutions is just a
/// `template` whose `vars` map is empty.
fn build_fs() -> StdModule {
    let mut fns = BTreeMap::new();
    fns.insert(
        "symlink".into(),
        intrinsic_fn(
            "symlink",
            &[("source", Type::String), ("target", Type::String)],
            Type::Symlink,
            IntrinsicId::Symlink,
        ),
    );
    fns.insert(
        "template".into(),
        // `vars` defaults to `{}`: the doc above calls the plain file
        // (no substitutions) the canonical degenerate case, so the
        // common form shouldn't force `vars = {}` boilerplate.
        with_default(
            intrinsic_fn(
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
                IntrinsicId::Template,
            ),
            "vars",
            Expr::Map(Vec::new()),
        ),
    );
    let mut types = BTreeMap::new();
    types.insert("Symlink".into(), Type::Symlink);
    types.insert("Template".into(), Type::Template);
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
/// (including the empty string), `null` when it is unset. The nullable
/// return type is the whole point — a config that needs a fallback can
/// `match` on it, and a config that strictly requires the var can fail
/// loudly through the type system instead of silently using an empty
/// string.
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
///
/// `secret(uri)` returns `Secret`, not `Secret?`: resolution failure
/// is a hard error, deliberately. `secret("op://x") ?? fallback` is
/// a type error (`??` requires a nullable LHS). See the design note
/// on [`IntrinsicId::Secret`] for the rationale.
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

/// `std:packages` builtins — `brew`, `cask`, `cargo`, and `winget`
/// constructors for the unified `Package` resource. Each returns a
/// `Package` (which widens to `Resource`, so a list / reconcile arm
/// can mix them with files and symlinks). The manager identity is
/// preserved on the produced value so the executor picks the right
/// CLI at apply time; the user-facing type system sees one shape.
///
/// `brew` and `cask` take an optional `tap_url: String? = null` second
/// arg: when the formula/cask name is slash-qualified
/// (`user/tap/formula`), `tap_url` overrides the auto-derived
/// `homebrew-<tap>` URL for taps whose repo doesn't follow the
/// convention. `cargo` and `winget` keep the single-name shape — their
/// upstream registries don't have tap-like indirection.
fn build_packages() -> StdModule {
    let mut fns = BTreeMap::new();
    fns.insert("brew".into(), build_brewish_fn("brew", IntrinsicId::Brew));
    fns.insert("cask".into(), build_brewish_fn("cask", IntrinsicId::Cask));
    fns.insert(
        "cargo".into(),
        intrinsic_fn(
            "cargo",
            &[("name", Type::String)],
            Type::Package,
            IntrinsicId::Cargo,
        ),
    );
    fns.insert(
        "winget".into(),
        intrinsic_fn(
            "winget",
            &[("name", Type::String)],
            Type::Package,
            IntrinsicId::Winget,
        ),
    );
    let mut types = BTreeMap::new();
    types.insert("Package".into(), Type::Package);
    StdModule { fns, types }
}

/// Build the two-arg `(name, tap_url? = null)` signature shared by
/// `brew` and `cask`.
fn build_brewish_fn(name: &str, intrinsic: IntrinsicId) -> FnDecl {
    with_default(
        intrinsic_fn(
            name,
            &[
                ("name", Type::String),
                ("tap_url", Type::Nullable(Box::new(Type::String))),
            ],
            Type::Package,
            intrinsic,
        ),
        "tap_url",
        Expr::Literal(Literal::Null),
    )
}

/// Attach a default expression to one parameter of an intrinsic
/// signature. The checker reads it as `has_default` (the argument may
/// be omitted at the call site); the evaluator materializes it via
/// `with_intrinsic_defaults` before dispatch — the same contract as a
/// user-fn default, so intrinsic and user signatures behave alike.
fn with_default(mut decl: FnDecl, param: &str, default: Expr) -> FnDecl {
    let p = decl
        .params
        .iter_mut()
        .find(|p| p.name.node == param)
        .expect("with_default: parameter must exist on the signature");
    p.default = Some(spanned(default));
    decl
}

/// `std:shell` builtins — explicit, always-run shell resources.
/// Construction is pure: the evaluator only records the script and
/// root cwd; planning verifies the selected shell exists, and apply
/// feeds the script over stdin.
fn build_shell() -> StdModule {
    let shell_kind = string_union("ShellKind", SHELL_KIND_VARIANTS);
    let mut fns = BTreeMap::new();
    fns.insert(
        "shell".into(),
        intrinsic_fn(
            "shell",
            &[
                ("kind", shell_kind.clone()),
                ("name", Type::String),
                ("script", Type::String),
            ],
            Type::Shell,
            IntrinsicId::Shell,
        ),
    );
    let mut types = BTreeMap::new();
    types.insert("Shell".into(), Type::Shell);
    types.insert("ShellKind".into(), shell_kind);
    StdModule { fns, types }
}

/// `std:keys` builtins — import-only SSH and GPG key resources.
///
/// Both intrinsics ensure *presence* of the supplied key material
/// without ever generating new keys. The encrypted blob flows in as a
/// [`Type::Secret`] (the typechecker rejects bare `String`), and the
/// produced resource is treated as always-sensitive by the plan
/// diff — `--verbose-will-reveal-sensitive-content` is the opt-in for
/// printing the material.
///
/// `apply` only ever issues `Create` or `NoOp` for these resources:
///
///   - SSH: writing to either path when a regular file with different
///     content already exists is a hard error rather than a silent
///     rotation. The user's memory of the prior key is the only path
///     out — they remove the file manually if a new key is intended.
///   - GPG: idempotency is checked via `gpg --batch --list-secret-keys
///     <fingerprint>` (exit status only, never stdout capture). If the
///     fingerprint is already in the keyring the resource is `NoOp`.
fn build_keys() -> StdModule {
    let mut fns = BTreeMap::new();
    fns.insert(
        "ssh_key".into(),
        intrinsic_fn(
            "ssh_key",
            &[
                ("private_path", Type::String),
                ("public_path", Type::String),
                ("private", Type::Secret),
                ("public", Type::String),
            ],
            Type::SshKey,
            IntrinsicId::SshKey,
        ),
    );
    fns.insert(
        "gpg_key".into(),
        intrinsic_fn(
            "gpg_key",
            &[("fingerprint", Type::String), ("key", Type::Secret)],
            Type::GpgKey,
            IntrinsicId::GpgKey,
        ),
    );
    let mut types = BTreeMap::new();
    types.insert("SshKey".into(), Type::SshKey);
    types.insert("GpgKey".into(), Type::GpgKey);
    StdModule { fns, types }
}

/// `std:host` builtins — per-machine identity signals (hostname,
/// invoking user) and standard directory locations. Directory
/// helpers wrap the `dirs` crate, which follows XDG on Linux and the
/// equivalent platform convention on macOS / Windows.
///
/// Universally-available dirs (`home`, `config`, `cache`, `data`)
/// return `String` and bail when the underlying lookup fails — which
/// practically means `$HOME` is unset with no platform fallback.
/// Linux-only dirs (`state`, `runtime`) return `String?` because the
/// `dirs` crate returns `None` on macOS / Windows by design.
fn build_host() -> StdModule {
    let mut fns = BTreeMap::new();
    fns.insert(
        "hostname".into(),
        intrinsic_fn("hostname", &[], Type::String, IntrinsicId::Hostname),
    );
    fns.insert(
        "user".into(),
        intrinsic_fn("user", &[], Type::String, IntrinsicId::User),
    );
    fns.insert(
        "home_dir".into(),
        intrinsic_fn("home_dir", &[], Type::String, IntrinsicId::HomeDir),
    );
    fns.insert(
        "config_dir".into(),
        intrinsic_fn("config_dir", &[], Type::String, IntrinsicId::ConfigDir),
    );
    fns.insert(
        "cache_dir".into(),
        intrinsic_fn("cache_dir", &[], Type::String, IntrinsicId::CacheDir),
    );
    fns.insert(
        "data_dir".into(),
        intrinsic_fn("data_dir", &[], Type::String, IntrinsicId::DataDir),
    );
    fns.insert(
        "state_dir".into(),
        intrinsic_fn(
            "state_dir",
            &[],
            Type::Nullable(Box::new(Type::String)),
            IntrinsicId::StateDir,
        ),
    );
    fns.insert(
        "runtime_dir".into(),
        intrinsic_fn(
            "runtime_dir",
            &[],
            Type::Nullable(Box::new(Type::String)),
            IntrinsicId::RuntimeDir,
        ),
    );
    StdModule {
        fns,
        types: BTreeMap::new(),
    }
}

/// `std:string` builtins — pure string operations that today have no
/// in-language equivalent. The set is deliberately minimal:
///
///   - `split(s, sep)` / `join(xs, sep)` — build and unbuild paths
///     and PATH-like strings.
///   - `replace(s, from, to)` — fixed-string rewrite (not a regex; we
///     don't want a regex engine in the dotfile DSL).
///   - `trim(s)` — strip surrounding whitespace, useful after reading
///     a `shell(...)` output via templating.
fn build_string() -> StdModule {
    let mut fns = BTreeMap::new();
    fns.insert(
        "split".into(),
        intrinsic_fn(
            "split",
            &[("s", Type::String), ("sep", Type::String)],
            Type::List(Box::new(Type::String)),
            IntrinsicId::Split,
        ),
    );
    fns.insert(
        "join".into(),
        intrinsic_fn(
            "join",
            &[
                ("xs", Type::List(Box::new(Type::String))),
                ("sep", Type::String),
            ],
            Type::String,
            IntrinsicId::Join,
        ),
    );
    fns.insert(
        "replace".into(),
        intrinsic_fn(
            "replace",
            &[
                ("s", Type::String),
                ("from", Type::String),
                ("to", Type::String),
            ],
            Type::String,
            IntrinsicId::Replace,
        ),
    );
    fns.insert(
        "trim".into(),
        intrinsic_fn(
            "trim",
            &[("s", Type::String)],
            Type::String,
            IntrinsicId::Trim,
        ),
    );
    fns.insert(
        "starts_with".into(),
        intrinsic_fn(
            "starts_with",
            &[("s", Type::String), ("prefix", Type::String)],
            Type::Boolean,
            IntrinsicId::StartsWith,
        ),
    );
    fns.insert(
        "ends_with".into(),
        intrinsic_fn(
            "ends_with",
            &[("s", Type::String), ("suffix", Type::String)],
            Type::Boolean,
            IntrinsicId::EndsWith,
        ),
    );
    StdModule {
        fns,
        types: BTreeMap::new(),
    }
}

/// `std:list` builtins — generic operations on any `List<T>`. The
/// `T` parameter is encoded with `Type::Generic("T")` in the signature;
/// `keron-lang::check::check_call` binds it from the actual argument
/// type at every call site and substitutes it into the return type.
/// Failing arms (e.g. `first` on an empty list) return `T?` so the
/// caller threads the absence through `??` or `match`.
fn build_list() -> StdModule {
    let t = Type::Generic("T".into());
    let mut fns = BTreeMap::new();
    fns.insert(
        "first".into(),
        intrinsic_fn(
            "first",
            &[("xs", Type::List(Box::new(t.clone())))],
            Type::Nullable(Box::new(t.clone())),
            IntrinsicId::ListFirst,
        ),
    );
    fns.insert(
        "last".into(),
        intrinsic_fn(
            "last",
            &[("xs", Type::List(Box::new(t.clone())))],
            Type::Nullable(Box::new(t.clone())),
            IntrinsicId::ListLast,
        ),
    );
    // `sort` is generic like its siblings (`unique`, `first`, `last`);
    // the checker gates `T` to orderable element types (`String`,
    // `Int`, `Double`, string unions) the same way `unique` /
    // `index_of` equality ops are gated.
    fns.insert(
        "sort".into(),
        intrinsic_fn(
            "sort",
            &[("xs", Type::List(Box::new(t.clone())))],
            Type::List(Box::new(t.clone())),
            IntrinsicId::Sort,
        ),
    );
    fns.insert(
        "unique".into(),
        intrinsic_fn(
            "unique",
            &[("xs", Type::List(Box::new(t.clone())))],
            Type::List(Box::new(t.clone())),
            IntrinsicId::Unique,
        ),
    );
    fns.insert(
        "index_of".into(),
        intrinsic_fn(
            "index_of",
            &[("xs", Type::List(Box::new(t.clone()))), ("x", t)],
            Type::Nullable(Box::new(Type::Int)),
            IntrinsicId::IndexOf,
        ),
    );
    StdModule {
        fns,
        types: BTreeMap::new(),
    }
}

/// Kind-uniform collection ops, resolved by first-argument type.
///
/// `len` and `contains` are the two operations whose meaning is the
/// same across `String`, `List`, and `Map` ("how big?", "is this in
/// that?"). The registry carries one representative generic
/// signature per name; `keron-lang::check::check_call` intercepts the
/// names and selects the concrete overload from the first argument's
/// type, and the evaluator dispatches on the runtime `Value` shape.
fn build_collection() -> StdModule {
    let mut fns = BTreeMap::new();
    fns.insert(
        "len".into(),
        intrinsic_fn(
            "len",
            &[("x", Type::Generic("C".into()))],
            Type::Int,
            IntrinsicId::Len,
        ),
    );
    fns.insert(
        "contains".into(),
        intrinsic_fn(
            "contains",
            &[
                ("x", Type::Generic("C".into())),
                ("item", Type::Generic("T".into())),
            ],
            Type::Boolean,
            IntrinsicId::Contains,
        ),
    );
    StdModule {
        fns,
        types: BTreeMap::new(),
    }
}

/// `std:map` builtins — generic operations on any `Map<K, V>`. `K`
/// and `V` are independent type variables bound at the call site.
/// `get` requires the caller to supply a `default: V` so the return
/// type stays `V` (not `V?`); use a `Map<K, V?>` if you need to
/// distinguish "absent" from "explicitly null".
fn build_map() -> StdModule {
    let k = Type::Generic("K".into());
    let v = Type::Generic("V".into());
    let map_kv = Type::Map(Box::new(k.clone()), Box::new(v.clone()));
    let mut fns = BTreeMap::new();
    fns.insert(
        "keys".into(),
        intrinsic_fn(
            "keys",
            &[("m", map_kv.clone())],
            Type::List(Box::new(k.clone())),
            IntrinsicId::MapKeys,
        ),
    );
    fns.insert(
        "values".into(),
        intrinsic_fn(
            "values",
            &[("m", map_kv.clone())],
            Type::List(Box::new(v.clone())),
            IntrinsicId::MapValues,
        ),
    );
    fns.insert(
        "get".into(),
        intrinsic_fn(
            "get",
            &[
                ("m", map_kv.clone()),
                ("k", k.clone()),
                ("default", v.clone()),
            ],
            v.clone(),
            IntrinsicId::MapGet,
        ),
    );
    fns.insert(
        "merge".into(),
        intrinsic_fn(
            "merge",
            &[("a", map_kv.clone()), ("b", map_kv.clone())],
            map_kv.clone(),
            IntrinsicId::MapMerge,
        ),
    );
    fns.insert(
        "without".into(),
        intrinsic_fn(
            "without",
            &[("m", map_kv.clone()), ("k", k.clone())],
            map_kv.clone(),
            IntrinsicId::MapWithout,
        ),
    );
    fns.insert(
        "with".into(),
        intrinsic_fn(
            "with",
            &[("m", map_kv.clone()), ("k", k), ("v", v)],
            map_kv,
            IntrinsicId::MapWith,
        ),
    );
    StdModule {
        fns,
        types: BTreeMap::new(),
    }
}

/// `std:file` builtins — file-content reads, distinct from `std:fs`
/// which constructs *resources* (symlinks, templates). The boundary
/// is "read raw bytes during planning" vs "schedule a file-shaped
/// effect for apply".
///
/// `read_file(path)` is keron-root-confined: the path is resolved
/// with the same `resolve_managed_path` rule the symlink/template
/// `source =` arguments use, so a hostile `.keron` repo cannot
/// exfiltrate host files via this intrinsic. Anything outside the
/// keron root, missing, unreadable, or not valid UTF-8 collapses to
/// `null` — matching the failure-to-null convention shared with
/// `env`, `path_parent`, `path_basename`, `path_extension`, and the
/// `parse_*` family. (`path_exists` is the odd one out: it answers a
/// Boolean question, so failure *is* `false`.)
fn build_file() -> StdModule {
    let mut fns = BTreeMap::new();
    fns.insert(
        "read_file".into(),
        intrinsic_fn(
            "read_file",
            &[("path", Type::String)],
            Type::Nullable(Box::new(Type::String)),
            IntrinsicId::ReadFile,
        ),
    );
    StdModule {
        fns,
        types: BTreeMap::new(),
    }
}

/// `std:numeric` builtins — string-to-number parsers. Both return
/// nullable types so a missing/malformed input flows through `??` the
/// same way `env(...) ?? "0"` does. Live in their own module rather
/// than `std:string` because the failure semantics are number-shaped
/// (not just "the string didn't satisfy a predicate").
fn build_numeric() -> StdModule {
    let mut fns = BTreeMap::new();
    fns.insert(
        "parse_int".into(),
        intrinsic_fn(
            "parse_int",
            &[("s", Type::String)],
            Type::Nullable(Box::new(Type::Int)),
            IntrinsicId::ParseInt,
        ),
    );
    fns.insert(
        "parse_double".into(),
        intrinsic_fn(
            "parse_double",
            &[("s", Type::String)],
            Type::Nullable(Box::new(Type::Double)),
            IntrinsicId::ParseDouble,
        ),
    );
    StdModule {
        fns,
        types: BTreeMap::new(),
    }
}

/// `std:path` builtins — path manipulation and filesystem probes on
/// `String` paths. We deliberately stay on `String` instead of
/// introducing a nominal `Path` type because all path values today
/// come from `home_dir()`, `keron_root()`, `env(...)`, and string
/// interpolation — there's no boundary a `Path` type would protect.
///
/// `path_exists` / `path_is_dir` / `path_is_file` read live host
/// filesystem metadata at evaluation time, so they make plan output
/// depend on disk state. Use them for explicit host-observation
/// branches the user expects to be live.
fn build_path() -> StdModule {
    let mut fns = BTreeMap::new();
    fns.insert(
        "path_join".into(),
        intrinsic_fn(
            "path_join",
            &[("p", Type::String), ("segment", Type::String)],
            Type::String,
            IntrinsicId::PathJoin,
        ),
    );
    fns.insert(
        "path_parent".into(),
        intrinsic_fn(
            "path_parent",
            &[("p", Type::String)],
            Type::Nullable(Box::new(Type::String)),
            IntrinsicId::PathParent,
        ),
    );
    // Absence is `null` across the whole component family
    // (`path_parent`, `path_basename`, `path_extension`) — one
    // encoding, so callers always branch with `??`/`match`.
    fns.insert(
        "path_basename".into(),
        intrinsic_fn(
            "path_basename",
            &[("p", Type::String)],
            Type::Nullable(Box::new(Type::String)),
            IntrinsicId::PathBasename,
        ),
    );
    fns.insert(
        "path_extension".into(),
        intrinsic_fn(
            "path_extension",
            &[("p", Type::String)],
            Type::Nullable(Box::new(Type::String)),
            IntrinsicId::PathExtension,
        ),
    );
    fns.insert(
        "path_is_absolute".into(),
        intrinsic_fn(
            "path_is_absolute",
            &[("p", Type::String)],
            Type::Boolean,
            IntrinsicId::PathIsAbsolute,
        ),
    );
    fns.insert(
        "path_exists".into(),
        intrinsic_fn(
            "path_exists",
            &[("p", Type::String)],
            Type::Boolean,
            IntrinsicId::PathExists,
        ),
    );
    fns.insert(
        "path_is_dir".into(),
        intrinsic_fn(
            "path_is_dir",
            &[("p", Type::String)],
            Type::Boolean,
            IntrinsicId::PathIsDir,
        ),
    );
    fns.insert(
        "path_is_file".into(),
        intrinsic_fn(
            "path_is_file",
            &[("p", Type::String)],
            Type::Boolean,
            IntrinsicId::PathIsFile,
        ),
    );
    StdModule {
        fns,
        types: BTreeMap::new(),
    }
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
        assert_eq!(names, vec!["symlink", "template"]);
    }

    #[test]
    fn registry_exposes_fs() {
        let reg = registry();
        let fs = reg.get("fs").expect("fs module present");
        assert!(fs.fns.contains_key("symlink"));
        assert!(fs.fns.contains_key("template"));
        assert!(!fs.fns.contains_key("file"));
    }

    #[test]
    fn fs_intrinsics_are_tagged() {
        let fs = build_fs();
        assert_eq!(fs.fns["symlink"].intrinsic, Some(IntrinsicId::Symlink));
        assert_eq!(fs.fns["template"].intrinsic, Some(IntrinsicId::Template));
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
        let reg = registry();
        let os = reg.get("os").expect("os module present");
        assert!(os.types.contains_key("OsType"));
        assert!(os.types.contains_key("OsArch"));
    }

    #[test]
    fn packages_module_registers_all_managers() {
        let reg = registry();
        let p = reg.get("packages").expect("packages module present");
        for (name, intrinsic) in [
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
        for (name, intrinsic) in [("brew", IntrinsicId::Brew), ("cask", IntrinsicId::Cask)] {
            let f = p
                .fns
                .get(name)
                .unwrap_or_else(|| panic!("{name} fn present"));
            assert_eq!(f.intrinsic, Some(intrinsic));
            assert_eq!(f.params.len(), 2);
            assert_eq!(f.params[0].name.node, "name");
            assert_eq!(f.params[0].ty.node, Type::String);
            assert!(f.params[0].default.is_none());
            assert_eq!(f.params[1].name.node, "tap_url");
            assert_eq!(f.params[1].ty.node, Type::Nullable(Box::new(Type::String)));
            assert!(
                f.params[1].default.is_some(),
                "tap_url should default to null"
            );
            assert_eq!(f.return_type.node, Type::Package);
        }
        assert_eq!(p.types.get("Package"), Some(&Type::Package));
    }

    #[test]
    fn shell_module_registers_shell_resource() {
        let reg = registry();
        let sh = reg.get("shell").expect("shell module present");
        let f = sh.fns.get("shell").expect("shell fn present");
        assert_eq!(f.intrinsic, Some(IntrinsicId::Shell));
        assert_eq!(f.params.len(), 3);
        assert_eq!(f.params[0].name.node, "kind");
        assert_eq!(f.params[1].name.node, "name");
        assert_eq!(f.params[1].ty.node, Type::String);
        assert_eq!(f.params[2].name.node, "script");
        assert_eq!(f.params[2].ty.node, Type::String);
        assert_eq!(f.return_type.node, Type::Shell);
        assert_eq!(sh.types.get("Shell"), Some(&Type::Shell));
        let Type::StringUnion { name, variants } = sh.types.get("ShellKind").unwrap() else {
            panic!("expected ShellKind string union");
        };
        assert_eq!(name, "ShellKind");
        assert_eq!(
            variants,
            &SHELL_KIND_VARIANTS
                .iter()
                .map(|variant| (*variant).to_string())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn secrets_module_registers_secret_and_unwrap_secret() {
        let reg = registry();
        let s = reg.get("secrets").expect("secrets module present");
        let sec = s.fns.get("secret").expect("secret fn present");
        assert_eq!(sec.intrinsic, Some(IntrinsicId::Secret));
        assert_eq!(sec.params.len(), 1);
        assert_eq!(sec.params[0].name.node, "uri");
        assert_eq!(sec.params[0].ty.node, Type::String);
        assert_eq!(sec.return_type.node, Type::Secret);
        let uw = s
            .fns
            .get("unwrap_secret")
            .expect("unwrap_secret fn present");
        assert_eq!(uw.intrinsic, Some(IntrinsicId::UnwrapSecret));
        assert_eq!(uw.params.len(), 1);
        assert_eq!(uw.params[0].name.node, "s");
        assert_eq!(uw.params[0].ty.node, Type::Secret);
        assert_eq!(uw.return_type.node, Type::String);
        assert_eq!(s.types.get("Secret"), Some(&Type::Secret));
    }

    #[test]
    fn env_module_registers_env_with_nullable_string_return() {
        let reg = registry();
        let env_mod = reg.get("env").expect("env module present");
        let f = env_mod.fns.get("env").expect("env fn present");
        assert_eq!(f.intrinsic, Some(IntrinsicId::Env));
        assert_eq!(f.params.len(), 1);
        assert_eq!(f.params[0].name.node, "name");
        assert_eq!(f.params[0].ty.node, Type::String);
        assert_eq!(f.return_type.node, Type::Nullable(Box::new(Type::String)),);
    }

    #[test]
    fn host_module_registers_identity_and_required_dir_helpers_as_string() {
        let reg = registry();
        let host = reg.get("host").expect("host module present");
        for (name, intrinsic) in [
            ("hostname", IntrinsicId::Hostname),
            ("user", IntrinsicId::User),
            ("home_dir", IntrinsicId::HomeDir),
            ("config_dir", IntrinsicId::ConfigDir),
            ("cache_dir", IntrinsicId::CacheDir),
            ("data_dir", IntrinsicId::DataDir),
        ] {
            let f = host
                .fns
                .get(name)
                .unwrap_or_else(|| panic!("{name} fn present"));
            assert_eq!(f.intrinsic, Some(intrinsic));
            assert!(f.params.is_empty(), "{name} takes no arguments");
            assert_eq!(
                f.return_type.node,
                Type::String,
                "{name} returns String (universally available)"
            );
        }
    }

    #[test]
    fn host_module_marks_linux_only_dirs_as_nullable() {
        let reg = registry();
        let host = reg.get("host").expect("host module present");
        for (name, intrinsic) in [
            ("state_dir", IntrinsicId::StateDir),
            ("runtime_dir", IntrinsicId::RuntimeDir),
        ] {
            let f = host
                .fns
                .get(name)
                .unwrap_or_else(|| panic!("{name} fn present"));
            assert_eq!(f.intrinsic, Some(intrinsic));
            // Linux-only dirs are `String?` so macOS / Windows users
            // see a `null` they can `??` rather than a runtime error.
            assert_eq!(
                f.return_type.node,
                Type::Nullable(Box::new(Type::String)),
                "{name} returns String?"
            );
        }
    }

    #[test]
    fn path_module_registers_manipulation_and_probe_intrinsics() {
        let reg = registry();
        let path = reg.get("path").expect("path module present");

        let join = path.fns.get("path_join").expect("path_join fn present");
        assert_eq!(join.intrinsic, Some(IntrinsicId::PathJoin));
        assert_eq!(join.params.len(), 2);
        assert_eq!(join.params[0].ty.node, Type::String);
        assert_eq!(join.params[1].ty.node, Type::String);
        assert_eq!(join.return_type.node, Type::String);

        let parent = path.fns.get("path_parent").expect("path_parent fn present");
        assert_eq!(parent.intrinsic, Some(IntrinsicId::PathParent));
        // `path_parent` returns `String?` so the no-parent case
        // surfaces as `null` rather than an exception.
        assert_eq!(
            parent.return_type.node,
            Type::Nullable(Box::new(Type::String)),
        );

        // The whole component family encodes absence as `null`.
        for (name, intrinsic) in [
            ("path_basename", IntrinsicId::PathBasename),
            ("path_extension", IntrinsicId::PathExtension),
        ] {
            let f = path
                .fns
                .get(name)
                .unwrap_or_else(|| panic!("{name} fn present"));
            assert_eq!(f.intrinsic, Some(intrinsic));
            assert_eq!(
                f.return_type.node,
                Type::Nullable(Box::new(Type::String)),
                "{name} returns String?",
            );
        }

        for (name, intrinsic) in [
            ("path_is_absolute", IntrinsicId::PathIsAbsolute),
            ("path_exists", IntrinsicId::PathExists),
            ("path_is_dir", IntrinsicId::PathIsDir),
            ("path_is_file", IntrinsicId::PathIsFile),
        ] {
            let f = path
                .fns
                .get(name)
                .unwrap_or_else(|| panic!("{name} fn present"));
            assert_eq!(f.intrinsic, Some(intrinsic));
            assert_eq!(f.return_type.node, Type::Boolean);
        }
    }

    #[test]
    fn list_module_signatures_are_generic_in_t() {
        let reg = registry();
        let list_mod = reg.get("list").expect("list module present");
        let t = Type::Generic("T".into());

        let first = list_mod.fns.get("first").expect("first fn present");
        assert_eq!(first.intrinsic, Some(IntrinsicId::ListFirst));
        assert_eq!(first.params[0].ty.node, Type::List(Box::new(t.clone())));
        assert_eq!(first.return_type.node, Type::Nullable(Box::new(t.clone())),);

        let last = list_mod.fns.get("last").expect("last fn present");
        assert_eq!(last.intrinsic, Some(IntrinsicId::ListLast));
        assert_eq!(last.return_type.node, Type::Nullable(Box::new(t)));
    }

    #[test]
    fn map_module_signatures_are_generic_in_k_and_v() {
        let reg = registry();
        let map = reg.get("map").expect("map module present");
        let k = Type::Generic("K".into());
        let v = Type::Generic("V".into());
        let map_kv = Type::Map(Box::new(k.clone()), Box::new(v.clone()));

        let keys = map.fns.get("keys").expect("keys fn present");
        assert_eq!(keys.intrinsic, Some(IntrinsicId::MapKeys));
        assert_eq!(keys.params[0].ty.node, map_kv);
        assert_eq!(keys.return_type.node, Type::List(Box::new(k.clone())));

        let values = map.fns.get("values").expect("values fn present");
        assert_eq!(values.intrinsic, Some(IntrinsicId::MapValues));
        assert_eq!(values.return_type.node, Type::List(Box::new(v.clone())));

        let get = map.fns.get("get").expect("get fn present");
        assert_eq!(get.intrinsic, Some(IntrinsicId::MapGet));
        assert_eq!(get.params[1].ty.node, k);
        assert_eq!(get.params[2].ty.node, v);
        // `get` returns the bound `V` — the caller supplies a default
        // so the result type stays non-nullable.
        assert_eq!(get.return_type.node, v);
    }

    #[test]
    fn string_module_registers_split_join_replace_trim() {
        let reg = registry();
        let s = reg.get("string").expect("string module present");

        let split = s.fns.get("split").expect("split fn present");
        assert_eq!(split.intrinsic, Some(IntrinsicId::Split));
        assert_eq!(split.params.len(), 2);
        assert_eq!(split.params[0].ty.node, Type::String);
        assert_eq!(split.params[1].ty.node, Type::String);
        assert_eq!(
            split.return_type.node,
            Type::List(Box::new(Type::String)),
            "split returns List<String>"
        );

        let join = s.fns.get("join").expect("join fn present");
        assert_eq!(join.intrinsic, Some(IntrinsicId::Join));
        assert_eq!(join.params.len(), 2);
        assert_eq!(join.params[0].ty.node, Type::List(Box::new(Type::String)));
        assert_eq!(join.params[1].ty.node, Type::String);
        assert_eq!(join.return_type.node, Type::String);

        let replace = s.fns.get("replace").expect("replace fn present");
        assert_eq!(replace.intrinsic, Some(IntrinsicId::Replace));
        assert_eq!(replace.params.len(), 3);
        assert_eq!(replace.return_type.node, Type::String);

        let trim = s.fns.get("trim").expect("trim fn present");
        assert_eq!(trim.intrinsic, Some(IntrinsicId::Trim));
        assert_eq!(trim.params.len(), 1);
        assert_eq!(trim.return_type.node, Type::String);
    }

    #[test]
    fn string_module_registers_starts_and_ends_with() {
        let reg = registry();
        let s = reg.get("string").expect("string module present");

        let starts = s.fns.get("starts_with").expect("starts_with present");
        assert_eq!(starts.intrinsic, Some(IntrinsicId::StartsWith));
        assert_eq!(starts.params.len(), 2);
        assert_eq!(starts.return_type.node, Type::Boolean);

        let ends = s.fns.get("ends_with").expect("ends_with present");
        assert_eq!(ends.intrinsic, Some(IntrinsicId::EndsWith));
        assert_eq!(ends.params.len(), 2);
        assert_eq!(ends.return_type.node, Type::Boolean);
    }

    #[test]
    fn collection_module_registers_type_directed_len_and_contains() {
        let reg = registry();
        let coll = reg.get("collection").expect("collection module present");

        // Representative generic signatures: the checker intercepts
        // both names and resolves the concrete overload from the
        // first argument's type, so `Generic("C")` is never bound.
        let len = coll.fns.get("len").expect("len present");
        assert_eq!(len.intrinsic, Some(IntrinsicId::Len));
        assert_eq!(len.params.len(), 1);
        assert_eq!(len.params[0].name.node, "x");
        assert_eq!(len.return_type.node, Type::Int);

        let contains = coll.fns.get("contains").expect("contains present");
        assert_eq!(contains.intrinsic, Some(IntrinsicId::Contains));
        assert_eq!(contains.params.len(), 2);
        assert_eq!(contains.params[0].name.node, "x");
        assert_eq!(contains.params[1].name.node, "item");
        assert_eq!(contains.return_type.node, Type::Boolean);

        // The removed prefixed variants must not resurface.
        for module in reg.values() {
            for gone in ["str_len", "list_contains", "map_contains"] {
                assert!(!module.fns.contains_key(gone), "`{gone}` should be gone");
            }
        }
    }
}
