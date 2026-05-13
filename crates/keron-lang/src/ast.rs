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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StructDecl {
    pub name: Spanned<String>,
    pub fields: Vec<StructField>,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StructField {
    pub name: Spanned<String>,
    pub ty: Spanned<Type>,
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
    /// `secret(uri)` — resolve an external secret URI (currently
    /// `op://` only) into a `Secret` value. Eager: the URI is read
    /// at plan-build time and any resolution failure is a hard
    /// error. The returned `Secret` cannot flow into any String sink
    /// without an explicit [`Self::UnwrapSecret`] call.
    Secret,
    /// `unwrap_secret(s)` — convert a `Secret` to a `String`. The
    /// only legal way to extract; every call site is an audit
    /// breadcrumb for "here is where the secret leaves the marker
    /// type and becomes plain text."
    UnwrapSecret,
    /// `brew(name)` — declares a Homebrew formula or cask the apply
    /// step should `brew install`. v1 carries only the name; taps
    /// can be encoded inline (`brew("home/repo/formula")`).
    Brew,
    /// `cargo(name)` — declares a `cargo install` binary. v1
    /// carries only the crate name.
    Cargo,
    /// `winget(name)` — declares a winget package id (e.g.
    /// `"Microsoft.PowerShell"`). v1 carries only the id; sources
    /// can be added later as a second arg.
    Winget,
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

/// One arm in a `match` expression: `pattern => body`.
#[derive(Debug, Clone, PartialEq)]
pub struct MatchArm {
    pub pattern: Spanned<Pattern>,
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
    /// Common supertype of [`Self::Symlink`], [`Self::Template`],
    /// [`Self::Package`], and [`Self::Shell`]. There is no
    /// constructor — the type only shows up via annotation (`val r:
    /// Resource = symlink(...)`), list inference for mixed elements
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
            Self::Void => f.write_str("Void"),
            // Structs and string unions are nominal: print just the
            // declared name. The full field/variant list is only
            // included in dedicated diagnostics.
            Self::Struct { name, .. } | Self::StringUnion { name, .. } | Self::Named(name) => {
                f.write_str(name)
            }
            Self::Null => f.write_str("Null"),
            Self::Nullable(inner) => write!(f, "{inner}?"),
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
