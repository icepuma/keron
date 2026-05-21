//! AST nodes for keron source.

use core::{fmt, ops::Range};

pub type Span = Range<usize>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Spanned<T> {
    pub node: T,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Program {
    pub items: Vec<Item>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Item {
    /// `from "<path>" use a, b, c` — bring named functions/vals from
    /// another module into this module's flat namespace. The imported
    /// names share scope with locals; a collision is an error.
    Use(UseDecl),
    Val(ValDecl),
    Fn(FnDecl),
    /// `struct Name { f: T, ... }` — nominal record. Constructed via
    /// the existing call form (`Name(...)`); field access via `v.f`.
    Struct(StructDecl),
    /// `type Name = "a" | "b" | ...` — nominal alias for a closed set
    /// of string literals. The only kind of type alias today.
    TypeAlias(TypeAliasDecl),
    Reconcile(ReconcileDecl),
    /// A top-level expression evaluated for its effect (e.g.
    /// `if cond { reconcile foo }`). The expression must have type
    /// `Void`; the type checker rejects anything else, which is how
    /// keron prevents pointless top-level computations.
    ExprStmt(Spanned<Expr>),
}

impl Item {
    /// Source span of this top-level item, used by the trivia
    /// extractor to attach comments to the right node.
    #[must_use]
    pub fn span(&self) -> Span {
        match self {
            Self::Use(u) => u.span.clone(),
            Self::Val(v) => v.span.clone(),
            Self::Fn(f) => f.span.clone(),
            Self::Struct(s) => s.span.clone(),
            Self::TypeAlias(t) => t.span.clone(),
            Self::Reconcile(r) => r.span.clone(),
            Self::ExprStmt(e) => e.span.clone(),
        }
    }
}

/// `from "<path>" use name1, name2, …`.
///
/// The path is a literal string with no interpolation. Permitted
/// shapes: `"./..."`, `"../..."`, `"/..."` — filesystem paths to other
/// `.keron` files, resolved relative to the importing module. Stdlib
/// items are exposed as builtins by the resolver and don't go through
/// this declaration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UseDecl {
    pub source: Spanned<String>,
    pub names: Vec<Spanned<String>>,
    pub span: Span,
}

/// A `reconcile` directive.
///
/// Three surface forms collapse into one shape: the bare single
/// resource (`reconcile x`), the inline chain (`reconcile a -> b -> c`),
/// and the block form (`reconcile { … }`). Each top-level element of
/// [`Self::chains`] is one logical step, executed in source order;
/// within a step, the inner `Vec` carries `->`-chained sub-steps, also
/// in source order.
///
/// The two `Vec`s are non-empty by construction: the parser rejects an
/// empty block and a trailing/missing-head `->`.
#[derive(Debug, Clone, PartialEq)]
pub struct ReconcileDecl {
    pub chains: Vec<Vec<Spanned<Expr>>>,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ValDecl {
    pub name: Spanned<String>,
    /// Optional type annotation. When `None`, the type is inferred from
    /// `value` (which makes the inferred type trivially correct, so the
    /// checker has nothing to verify).
    pub ty: Option<Spanned<Type>>,
    pub value: Spanned<Expr>,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FnDecl {
    pub name: Spanned<String>,
    pub params: Vec<Param>,
    pub return_type: Spanned<Type>,
    pub body: Block,
    pub span: Span,
    /// Set only by the stdlib registry — never produced by the parser.
    /// The evaluator dispatches on this tag instead of `body`, so the
    /// `body` field is an unused empty block for intrinsic decls.
    pub intrinsic: Option<IntrinsicId>,
}

/// `struct Name { field: Type, ... }` — a nominal record type. Field
/// order is significant: the implicit constructor accepts positional
/// arguments in declared order (and named arguments by field name).
#[derive(Debug, Clone, PartialEq)]
pub struct StructDecl {
    pub name: Spanned<String>,
    pub fields: Vec<StructField>,
    pub span: Span,
}

/// One declared field of a struct.
///
/// `default` is set when the field is written as `name: Type = expr`
/// — the default value is evaluated at construction time when the
/// caller omits the field. Same shape and ordering rule as fn-parameter
/// defaults: any required field must precede every defaulted one (the
/// checker enforces this).
#[derive(Debug, Clone, PartialEq)]
pub struct StructField {
    pub name: Spanned<String>,
    pub ty: Spanned<Type>,
    pub default: Option<Spanned<Expr>>,
    pub span: Span,
}

/// `type Name = "a" | "b" | ...` — a nominal closed enumeration of
/// string literals. There must be at least one variant; duplicates are
/// rejected by the checker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypeAliasDecl {
    pub name: Spanned<String>,
    pub variants: Vec<Spanned<String>>,
    pub span: Span,
}

/// Tag identifying a stdlib intrinsic.
///
/// The evaluator's special case for resource constructors keys on
/// this rather than the function name, so aliasing via `use foo as
/// bar` (when added later) keeps working without further changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntrinsicId {
    Symlink,
    /// `shell(kind, name, script)` — declare an explicit shell script
    /// resource. Planning verifies the named shell exists; execution
    /// feeds `script` to that shell on stdin.
    Shell,
    /// `template(source, target, vars)` — render a templated file. At
    /// apply time, `source` is read (resolved relative to the
    /// importing module's directory), `${name}` placeholders are
    /// substituted with values from `vars`, and the rendered text is
    /// written to `target`. Subsumes the old `file(target, content)`
    /// constructor: a non-templating file is just a `template` with
    /// an empty `vars` map.
    Template,
    /// `keron_root()` — absolute path to the directory the user passed
    /// to `keron apply` (or the file's parent for the single-file
    /// case). Returned as a `String` so it can be interpolated into
    /// other path arguments.
    KeronRoot,
    /// `os_type()` — current OS as one of the [`OsType`] string-union
    /// variants (`"Linux"`, `"Macos"`, `"Windows"`, `"Unknown"`).
    /// Anything outside that set collapses to `"Unknown"`.
    OsType,
    /// `os_arch()` — current CPU architecture as one of the [`OsArch`]
    /// string-union variants. Falls back to `"Unknown"` when the host
    /// reports something we don't enumerate.
    OsArch,
    /// `env(name)` — read an environment variable. Returns `String?`:
    /// the value when the variable is set (even if empty), or `null`
    /// when it is unset. Distinguishing "unset" from "empty" is the
    /// whole reason the return type is nullable rather than `String`.
    Env,
    /// `secret(uri)` — resolve an external secret URI into a
    /// `Secret` value. Eager: the URI is read at plan-build time and
    /// any resolution failure is a hard error.
    ///
    /// **Design decision (fail-hard, not nullable):** the return
    /// type is `Secret`, not `Secret?`. Adding `??` to the language
    /// opened the question of whether `secret(...) ?? fallback`
    /// should be expressible; we deliberately left it as a type
    /// error. Two reasons: (1) silent fallback is dangerous —
    /// imagine a typo in a URI silently substituting a placeholder
    /// for a database password; (2) `Secret` is an audit-breadcrumb
    /// type, and admitting non-secret fallbacks would dilute that
    /// guarantee. Users who need "this credential or a fallback
    /// non-secret value" can branch on `env(...)` (which *is*
    /// nullable) before reaching for `secret`.
    ///
    /// The returned `Secret` cannot flow into any String sink
    /// without an explicit [`Self::UnwrapSecret`] call.
    Secret,
    /// `unwrap_secret(s)` — convert a `Secret` to a `String`. The
    /// only legal way to extract; every call site is an audit
    /// breadcrumb for "here is where the secret leaves the marker
    /// type and becomes plain text."
    UnwrapSecret,
    /// `brew(name, tap_url? = null)` — declares a Homebrew formula. The
    /// apply step runs `brew install`. A slash-qualified `name` of the
    /// form `user/tap/formula` declares the formula's tap inline; the
    /// optional `tap_url` overrides the auto-derived `homebrew-<tap>`
    /// GitHub URL for taps whose repo doesn't follow that naming
    /// convention. State is classified Create / `NoOp` / Update by diffing
    /// against `brew list --formula -1` (bare-name match) and
    /// `brew outdated --formula --quiet` (qualified-name match).
    Brew,
    /// `cask(name, tap_url? = null)` — declares a Homebrew cask. The
    /// apply step runs `brew install --cask`. Same tap rules and same
    /// Create / `NoOp` / Update upsert as [`Self::Brew`], but diffed
    /// against the cask-side lists (`brew list --cask -1` and
    /// `brew outdated --cask --quiet`). Cask resources are macOS-only.
    Cask,
    /// `cargo(name)` — declares a `cargo install` binary. v1
    /// carries only the crate name.
    Cargo,
    /// `winget(name)` — declares a winget package id (e.g.
    /// `"Microsoft.PowerShell"`). v1 carries only the id; sources
    /// can be added later as a second arg.
    Winget,
    /// `hostname()` — the host's network name. Resolved at evaluation
    /// time via `gethostname` on Unix and `$COMPUTERNAME` on Windows.
    /// Returns `String`; a syscall failure is a hard error (the
    /// failure mode is rare enough that wrapping every call site in a
    /// `??` would be noise, and a missing hostname usually signals a
    /// broken machine, not a manifest bug).
    Hostname,
    /// `user()` — the invoking user's login name. Sourced from `$USER`
    /// on Unix and `$USERNAME` on Windows. Returns `String`; bails if
    /// neither is set (rare outside of CI sandboxes — those almost
    /// always set one of the two).
    User,
    /// `home_dir()` — the invoking user's home directory as an
    /// absolute path. Resolved via the `dirs` crate so the value
    /// matches the platform convention (`$HOME` on Unix,
    /// `%USERPROFILE%` on Windows). Returns `String`; bails if the
    /// crate can't determine it (effectively only when `$HOME` is
    /// unset and there's no fallback the OS can supply).
    HomeDir,
    /// `config_dir()` — user config root. Linux: `$XDG_CONFIG_HOME`
    /// or `~/.config`. macOS: `~/Library/Application Support`.
    /// Windows: `%APPDATA%` (the roaming variant). Returns `String`;
    /// bails on the same failure mode as [`Self::HomeDir`].
    ConfigDir,
    /// `cache_dir()` — user cache root. Linux: `$XDG_CACHE_HOME` or
    /// `~/.cache`. macOS: `~/Library/Caches`. Windows: `%LOCALAPPDATA%`.
    /// Same failure model as [`Self::HomeDir`].
    CacheDir,
    /// `data_dir()` — user data root for things that may sync across
    /// machines. Linux: `$XDG_DATA_HOME` or `~/.local/share`. macOS:
    /// `~/Library/Application Support`. Windows: `%APPDATA%`. Same
    /// failure model as [`Self::HomeDir`].
    DataDir,
    /// `state_dir()` — user state root for ephemeral-but-resumable
    /// data (Linux's XDG state slot). Linux: `$XDG_STATE_HOME` or
    /// `~/.local/state`. macOS and Windows: `null` — no platform
    /// equivalent exists, so the return type is `String?` and users
    /// must `??` a fallback (or `match` for OS-specific handling).
    StateDir,
    /// `runtime_dir()` — user runtime root (Linux only). Linux:
    /// `$XDG_RUNTIME_DIR`. macOS and Windows: `null`. Returns
    /// `String?` for the same reason as [`Self::StateDir`].
    RuntimeDir,
    /// `split(s, sep)` — split `s` on every (non-overlapping) match of
    /// `sep`. Returns `List<String>`. An empty `sep` is an error (no
    /// well-defined split point). Result preserves empty pieces at
    /// the ends and between adjacent separators.
    Split,
    /// `join(xs, sep)` — concatenate `xs` with `sep` between every
    /// pair. Returns `String`. Empty list produces `""`.
    Join,
    /// `contains(haystack, needle)` — true when `needle` appears
    /// anywhere in `haystack`. An empty `needle` is `true` for any
    /// `haystack` (matches Rust's `str::contains`).
    Contains,
    /// `replace(s, from, to)` — replace every (non-overlapping)
    /// occurrence of `from` in `s` with `to`. Empty `from` is an
    /// error.
    Replace,
    /// `trim(s)` — strip leading and trailing Unicode whitespace.
    Trim,
    /// `len(xs: List<T>) -> Int` — element count. Generic in `T`.
    ListLen,
    /// `list_contains(xs: List<T>, x: T) -> Boolean` — membership
    /// test (uses the same equality rule as `==`). Distinct from the
    /// `std:string` `contains` (substring check) — both are useful
    /// and live in the same flat namespace, so the list form gets a
    /// `list_` prefix. Generic in `T`.
    ListContains,
    /// `first(xs: List<T>) -> T?` — first element, or `null` for an
    /// empty list. Generic in `T`.
    ListFirst,
    /// `last(xs: List<T>) -> T?` — last element, or `null` for an
    /// empty list. Generic in `T`.
    ListLast,
    /// `keys(m: Map<K, V>) -> List<K>` — the map's keys in declared
    /// order. Generic in `K` and `V`.
    MapKeys,
    /// `values(m: Map<K, V>) -> List<V>` — the map's values in
    /// declared order. Generic in `K` and `V`.
    MapValues,
    /// `get(m: Map<K, V>, k: K, default: V) -> V` — map lookup with a
    /// caller-supplied fallback. Returns the bound `V`; if a future
    /// release wants `V?`-returning lookup, that's a separate
    /// intrinsic. Generic in `K` and `V`.
    MapGet,
    /// `map_contains(m: Map<K, V>, k: K) -> Boolean` — does the map
    /// have a binding for `k`? Distinct from list `contains` because
    /// the two have different shapes (key vs. element); shared name
    /// would be ambiguous when both Lists and Maps are in scope.
    MapContains,
    /// `path_join(p: String, segment: String) -> String` — append
    /// `segment` to `p` with platform-native separator handling. If
    /// `segment` is absolute it replaces `p` (matching `PathBuf::join`),
    /// so users who concatenate `home_dir()` with a `${maybe_abs_var}`
    /// don't silently get a corrupted path.
    PathJoin,
    /// `path_parent(p: String) -> String?` — the directory portion of
    /// `p`, or `null` when `p` is a root (`/`, `C:\`) or has no
    /// parent. Use `??` to thread the "no parent" case through.
    PathParent,
    /// `path_basename(p: String) -> String` — the final component of
    /// `p` (file name, or last directory segment). Empty for paths
    /// ending in a separator.
    PathBasename,
    /// `path_extension(p: String) -> String` — the substring after
    /// the final `.` of the basename, or `""` when there is none.
    /// Mirrors `std::path::Path::extension` — leading-dot files (e.g.
    /// `.zshrc`) are treated as having no extension.
    PathExtension,
    /// `path_is_absolute(p: String) -> Boolean` — true when `p` is a
    /// platform-absolute path (`/...` on Unix, `C:\...` on Windows).
    PathIsAbsolute,
    /// `path_exists(p: String) -> Boolean` — filesystem probe. Like
    /// `template(source = ...)` it makes plan output depend on the
    /// disk state at evaluation time; use sparingly and only for
    /// branching decisions the user expects to be live.
    PathExists,
    /// `path_is_dir(p: String) -> Boolean` — `true` only when the
    /// path exists *and* is a directory (symlinks are followed).
    PathIsDir,
    /// `path_is_file(p: String) -> Boolean` — `true` only when the
    /// path exists *and* is a regular file (symlinks are followed).
    PathIsFile,
    /// `read_file(path: String) -> String?` — load the UTF-8 contents
    /// of a file *inside the keron root* during evaluation. The path
    /// goes through the same `resolve_managed_path` containment check
    /// that `symlink(source = …)` and `template(source = …)` use, so a
    /// hostile manifest cannot read `/etc/passwd` or any host file
    /// the keron repo doesn't already include.
    ///
    /// Every failure mode — file missing, not readable, resolves
    /// outside the keron root, not valid UTF-8 — collapses to `null`,
    /// matching the convention used by `path_exists` and `env`. This
    /// is deliberately not the right place to debug "why didn't my
    /// file load"; users diagnose with `path_exists(…)` first if they
    /// need granularity, then call `read_file` once the path resolves.
    ReadFile,
    /// `sort(xs: List<String>) -> List<String>` — Unicode codepoint
    /// order ascending. Deliberately *not* generic in `T`: real
    /// dotfile use cases (`PATH` segments, package lists) are
    /// String-typed, and generic ordering would force a comparator
    /// design (Int/Double comparison, lexicographic structs) we don't
    /// have a workflow for. Lift to generics only when a manifest
    /// needs it.
    Sort,
    /// `unique(xs: List<T>) -> List<T>` — preserve first occurrence,
    /// drop subsequent duplicates. Element equality follows the same
    /// rule as `==` / `list_contains`. Generic in `T`.
    Unique,
    /// `index_of(xs: List<T>, x: T) -> Int?` — position of the first
    /// element equal to `x`, or `null` when absent. Returning `Int?`
    /// (not a sentinel `-1`) keeps the `??`-fallback path natural and
    /// avoids the "is the index also a valid result?" footgun.
    /// Generic in `T`.
    IndexOf,
    /// `merge(a: Map<K, V>, b: Map<K, V>) -> Map<K, V>` — last-wins
    /// overlay: every binding of `b` shadows the same key in `a`.
    /// Preserves declaration order of `a`, with new keys from `b`
    /// appended. Generic in `K`, `V`.
    MapMerge,
    /// `without(m: Map<K, V>, k: K) -> Map<K, V>` — drop the binding
    /// for `k` from `m`. No-op when `k` is absent (mirrors `Map::remove`
    /// when the caller doesn't need the removed value). Generic in
    /// `K`, `V`.
    MapWithout,
    /// `with(m: Map<K, V>, k: K, v: V) -> Map<K, V>` — insert or
    /// overwrite `k -> v`. When `k` is already bound, its position is
    /// preserved and only the value changes; otherwise the binding is
    /// appended. Generic in `K`, `V`.
    MapWith,
    /// `parse_int(s: String) -> Int?` — strict signed-integer parse.
    /// Accepts an optional leading `+`/`-` and ASCII digits only;
    /// returns `null` for any non-conforming input (whitespace, empty,
    /// trailing junk, hex prefixes, overflow). Use `trim(s)` first if
    /// you want to tolerate surrounding whitespace.
    ParseInt,
    /// `parse_double(s: String) -> Double?` — strict IEEE-754 parse
    /// via Rust's `str::parse::<f64>()`. Returns `null` for malformed
    /// input. NaN / infinity are not produced — Rust's parser accepts
    /// `"inf"`/`"NaN"`, so we screen those explicitly to keep `Double`
    /// values finite (the rest of the language assumes that).
    ParseDouble,
    /// `ssh_key(private_path, public_path, private, public)` — writes
    /// a user-supplied SSH keypair to disk as a single atomic
    /// resource. The `private` half flows in as [`Type::Secret`] (so a
    /// raw `String` is a typecheck error) and is written at mode
    /// `0600`; the `public` half is a `String` written at `0644`.
    /// `apply` only ensures *presence* — Create when the files are
    /// missing, `NoOp` when present and byte-identical, hard error
    /// when present with different content (refuses to silently
    /// rotate a key).
    SshKey,
    /// `gpg_key(fingerprint, key)` — imports an ASCII-armored secret
    /// keyring blob into the user's gnupg keyring. `fingerprint` is
    /// the required idempotency probe (`gpg --batch --list-secret-keys
    /// <fingerprint>`, exit status only — never stdout capture). `key`
    /// flows in as [`Type::Secret`] and is piped to `gpg --batch
    /// --import` over child stdin (never argv, never a tempfile).
    /// `apply` only ensures *presence*: Create when the fingerprint
    /// is absent, `NoOp` when present.
    GpgKey,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Param {
    pub name: Spanned<String>,
    pub ty: Spanned<Type>,
    /// Optional default value. Type-checked left-to-right: each
    /// default sees prior params as bindings.
    pub default: Option<Spanned<Expr>>,
    pub span: Span,
}

/// A `{ stmt* trailing? }` block. Used as a function body and as the
/// `then` / `else` arm of an `if` expression. The block's type is the
/// trailing expression's type if present, otherwise [`Type::Void`].
///
/// Statements inside a block are restricted to local `val`s and
/// `reconcile` directives (see [`Stmt`]); arbitrary expression
/// statements are not permitted, since keron is otherwise pure and
/// such statements would be no-ops. Conditional side effects use the
/// trailing expression slot or appear at top level via
/// [`Item::ExprStmt`].
#[derive(Debug, Clone, PartialEq)]
pub struct Block {
    pub stmts: Vec<Stmt>,
    pub trailing: Option<Spanned<Expr>>,
    pub span: Span,
}

/// A statement inside a [`Block`]. Restricted to local bindings and
/// `reconcile` directives; the type checker rejects everything else.
#[derive(Debug, Clone, PartialEq)]
pub enum Stmt {
    Val(ValDecl),
    Reconcile(ReconcileDecl),
}

#[derive(Debug, Clone, PartialEq)]
pub struct MapEntry {
    pub key: Spanned<Expr>,
    pub value: Spanned<Expr>,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CallArg {
    /// `Some(name)` for `name = value`; `None` for positional args.
    pub name: Option<Spanned<String>>,
    pub value: Spanned<Expr>,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Literal(Literal),
    Unary {
        op: UnaryOp,
        operand: Box<Spanned<Self>>,
    },
    Binary {
        op: BinOp,
        lhs: Box<Spanned<Self>>,
        rhs: Box<Spanned<Self>>,
    },
    /// A double-quoted string with one or more `${expr}` interpolations.
    /// Strings without interpolations are stored as `Literal::String`
    /// instead, so this variant always has at least one [`StringPart::Expr`].
    Interpolation(Vec<StringPart>),
    /// `[e1, e2, …]`. Empty lists carry no element type and require a
    /// `List<T>` annotation upstream to be type-checked.
    List(Vec<Spanned<Self>>),
    /// `{k: v, …}`. Empty maps similarly require a `Map<K, V>`
    /// annotation. Allowed key types: `String`, `Int`, `Boolean`.
    Map(Vec<MapEntry>),
    /// Reference to a previously-declared `val`. Resolved against the
    /// declaration order; forward references are an error.
    Var(String),
    /// Call to a top-level `fn`. Functions live in their own
    /// namespace and are not first-class values; the callee is a
    /// bare identifier rather than an arbitrary expression.
    Call {
        callee: Spanned<String>,
        args: Vec<CallArg>,
    },
    /// `if cond { … } else { … }`. Both branches are full [`Block`]s.
    /// `else` is optional in source; an omitted `else` is stored as an
    /// empty [`Block`] (type [`Type::Void`]). The condition must be
    /// `Boolean`, and the two branches' block types must match. When
    /// both branches are `Void`, the `if` is being used as control
    /// flow; otherwise, it is a value-producing expression.
    If {
        cond: Box<Spanned<Self>>,
        then_branch: Box<Block>,
        else_branch: Box<Block>,
    },
    /// `for x in xs { … }` over `List<T>` or
    /// `for (k, v) in m { … }` over `Map<K, V>`. Always has type
    /// [`Type::Void`]; the body's trailing expression must also be
    /// `Void`. Used for iteration that declares resources or gates
    /// `reconcile` directives. Permitted at top level via
    /// [`Item::ExprStmt`]. The single-bind form is list-only and the
    /// pair form is map-only — mismatches are type errors.
    For {
        pattern: ForPattern,
        iter_expr: Box<Spanned<Self>>,
        body: Box<Block>,
    },
    /// `receiver.field` — postfix field access. The checker requires
    /// the receiver to have a struct type and the field name to exist
    /// on that struct.
    Field {
        receiver: Box<Spanned<Self>>,
        field: Spanned<String>,
    },
    /// `match scrutinee { pattern => body, ... }`. Arms are tried in
    /// source order; the first matching pattern wins. The match's
    /// type is the common type of every arm body. Exhaustiveness is
    /// enforced by the checker: a string-union scrutinee may exhaust
    /// by listing every variant; every other scrutinee type requires
    /// a wildcard `_` (or bind) arm.
    Match {
        scrutinee: Box<Spanned<Self>>,
        arms: Vec<MatchArm>,
    },
}

/// One arm in a `match` expression: `pattern ('if' guard)? '=>' body`.
///
/// `guard` is an optional `Boolean` expression evaluated after the
/// pattern binds; the arm only fires when the guard returns `true`.
/// A guarded arm does **not** count as covering for exhaustiveness
/// (its guard may always be false) — the checker still requires a
/// trailing catch-all / literal arm to close the match.
#[derive(Debug, Clone, PartialEq)]
pub struct MatchArm {
    pub pattern: Spanned<Pattern>,
    pub guard: Option<Spanned<Expr>>,
    pub body: Spanned<Expr>,
    pub span: Span,
}

/// A `match` arm pattern.
#[derive(Debug, Clone, PartialEq)]
pub enum Pattern {
    /// A literal pattern: matches values equal to `lit`. Numeric and
    /// boolean literals match the corresponding primitive scrutinee;
    /// string literals match a `String` scrutinee or — when allowed —
    /// a `StringUnion` whose variant set contains the literal.
    Lit(Literal),
    /// `_` — matches anything; binds nothing.
    Wildcard,
    /// A bare lowercase identifier — matches anything; binds the
    /// scrutinee value to that name in the arm body.
    Bind(String),
    /// `Name { f: pat, g, ... }` — destructures a struct value.
    /// Pattern fields may be partial (uncovered fields are ignored,
    /// like `_`). A field with no sub-pattern is shorthand for
    /// `f: f` (binds the field's value to its own name).
    Struct {
        name: Spanned<String>,
        fields: Vec<StructPatternField>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct StructPatternField {
    pub name: Spanned<String>,
    /// `None` is shorthand: bind the field value to its own name.
    pub pattern: Option<Spanned<Pattern>>,
    pub span: Span,
}

/// Binding shape for a [`Expr::For`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ForPattern {
    /// `for x in xs` — list iteration; binds `x: T` for `xs: List<T>`.
    Elem(Spanned<String>),
    /// `for (k, v) in m` — map iteration; binds `k: K` and `v: V` for
    /// `m: Map<K, V>`. `key` and `value` must be distinct names.
    Entry {
        key: Spanned<String>,
        value: Spanned<String>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum StringPart {
    Text(String),
    Expr {
        expr: Box<Spanned<Expr>>,
        indent: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnaryOp {
    Neg,
}

impl UnaryOp {
    #[must_use]
    pub const fn symbol(self) -> &'static str {
        match self {
            Self::Neg => "-",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Pow,
    /// `++` — list concat. Operands must both be `List<T>` with the
    /// same element type. Same precedence as `+`/`-`. (String concat
    /// uses `+` instead.)
    Concat,
    /// `==` / `!=` — equality on `String`, `Int`, `Boolean`, `Double`
    /// (with `Int`↔`Double` promotion). Result is `Boolean`.
    Eq,
    Neq,
    /// `<` / `<=` / `>` / `>=` — ordering on `String` and numeric
    /// (`Int`/`Double`, with promotion). Result is `Boolean`.
    Lt,
    Le,
    Gt,
    Ge,
    /// `??` — null-coalesce. LHS must be `T?` (or the bare `null`
    /// literal of type `Null`); RHS must be `T` (or `T?`). Evaluates
    /// RHS only when LHS is `null`. Right-associative.
    Coalesce,
    /// `&&` / `||` — short-circuit logical conjunction / disjunction
    /// over `Boolean`. Both operands must be `Boolean` (no coercion);
    /// the RHS is evaluated only when the LHS doesn't already decide
    /// the result. `&&` binds tighter than `||`; both bind looser than
    /// comparison.
    And,
    Or,
}

impl BinOp {
    #[must_use]
    pub const fn symbol(self) -> &'static str {
        match self {
            Self::Add => "+",
            Self::Sub => "-",
            Self::Mul => "*",
            Self::Div => "/",
            Self::Pow => "**",
            Self::Concat => "++",
            Self::Eq => "==",
            Self::Neq => "!=",
            Self::Lt => "<",
            Self::Le => "<=",
            Self::Gt => ">",
            Self::Ge => ">=",
            Self::Coalesce => "??",
            Self::And => "&&",
            Self::Or => "||",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Type {
    String,
    Int,
    Boolean,
    Double,
    List(Box<Self>),
    /// `Map<K, V>`. Allowed key types are validated at type-check
    /// time: only `String` and `Int` are accepted (Boolean is too
    /// coarse and Double / structured keys make round-tripping
    /// problematic).
    Map(Box<Self>, Box<Self>),
    /// Filesystem symlink. Constructed via the builtin `symlink(...)`
    /// fn; only enters the apply queue via `reconcile`.
    Symlink,
    /// Templated file. Constructed via the builtin
    /// `template(source, target, vars)` fn; at apply time the
    /// substituted text lands at `target`. A "plain" file is just a
    /// template with an empty `vars` map.
    Template,
    /// A package-manager-managed package (brew formula, `cargo
    /// install` binary, winget package, etc.). Constructed via the
    /// per-manager builtins (`brew(...)`, `cargo(...)`,
    /// `winget(...)`); the manager identity is preserved on the
    /// resulting [`crate::IntrinsicId`]-tagged resource so the
    /// executor can pick the right CLI at apply time.
    Package,
    /// Explicit shell execution resource. Constructed via
    /// `shell(kind, name, script)`, stays pure during evaluation, and
    /// runs only during `apply --execute`.
    Shell,
    /// A user-supplied SSH keypair. Constructed via the builtin
    /// `ssh_key(private_path, public_path, private, public)` fn. The
    /// resulting resource carries the encrypted private-key blob in
    /// memory; plan diff renders it as `[sensitive]` unless
    /// `--verbose-will-reveal-sensitive-content` opts in.
    SshKey,
    /// A user-supplied GPG secret-key import. Constructed via the
    /// builtin `gpg_key(fingerprint, key)` fn. The key material is
    /// only ever piped to `gpg --batch --import` over child stdin —
    /// never argv, never a tempfile.
    GpgKey,
    /// Common supertype of [`Self::Symlink`], [`Self::Template`],
    /// [`Self::Package`], [`Self::Shell`], [`Self::SshKey`], and
    /// [`Self::GpgKey`]. There is no constructor — the type only
    /// shows up via annotation (`val r: Resource = symlink(...)`),
    /// list inference for mixed elements
    /// (`[symlink(...), template(...)]` has type `List<Resource>`),
    /// and `Resource`-typed fn signatures. Subtyping is one-way: a
    /// specific resource fits a `Resource` slot, but `Resource` does
    /// not auto-narrow to a specific kind.
    Resource,
    /// A value sourced from an external secret store (e.g.
    /// `secret("op://...")`). `Secret` is **not** a subtype of
    /// `String`: interpolation, concat, and cross-type equality with
    /// `String` are rejected, so a secret cannot accidentally land
    /// in a sink without explicit `unwrap_secret(...)`. The only
    /// non-reflexive operation allowed is `Secret == Secret`.
    Secret,
    /// "No value." The type of an empty block, of a `Void`-returning
    /// function's body, and of an `if` expression used as control
    /// flow. Writable in source as the annotation `Void`.
    Void,
    /// A user-declared `struct Name { field: T, ... }`. Nominal: two
    /// structs with identical fields but different names are distinct
    /// types. Field order is preserved for positional construction.
    Struct {
        name: String,
        fields: Vec<(String, Self)>,
    },
    /// A user-declared `type Name = "a" | "b" | ...` — a nominal
    /// closed enumeration of string literals. Nominal: two unions
    /// with identical variant sets but different names are distinct
    /// types. Variants are unique and stored in source order.
    StringUnion {
        name: String,
        variants: Vec<String>,
    },
    /// A type referenced by name in source, awaiting import
    /// resolution. The parser produces this for any capitalized
    /// identifier in type position that isn't a primitive keyword
    /// (`String`/`Int`/`Boolean`/`Double`/`Void`/`List`/`Map`); the
    /// module loader rewrites it to the canonical variant via the
    /// builtin registry (`Symlink`/`Template`/`Resource`) or
    /// the local module's `struct`/`type` declarations. After
    /// loading, this variant should never appear — the type checker
    /// treats any leftover `Named` as an error.
    Named(String),
    /// The singleton type of the `null` literal. Only appears as the
    /// payload of a [`Self::Nullable`] (post-parser normalization) or
    /// as the inferred type of a bare `null` literal.
    Null,
    /// A nullable wrapper. Spelled `T?` in source. The parser collapses
    /// `T??` to `T?` so this never nests; the type checker treats
    /// `Null` and `Nullable(_)` as the only types `null` can flow into.
    Nullable(Box<Self>),
    /// A type variable used in **intrinsic** signatures. Carrier of
    /// parametric polymorphism for builtins like `len(xs: List<T>) ->
    /// Int`. The parser never produces this — only the stdlib registry
    /// constructs `Type::Generic("T")` inside intrinsic `FnSig`s. The
    /// type checker substitutes it with a concrete type at every call
    /// site (see `bind_generics`/`substitute_generics`), so after
    /// `check_call` returns it should never appear in user-facing
    /// types. Encountering it outside the intrinsic signature path is
    /// a bug — the type resolver and equality / subtyping helpers
    /// treat it as an opaque leaf to keep that invariant loud rather
    /// than papering over it.
    Generic(String),
}

impl fmt::Display for Type {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::String => f.write_str("String"),
            Self::Int => f.write_str("Int"),
            Self::Boolean => f.write_str("Boolean"),
            Self::Double => f.write_str("Double"),
            Self::List(inner) => write!(f, "List<{inner}>"),
            Self::Map(k, v) => write!(f, "Map<{k}, {v}>"),
            Self::Symlink => f.write_str("Symlink"),
            Self::Template => f.write_str("Template"),
            Self::Resource => f.write_str("Resource"),
            Self::Secret => f.write_str("Secret"),
            Self::Package => f.write_str("Package"),
            Self::Shell => f.write_str("Shell"),
            Self::SshKey => f.write_str("SshKey"),
            Self::GpgKey => f.write_str("GpgKey"),
            Self::Void => f.write_str("Void"),
            // Structs and string unions are nominal: print just the
            // declared name. The full field/variant list is only
            // included in dedicated diagnostics.
            Self::Struct { name, .. } | Self::StringUnion { name, .. } | Self::Named(name) => {
                f.write_str(name)
            }
            Self::Null => f.write_str("Null"),
            Self::Nullable(inner) => write!(f, "{inner}?"),
            // The intrinsic-only generic leaks into Display only when
            // a diagnostic prints a partially-substituted signature
            // (a bug, but `{name}` is the most readable rendering).
            Self::Generic(name) => f.write_str(name),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Literal {
    String(String),
    Int(i64),
    Boolean(bool),
    Double(f64),
    /// `null`. Type-checks against [`Type::Null`] and against any
    /// [`Type::Nullable`] slot via the subtyping rules.
    Null,
}

impl Literal {
    #[must_use]
    pub const fn type_of(&self) -> Type {
        match self {
            Self::String(_) => Type::String,
            Self::Int(_) => Type::Int,
            Self::Boolean(_) => Type::Boolean,
            Self::Double(_) => Type::Double,
            Self::Null => Type::Null,
        }
    }
}

// =====================================================================
// Trivia (comments)
//
// Comments are not parsed as tokens — `parser::util::pad` discards
// them along with whitespace, so they never appear in the AST itself.
// To round-trip them through a pretty-printer, we capture them in a
// side `CommentMap` produced by `trivia::extract_comments` and keyed
// by which AST span they attach to. Existing callers that only care
// about parse output (`parser::parse`) are unaffected; only the
// formatter needs the map.
// =====================================================================

/// One `#...` comment captured from source. `text` includes the
/// leading `#` and any whitespace after it but excludes the
/// terminating newline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Comment {
    pub text: String,
    pub span: Span,
}

/// How a comment relates to the AST.
///
/// Resolution order when more than one attachment is plausible (e.g.
/// a comment between two top-level items could be trailing the
/// previous OR leading the next): `Trailing` > `Leading` >
/// `BlockInterior` > `ModuleTrailing`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommentAttachment {
    /// Comment appears immediately before an AST node, separated from
    /// it by whitespace only (blank lines allowed). The held span is
    /// the node's full span.
    Leading(Span),
    /// Comment starts on the same source line as the node ends.
    Trailing(Span),
    /// Comment sits inside a block (`{ ... }`, `[ ... ]`, `( ... )`)
    /// but not adjacent to any specific item. `after = None` means
    /// it precedes the first item; `Some(s)` means it follows the
    /// item with span `s`. `block_span` is the full block including
    /// delimiters.
    BlockInterior {
        block_span: Span,
        after: Option<Span>,
    },
    /// Comment appears after the last top-level item in the module.
    ModuleTrailing,
}

/// All comments extracted from a source file, paired with their
/// attachment policy.
///
/// Stored as a `Vec` rather than a map because the formatter walks
/// comments in source order during emission and doesn't benefit from
/// random-access lookup by span.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CommentMap {
    pub comments: Vec<(Comment, CommentAttachment)>,
}

impl CommentMap {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            comments: Vec::new(),
        }
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.comments.is_empty()
    }
}
