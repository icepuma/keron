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
    Val(ValDecl),
    Fn(FnDecl),
    Reconcile(ReconcileDecl),
    /// A top-level expression evaluated for its effect (e.g.
    /// `if cond { reconcile foo }`). The expression must have type
    /// `Void`; the type checker rejects anything else, which is how
    /// keron prevents pointless top-level computations.
    ExprStmt(Spanned<Expr>),
}

#[derive(Debug, Clone, PartialEq)]
pub struct ReconcileDecl {
    pub expr: Spanned<Expr>,
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
    Expr(Box<Spanned<Expr>>),
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
    /// time: only `String`, `Int`, and `Boolean` are accepted.
    Map(Box<Self>, Box<Self>),
    /// Filesystem symlink. Constructed via the builtin `symlink(...)`
    /// fn; only enters the apply queue via `reconcile`.
    Symlink,
    /// File-with-content. Constructed via the builtin `file(...)` fn.
    File,
    /// Directory ensure-existence. Constructed via the builtin
    /// `directory(...)` fn.
    Directory,
    /// "No value." The type of an empty block, of a `Void`-returning
    /// function's body, and of an `if` expression used as control
    /// flow. Writable in source as the annotation `Void`.
    Void,
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
            Self::File => f.write_str("File"),
            Self::Directory => f.write_str("Directory"),
            Self::Void => f.write_str("Void"),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Literal {
    String(String),
    Int(i64),
    Boolean(bool),
    Double(f64),
}

impl Literal {
    #[must_use]
    pub const fn type_of(&self) -> Type {
        match self {
            Self::String(_) => Type::String,
            Self::Int(_) => Type::Int,
            Self::Boolean(_) => Type::Boolean,
            Self::Double(_) => Type::Double,
        }
    }
}
