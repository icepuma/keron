//! Tree-walking evaluator that turns a checked [`ModuleGraph`] into an
//! ordered list of concrete `ResourceState`s — what `reconcile` would
//! actually act on.
//!
//! Top-level `val`s are evaluated lazily: a binding is only computed
//! when something reachable from a `reconcile` (or top-level
//! `if`/`for`) refers to it. This keeps fixtures like `kitchen_sink`,
//! which define helpers that never participate in apply (e.g. mutual
//! recursion without a base case bound to a `val` that nothing
//! consumes), from blowing the stack at plan time.
//!
//! With imports, top-level state is **per-module**. A name lookup
//! resolves through the importing module's local bindings and val
//! cache first, then crosses an import edge into the origin module's
//! own scope — so an imported function that references its module's
//! own vals sees those, not the importer's. Calls to stdlib
//! intrinsics dispatch on the [`IntrinsicId`] tag carried on the
//! `FnDecl`, not on the function name.
//!
//! The type checker has already proven each module sound, so most
//! "type error" branches here are unreachable in well-typed input but
//! kept as `bail!` rather than `unreachable!` to fail loudly if AST
//! invariants ever drift.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use keron_lang::{
    BinOp, Block, CallArg, Expr, FnDecl, ForPattern, IntrinsicId, Item, Literal, MapEntry,
    MatchArm, Pattern, Spanned, Stmt, StringPart, StructDecl, StructPatternField, UnaryOp,
};
use keron_modules::{ModuleGraph, ModuleId, stdlib};

use crate::plan::{PackageManager, ResourceState};

#[derive(Clone)]
enum Value {
    String {
        text: String,
        sensitive: bool,
    },
    Int(i64),
    Bool(bool),
    Double(f64),
    List(Vec<Self>),
    Map(Vec<(Self, Self)>),
    Resource(ResourceState),
    /// A user-defined struct value. The `name` carries the declared
    /// struct name (used for diagnostics and pattern matching);
    /// `fields` preserves declaration order.
    Struct {
        name: String,
        fields: Vec<(String, Self)>,
    },
    Void,
    /// The single inhabitant of `Type::Null` and the absent end of any
    /// `Type::Nullable(_)`. Constructed only by evaluating the `null`
    /// literal — there's no runtime path that produces `Null`
    /// implicitly.
    Null,
    /// A value sourced from a secret store via `secret("op://...")`.
    /// The payload is the resolved plaintext; the `Debug` impl
    /// redacts it so a `dbg!`, panic backtrace, or any auto-derived
    /// `Debug` further up the stack can't leak the value.
    /// `unwrap_secret(...)` is the only way to extract the payload
    /// back into a `Value::String`.
    Secret(String),
}

// Manual `Debug` so `Value::Secret` redacts its payload. Every other
// variant defers to the same shape `#[derive(Debug)]` would have
// produced — this is a one-arm carve-out, nothing more.
impl std::fmt::Debug for Value {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::String { text, sensitive } => {
                if *sensitive {
                    write!(f, "String(<sensitive, {} bytes>)", text.len())
                } else {
                    f.debug_tuple("String").field(text).finish()
                }
            }
            Self::Int(n) => f.debug_tuple("Int").field(n).finish(),
            Self::Bool(b) => f.debug_tuple("Bool").field(b).finish(),
            Self::Double(d) => f.debug_tuple("Double").field(d).finish(),
            Self::List(xs) => f.debug_tuple("List").field(xs).finish(),
            Self::Map(entries) => f.debug_tuple("Map").field(entries).finish(),
            Self::Resource(r) => f.debug_tuple("Resource").field(r).finish(),
            Self::Struct { name, fields } => f
                .debug_struct("Struct")
                .field("name", name)
                .field("fields", fields)
                .finish(),
            Self::Void => f.write_str("Void"),
            Self::Null => f.write_str("Null"),
            Self::Secret(s) => write!(f, "Secret(<redacted, {} bytes>)", s.len()),
        }
    }
}

impl Value {
    fn plain_string(text: impl Into<String>) -> Self {
        Self::String {
            text: text.into(),
            sensitive: false,
        }
    }

    fn sensitive_string(text: impl Into<String>) -> Self {
        Self::String {
            text: text.into(),
            sensitive: true,
        }
    }

    fn type_name(&self) -> String {
        match self {
            Self::String { .. } => "String".into(),
            Self::Int(_) => "Int".into(),
            Self::Bool(_) => "Boolean".into(),
            Self::Double(_) => "Double".into(),
            Self::List(_) => "List".into(),
            Self::Map(_) => "Map".into(),
            Self::Resource(_) => "Resource".into(),
            Self::Struct { name, .. } => name.clone(),
            Self::Void => "Void".into(),
            Self::Null => "Null".into(),
            Self::Secret(_) => "Secret".into(),
        }
    }
}

/// Per-module top-level state. Each module gets one of these: vals
/// memoised lazily, fns hoisted from the AST, plus a cycle-detector
/// for in-progress evaluations.
struct ModuleTop<'p> {
    val_decls: HashMap<String, &'p Spanned<Expr>>,
    fns: HashMap<String, &'p FnDecl>,
    /// Struct decls in this module, keyed by the struct's name. Used
    /// by `eval_call` to dispatch construction calls before falling
    /// through to fn lookup.
    structs: HashMap<String, &'p StructDecl>,
    cache: RefCell<HashMap<String, Value>>,
    in_progress: RefCell<HashSet<String>>,
    /// `local_name` → (`origin_module`, `original_name`).
    imports: HashMap<String, (ModuleId, String)>,
}

/// Cross-module top-level state. Owns one [`ModuleTop`] per module
/// reachable from the entry; the evaluator's [`Env`] points back into
/// this with the current module's identity.
struct GraphTop<'p> {
    modules: HashMap<ModuleId, ModuleTop<'p>>,
    /// Canonical absolute path the user passed to `keron apply` —
    /// surfaced to user code through the `keron_root()` builtin.
    keron_root: PathBuf,
}

#[derive(Clone)]
struct Env<'a, 'p> {
    graph: &'a GraphTop<'p>,
    current: ModuleId,
    /// Lexically-scoped bindings (function params, block-local vals,
    /// loop bindings). Take priority over top-level lookup.
    local: HashMap<String, Value>,
}

impl<'a, 'p> Env<'a, 'p> {
    fn new(graph: &'a GraphTop<'p>, current: ModuleId) -> Self {
        Self {
            graph,
            current,
            local: HashMap::new(),
        }
    }

    fn extended(&self, name: String, value: Value) -> Self {
        let mut local = self.local.clone();
        local.insert(name, value);
        Self {
            graph: self.graph,
            current: self.current.clone(),
            local,
        }
    }

    fn current_module(&self) -> &ModuleTop<'p> {
        self.graph
            .modules
            .get(&self.current)
            .expect("current module must exist in graph")
    }

    fn lookup(&self, name: &str) -> Result<Value> {
        if let Some(v) = self.local.get(name) {
            return Ok(v.clone());
        }
        let module = self.current_module();
        if let Some(v) = module.cache.borrow().get(name) {
            return Ok(v.clone());
        }
        if let Some(expr) = module.val_decls.get(name) {
            let key = name.to_string();
            if !module.in_progress.borrow_mut().insert(key.clone()) {
                bail!("cycle while evaluating `val {name}`");
            }
            let module_env = Env::new(self.graph, self.current.clone());
            let v = eval_expr(expr, &module_env)?;
            module.in_progress.borrow_mut().remove(&key);
            module.cache.borrow_mut().insert(key, v.clone());
            return Ok(v);
        }
        if let Some((origin, original)) = module.imports.get(name) {
            // Resolve the imported name in its defining module's
            // scope. For a fn, an importer that calls `name(...)`
            // hits `eval_call` — so this branch only fires for vals.
            let cross = Env::new(self.graph, origin.clone());
            return cross.lookup(original);
        }
        Err(anyhow!("unknown name `{name}`"))
    }
}

pub fn eval_graph(graph: &ModuleGraph, keron_root: &Path) -> Result<Vec<ResourceState>> {
    let mut graph_top = GraphTop {
        modules: HashMap::new(),
        keron_root: keron_root.to_path_buf(),
    };
    for (id, module) in &graph.modules {
        let mut top = ModuleTop {
            val_decls: HashMap::new(),
            fns: HashMap::new(),
            structs: HashMap::new(),
            cache: RefCell::new(HashMap::new()),
            in_progress: RefCell::new(HashSet::new()),
            imports: module.imports.clone(),
        };
        for item in &module.program.items {
            match item {
                Item::Val(v) => {
                    top.val_decls.insert(v.name.node.clone(), &v.value);
                }
                Item::Fn(f) => {
                    top.fns.insert(f.name.node.clone(), f);
                }
                Item::Struct(s) => {
                    top.structs.insert(s.name.node.clone(), s);
                }
                Item::Use(_) | Item::TypeAlias(_) | Item::Reconcile(_) | Item::ExprStmt(_) => {}
            }
        }
        graph_top.modules.insert(id.clone(), top);
    }

    // Evaluate each module's reconciles in topological order:
    // dependencies' side effects fire before dependents'. A library
    // imported twice still has its reconciles evaluated once (each
    // module appears once in `topo_order`).
    let mut out = Vec::new();
    for id in &graph.topo_order {
        let module = graph
            .modules
            .get(id)
            .expect("topo_order must reference existing modules");
        let env = Env::new(&graph_top, id.clone());
        for item in &module.program.items {
            match item {
                Item::Use(_)
                | Item::Val(_)
                | Item::Fn(_)
                | Item::Struct(_)
                | Item::TypeAlias(_) => {}
                Item::Reconcile(r) => {
                    for chain in &r.chains {
                        for expr in chain {
                            let v = eval_expr(expr, &env)?;
                            push_resources(v, &mut out)?;
                        }
                    }
                }
                Item::ExprStmt(expr) => exec_void_expr(expr, &env, &mut out)?,
            }
        }
    }
    Ok(out)
}

fn push_resources(v: Value, out: &mut Vec<ResourceState>) -> Result<()> {
    match v {
        Value::Resource(r) => {
            out.push(r);
            Ok(())
        }
        Value::List(items) => {
            for item in items {
                push_resources(item, out)?;
            }
            Ok(())
        }
        other => bail!(
            "expected Resource or List<Resource> in `reconcile`, got {}",
            other.type_name()
        ),
    }
}

fn exec_void_expr(
    expr: &Spanned<Expr>,
    env: &Env<'_, '_>,
    out: &mut Vec<ResourceState>,
) -> Result<()> {
    match &expr.node {
        Expr::If {
            cond,
            then_branch,
            else_branch,
        } => {
            let c = eval_expr(cond, env)?;
            let Value::Bool(b) = c else {
                bail!("`if` condition was {} (expected Boolean)", c.type_name());
            };
            let block: &Block = if b { then_branch } else { else_branch };
            exec_void_block(block, env, out)
        }
        Expr::For {
            pattern,
            iter_expr,
            body,
        } => {
            let iterable = eval_expr(iter_expr, env)?;
            iterate(&iterable, pattern, env, body, out)
        }
        _ => bail!("expected `if` or `for` at top level"),
    }
}

fn exec_void_block(block: &Block, env: &Env<'_, '_>, out: &mut Vec<ResourceState>) -> Result<()> {
    let mut local = env.clone();
    for stmt in &block.stmts {
        match stmt {
            Stmt::Val(v) => {
                let val = eval_expr(&v.value, &local)?;
                local = local.extended(v.name.node.clone(), val);
            }
            Stmt::Reconcile(r) => {
                for chain in &r.chains {
                    for expr in chain {
                        let v = eval_expr(expr, &local)?;
                        push_resources(v, out)?;
                    }
                }
            }
        }
    }
    if let Some(trailing) = &block.trailing {
        exec_void_expr(trailing, &local, out)?;
    }
    Ok(())
}

fn iterate(
    iterable: &Value,
    pattern: &ForPattern,
    env: &Env<'_, '_>,
    body: &Block,
    out: &mut Vec<ResourceState>,
) -> Result<()> {
    match (iterable, pattern) {
        (Value::List(items), ForPattern::Elem(name)) => {
            for item in items {
                let scoped = env.extended(name.node.clone(), item.clone());
                exec_void_block(body, &scoped, out)?;
            }
            Ok(())
        }
        (Value::Map(entries), ForPattern::Entry { key, value }) => {
            for (k, v) in entries {
                let scoped = env
                    .extended(key.node.clone(), k.clone())
                    .extended(value.node.clone(), v.clone());
                exec_void_block(body, &scoped, out)?;
            }
            Ok(())
        }
        (other, _) => bail!("`for` over {} is not supported", other.type_name()),
    }
}

fn eval_expr(expr: &Spanned<Expr>, env: &Env<'_, '_>) -> Result<Value> {
    match &expr.node {
        Expr::Literal(lit) => Ok(eval_literal(lit)),
        Expr::Unary { op, operand } => eval_unary(*op, eval_expr(operand, env)?),
        Expr::Binary { op, lhs, rhs } => {
            let l = eval_expr(lhs, env)?;
            let r = eval_expr(rhs, env)?;
            eval_binop(*op, l, r)
        }
        Expr::Interpolation(parts) => eval_interpolation(parts, env),
        Expr::List(items) => {
            let mut vals = Vec::with_capacity(items.len());
            for it in items {
                vals.push(eval_expr(it, env)?);
            }
            Ok(Value::List(vals))
        }
        Expr::Map(entries) => {
            let mut pairs = Vec::with_capacity(entries.len());
            for MapEntry { key, value, .. } in entries {
                pairs.push((eval_expr(key, env)?, eval_expr(value, env)?));
            }
            Ok(Value::Map(pairs))
        }
        Expr::Var(name) => env.lookup(name),
        Expr::Call { callee, args } => eval_call(&callee.node, args, env),
        Expr::If {
            cond,
            then_branch,
            else_branch,
        } => {
            let c = eval_expr(cond, env)?;
            let Value::Bool(b) = c else {
                bail!("`if` condition was {} (expected Boolean)", c.type_name());
            };
            let block: &Block = if b { then_branch } else { else_branch };
            let mut sink = Vec::new();
            eval_block_value(block, env, &mut sink)
        }
        Expr::For { .. } => bail!("`for` is not a value expression"),
        Expr::Field { receiver, field } => {
            let v = eval_expr(receiver, env)?;
            match v {
                Value::Struct { name, fields } => fields
                    .into_iter()
                    .find(|(n, _)| n == &field.node)
                    .map(|(_, val)| val)
                    .ok_or_else(|| anyhow!("struct `{name}` has no field `{}`", field.node)),
                other => bail!(
                    "field access requires a struct, found {} for `.{}`",
                    other.type_name(),
                    field.node
                ),
            }
        }
        Expr::Match { scrutinee, arms } => eval_match(scrutinee, arms, env),
    }
}

/// Evaluate a `match` expression: try each arm in source order; the
/// first pattern that succeeds wins. The type checker has already
/// proven exhaustiveness, so a fall-through here means the AST and
/// the checker disagree — surface it as an error rather than panic.
fn eval_match(scrutinee: &Spanned<Expr>, arms: &[MatchArm], env: &Env<'_, '_>) -> Result<Value> {
    let val = eval_expr(scrutinee, env)?;
    for arm in arms {
        let mut bindings: HashMap<String, Value> = HashMap::new();
        if try_match_pattern(&arm.pattern.node, &val, &mut bindings) {
            let mut arm_env = env.clone();
            for (n, v) in bindings {
                arm_env.local.insert(n, v);
            }
            return eval_expr(&arm.body, &arm_env);
        }
    }
    bail!("no `match` arm matched value of type {}", val.type_name())
}

fn try_match_pattern(
    pattern: &Pattern,
    value: &Value,
    bindings: &mut HashMap<String, Value>,
) -> bool {
    match pattern {
        Pattern::Wildcard => true,
        Pattern::Bind(name) => {
            bindings.insert(name.clone(), value.clone());
            true
        }
        // Literal patterns mirror `==` semantics: `value_eq` is the
        // single source of truth so a `match` arm and a `x == lit`
        // test always agree on equality (including the Int↔Double
        // promotion rules and NaN-safe Double comparisons).
        Pattern::Lit(lit) => value_eq(&eval_literal(lit), value),
        Pattern::Struct { name, fields } => match_struct_pattern(name, fields, value, bindings),
    }
}

fn match_struct_pattern(
    name: &Spanned<String>,
    fields: &[StructPatternField],
    value: &Value,
    bindings: &mut HashMap<String, Value>,
) -> bool {
    let Value::Struct {
        name: vname,
        fields: vfields,
    } = value
    else {
        return false;
    };
    if vname != &name.node {
        return false;
    }
    for f in fields {
        let Some((_, fval)) = vfields.iter().find(|(n, _)| n == &f.name.node) else {
            return false;
        };
        match &f.pattern {
            Some(sub) => {
                if !try_match_pattern(&sub.node, fval, bindings) {
                    return false;
                }
            }
            None => {
                bindings.insert(f.name.node.clone(), fval.clone());
            }
        }
    }
    true
}

fn eval_block_value(
    block: &Block,
    env: &Env<'_, '_>,
    sink: &mut Vec<ResourceState>,
) -> Result<Value> {
    let mut local = env.clone();
    for stmt in &block.stmts {
        match stmt {
            Stmt::Val(v) => {
                let val = eval_expr(&v.value, &local)?;
                local = local.extended(v.name.node.clone(), val);
            }
            Stmt::Reconcile(r) => {
                for chain in &r.chains {
                    for expr in chain {
                        let v = eval_expr(expr, &local)?;
                        push_resources(v, sink)?;
                    }
                }
            }
        }
    }
    let Some(trailing) = &block.trailing else {
        return Ok(Value::Void);
    };
    eval_expr(trailing, &local)
}

fn eval_literal(lit: &Literal) -> Value {
    match lit {
        Literal::String(s) => Value::plain_string(s.clone()),
        Literal::Int(n) => Value::Int(*n),
        Literal::Boolean(b) => Value::Bool(*b),
        Literal::Double(d) => Value::Double(*d),
        Literal::Null => Value::Null,
    }
}

fn eval_unary(op: UnaryOp, v: Value) -> Result<Value> {
    match (op, v) {
        (UnaryOp::Neg, Value::Int(n)) => Ok(Value::Int(-n)),
        (UnaryOp::Neg, Value::Double(d)) => Ok(Value::Double(-d)),
        (op, v) => bail!("unary `{}` on {}", op.symbol(), v.type_name()),
    }
}

#[allow(clippy::cast_precision_loss, clippy::too_many_lines)]
fn eval_binop(op: BinOp, l: Value, r: Value) -> Result<Value> {
    use BinOp::{Add, Concat, Div, Eq, Ge, Gt, Le, Lt, Mul, Neq, Pow, Sub};
    match (op, l, r) {
        (
            Add,
            Value::String {
                text: a,
                sensitive: sa,
            },
            Value::String {
                text: b,
                sensitive: sb,
            },
        ) => Ok(Value::String {
            text: a + &b,
            sensitive: sa || sb,
        }),
        (Add, Value::Int(a), Value::Int(b)) => Ok(Value::Int(a + b)),
        (Sub, Value::Int(a), Value::Int(b)) => Ok(Value::Int(a - b)),
        (Mul, Value::Int(a), Value::Int(b)) => Ok(Value::Int(a * b)),
        (Div, Value::Int(a), Value::Int(b)) => {
            if b == 0 {
                bail!("division by zero");
            }
            Ok(Value::Int(a / b))
        }
        (Pow, Value::Int(a), Value::Int(b)) => {
            let exp = u32::try_from(b).context("`**` exponent does not fit in u32")?;
            Ok(Value::Int(a.pow(exp)))
        }
        (Add, Value::Double(a), Value::Double(b)) => Ok(Value::Double(a + b)),
        (Sub, Value::Double(a), Value::Double(b)) => Ok(Value::Double(a - b)),
        (Mul, Value::Double(a), Value::Double(b)) => Ok(Value::Double(a * b)),
        (Div, Value::Double(a), Value::Double(b)) => Ok(Value::Double(a / b)),
        (Pow, Value::Double(a), Value::Double(b)) => Ok(Value::Double(a.powf(b))),
        (Add, Value::Int(a), Value::Double(b)) => Ok(Value::Double(a as f64 + b)),
        (Add, Value::Double(a), Value::Int(b)) => Ok(Value::Double(a + b as f64)),
        (Sub, Value::Int(a), Value::Double(b)) => Ok(Value::Double(a as f64 - b)),
        (Sub, Value::Double(a), Value::Int(b)) => Ok(Value::Double(a - b as f64)),
        (Mul, Value::Int(a), Value::Double(b)) => Ok(Value::Double(a as f64 * b)),
        (Mul, Value::Double(a), Value::Int(b)) => Ok(Value::Double(a * b as f64)),
        (Div, Value::Int(a), Value::Double(b)) => Ok(Value::Double(a as f64 / b)),
        (Div, Value::Double(a), Value::Int(b)) => Ok(Value::Double(a / b as f64)),
        (Pow, Value::Int(a), Value::Double(b)) => Ok(Value::Double((a as f64).powf(b))),
        #[allow(clippy::cast_possible_truncation)]
        (Pow, Value::Double(a), Value::Int(b)) => Ok(Value::Double(a.powi(b as i32))),

        (Concat, Value::List(mut a), Value::List(b)) => {
            a.extend(b);
            Ok(Value::List(a))
        }

        (Eq, a, b) => Ok(Value::Bool(value_eq(&a, &b))),
        (Neq, a, b) => Ok(Value::Bool(!value_eq(&a, &b))),
        (Lt, a, b) => Ok(Value::Bool(value_cmp(&a, &b)? == std::cmp::Ordering::Less)),
        (Le, a, b) => Ok(Value::Bool(
            value_cmp(&a, &b)? != std::cmp::Ordering::Greater,
        )),
        (Gt, a, b) => Ok(Value::Bool(
            value_cmp(&a, &b)? == std::cmp::Ordering::Greater,
        )),
        (Ge, a, b) => Ok(Value::Bool(value_cmp(&a, &b)? != std::cmp::Ordering::Less)),

        (op, l, r) => bail!(
            "binary `{}` on {} / {}",
            op.symbol(),
            l.type_name(),
            r.type_name()
        ),
    }
}

#[allow(clippy::cast_precision_loss)]
fn value_eq(a: &Value, b: &Value) -> bool {
    match (a, b) {
        // String and Secret share an inner-string equality body.
        // The checker only admits `Secret == Secret` (no
        // String↔Secret cross-type), so the merged arm is safe even
        // though semantically these are distinct rules.
        (Value::String { text: x, .. }, Value::String { text: y, .. })
        | (Value::Secret(x), Value::Secret(y)) => x == y,
        (Value::Int(x), Value::Int(y)) => x == y,
        (Value::Bool(x), Value::Bool(y)) => x == y,
        (Value::Double(x), Value::Double(y)) => x == y,
        (Value::Int(x), Value::Double(y)) => (*x as f64) == *y,
        (Value::Double(x), Value::Int(y)) => *x == (*y as f64),
        // The type checker only lets `null` reach `==` on the other
        // side of a `T?`, so anything-vs-null is a real check: it's
        // true iff the other operand is also null. The wildcard
        // catches every cross-type pairing (which the checker has
        // already rejected) plus the `Null` vs non-null cases — both
        // false.
        (Value::Null, Value::Null) => true,
        _ => false,
    }
}

#[allow(clippy::cast_precision_loss)]
fn value_cmp(a: &Value, b: &Value) -> Result<std::cmp::Ordering> {
    match (a, b) {
        (Value::Int(x), Value::Int(y)) => Ok(x.cmp(y)),
        (Value::Double(x), Value::Double(y)) => {
            x.partial_cmp(y).ok_or_else(|| anyhow!("NaN comparison"))
        }
        (Value::Int(x), Value::Double(y)) => (*x as f64)
            .partial_cmp(y)
            .ok_or_else(|| anyhow!("NaN comparison")),
        (Value::Double(x), Value::Int(y)) => x
            .partial_cmp(&(*y as f64))
            .ok_or_else(|| anyhow!("NaN comparison")),
        (Value::String { text: x, .. }, Value::String { text: y, .. }) => Ok(x.cmp(y)),
        (a, b) => bail!("ordering on {} / {}", a.type_name(), b.type_name()),
    }
}

fn eval_interpolation(parts: &[StringPart], env: &Env<'_, '_>) -> Result<Value> {
    let mut out = String::new();
    let mut sensitive = false;
    for part in parts {
        match part {
            StringPart::Text(s) => out.push_str(s),
            StringPart::Expr(e) => {
                let v = eval_expr(e, env)?;
                sensitive |= stringify(&v, &mut out)?;
            }
        }
    }
    Ok(Value::String {
        text: out,
        sensitive,
    })
}

fn stringify(v: &Value, out: &mut String) -> Result<bool> {
    use std::fmt::Write as _;
    match v {
        Value::String { text, sensitive } => {
            out.push_str(text);
            Ok(*sensitive)
        }
        Value::Int(n) => {
            let _ = write!(out, "{n}");
            Ok(false)
        }
        Value::Bool(b) => {
            out.push_str(if *b { "true" } else { "false" });
            Ok(false)
        }
        Value::Double(d) => {
            let _ = write!(out, "{d}");
            Ok(false)
        }
        other => bail!("cannot interpolate {}", other.type_name()),
    }
}

/// Resolve a callee through (1) the current module's `from … use …`
/// imports, (2) the current module's own fns / structs, and (3) the
/// implicit stdlib builtin registry, then dispatch. Intrinsic fns
/// (carried via [`FnDecl::intrinsic`]) and struct constructors
/// bypass body evaluation.
fn eval_call(name: &str, args: &[CallArg], env: &Env<'_, '_>) -> Result<Value> {
    let module = env.current_module();
    // Cross-module struct construction: an imported `Point` resolves
    // to a struct in its origin module.
    if let Some((origin, original)) = module.imports.get(name)
        && let Some(origin_mod) = env.graph.modules.get(origin)
        && let Some(decl) = origin_mod.structs.get(original)
    {
        return construct_struct(decl, args, env);
    }
    if let Some(decl) = module.structs.get(name) {
        return construct_struct(decl, args, env);
    }
    let (origin_id, fn_decl): (ModuleId, &FnDecl) =
        if let Some((origin, original)) = module.imports.get(name) {
            let origin_mod = env
                .graph
                .modules
                .get(origin)
                .ok_or_else(|| anyhow!("origin module for `{name}` not in graph"))?;
            let decl = *origin_mod.fns.get(original).ok_or_else(|| {
                anyhow!(
                    "imported `{name}` resolves to `{}` in `{}`, not present at eval time",
                    original,
                    origin.display()
                )
            })?;
            (origin.clone(), decl)
        } else if let Some(decl) = module.fns.get(name) {
            (env.current.clone(), *decl)
        } else if let Some(decl) = builtin_fn(name) {
            // Builtins are always intrinsic-tagged, so the body path is
            // never taken; the "origin" we return is irrelevant but
            // must be a real module in the graph for `Env::new` to
            // succeed if the body path were ever reached.
            (env.current.clone(), decl)
        } else {
            return Err(anyhow!("unknown function `{name}`"));
        };

    if let Some(intrinsic) = fn_decl.intrinsic {
        return dispatch_intrinsic(intrinsic, args, env);
    }

    let mut call_env = Env::new(env.graph, origin_id);
    bind_params(fn_decl, args, env, &mut call_env)?;

    let mut sink = Vec::new();
    let v = eval_block_value(&fn_decl.body, &call_env, &mut sink)?;
    Ok(v)
}

/// Construct a struct value: bind each declared field by name (named
/// arg) or by position (positional arg), then assemble a
/// [`Value::Struct`]. Argument resolution mirrors [`bind_params`] —
/// the type checker has already validated counts and types so a hit
/// here is well-typed by construction.
fn construct_struct(decl: &StructDecl, args: &[CallArg], env: &Env<'_, '_>) -> Result<Value> {
    let mut fields: Vec<(String, Value)> = Vec::with_capacity(decl.fields.len());
    let mut positional = args.iter().filter(|a| a.name.is_none());
    for field in &decl.fields {
        let named = args
            .iter()
            .find(|a| a.name.as_ref().is_some_and(|n| n.node == field.name.node));
        let value = if let Some(arg) = named {
            eval_expr(&arg.value, env)?
        } else if let Some(arg) = positional.next() {
            eval_expr(&arg.value, env)?
        } else {
            bail!(
                "missing argument for field `{}` of struct `{}`",
                field.name.node,
                decl.name.node
            );
        };
        fields.push((field.name.node.clone(), value));
    }
    Ok(Value::Struct {
        name: decl.name.node.clone(),
        fields,
    })
}

fn builtin_fn(name: &str) -> Option<&'static FnDecl> {
    stdlib::registry()
        .values()
        .find_map(|stdmod| stdmod.fns.get(name))
}

fn dispatch_intrinsic(id: IntrinsicId, args: &[CallArg], env: &Env<'_, '_>) -> Result<Value> {
    match id {
        IntrinsicId::Symlink => {
            let from = call_string(args, env, "from", 0)?;
            let to = call_string(args, env, "to", 1)?;
            let target = resolve_managed_path(&to, env, "symlink", "to")?;
            Ok(Value::Resource(ResourceState::Symlink {
                from: PathBuf::from(from),
                to: target,
            }))
        }
        IntrinsicId::Template => dispatch_template(args, env),
        IntrinsicId::KeronRoot => Ok(Value::plain_string(
            env.graph.keron_root.to_string_lossy().into_owned(),
        )),
        IntrinsicId::OsType => Ok(Value::plain_string(detect_os_type())),
        IntrinsicId::OsArch => Ok(Value::plain_string(detect_os_arch())),
        IntrinsicId::Env => {
            let name = call_string(args, env, "name", 0)?;
            // `env::var` errs both for "not present" and for "not
            // valid Unicode". We collapse both onto `null` rather
            // than surfacing the latter as a hard error: a user
            // who'd want to distinguish them can read the var via a
            // host-side wrapper. Matches what most config DSLs do.
            Ok(std::env::var(&name).map_or(Value::Null, Value::plain_string))
        }
        IntrinsicId::Secret => {
            let uri = call_string(args, env, "uri", 0)?;
            let value =
                resolve_secret(&uri).with_context(|| format!("resolving secret `{uri}`"))?;
            Ok(Value::Secret(value))
        }
        IntrinsicId::UnwrapSecret => {
            // The type checker has proven the argument is `Secret`,
            // so `Value::String` / other variants are unreachable.
            // We `bail!` instead of `unreachable!` so an AST drift
            // shows up as a loud error at apply time rather than a
            // panic.
            let v = eval_call_arg(args, env, "s", 0)?;
            match v {
                Value::Secret(s) => Ok(Value::sensitive_string(s)),
                other => bail!(
                    "unwrap_secret expected `Secret`, found `{}`",
                    other.type_name()
                ),
            }
        }
        IntrinsicId::Brew => dispatch_package(args, env, PackageManager::Brew),
        IntrinsicId::Cargo => dispatch_package(args, env, PackageManager::Cargo),
        IntrinsicId::Winget => dispatch_package(args, env, PackageManager::Winget),
    }
}

/// Construct a `Package` resource. Each of the three package
/// constructors (`brew`/`cargo`/`winget`) routes through here with
/// the manager identity preselected; the only argument is the
/// package name, validated by the type checker as a `String`.
fn dispatch_package(args: &[CallArg], env: &Env<'_, '_>, manager: PackageManager) -> Result<Value> {
    let name = call_string(args, env, "name", 0)?;
    if name.is_empty() {
        bail!("{} package name must not be empty", manager.label());
    }
    Ok(Value::Resource(ResourceState::Package { manager, name }))
}

/// Dispatch a `secret(uri)` call to the right resolver based on the
/// scheme prefix. Failure to parse, run, or interpret the underlying
/// CLI is a hard error — there's no "gracefully missing secret" use
/// case.
///
/// The supported-schemes list is the canonical reference; adding a
/// new provider means one new arm and one CLI wrapper below.
fn resolve_secret(uri: &str) -> Result<String> {
    // Test seam: a per-URI override short-circuits all real CLI
    // shell-outs. Keyed on the full URI so a single registry covers
    // every scheme uniformly. Production builds skip this entirely.
    #[cfg(test)]
    if let Some(v) = secret_test::lookup_override(uri) {
        return v.map_err(|msg| anyhow!("{msg}"));
    }

    if uri.starts_with("op://") {
        return real_resolve_op(uri);
    }
    if let Some(rest) = uri.strip_prefix("infisical://") {
        return real_resolve_infisical(uri, rest);
    }
    if let Some(rest) = uri.strip_prefix("bw://") {
        return real_resolve_bw(uri, rest);
    }
    bail!("unsupported secret URI scheme in `{uri}`; supported schemes: op://, infisical://, bw://")
}

/// Shell out to the 1Password CLI for `op://Vault/Item/field` URIs.
/// `op read` accepts the URI verbatim; stdout is the secret value
/// with one trailing newline stripped (matching how the CLI prints).
/// The function itself is `#[mutants::skip]` because the
/// `Command::new("op")` invocation can't be exercised in tests
/// without the CLI on `$PATH`; the testable logic — status / stdout
/// decoding — lives in [`decode_op_output`].
#[cfg_attr(test, mutants::skip)]
fn real_resolve_op(uri: &str) -> Result<String> {
    let output = std::process::Command::new("op")
        .arg("read")
        .arg(uri)
        .output()
        .with_context(|| format!("invoking `op` for `{uri}` (is the 1Password CLI installed?)"))?;
    decode_op_output(uri, output)
}

/// Decode the `Output` of `op read <uri>` into the secret value or a
/// contextful error. Split out from [`real_resolve_op`] so the
/// status-success / stderr-on-failure branches stay testable from a
/// host that doesn't ship `op` — tests build synthetic
/// [`std::process::Output`] values and assert the rendered error.
fn decode_op_output(uri: &str, output: std::process::Output) -> Result<String> {
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("`op read {uri}` failed: {}", stderr.trim());
    }
    take_stdout(output.stdout, &format!("op read {uri}"))
}

/// Shell out to the Infisical CLI for `infisical://<env>/<name>`
/// URIs. The CLI is invoked as `infisical secrets get <name> --env
/// <env> --plain`; project ID and path are taken from the
/// `INFISICAL_PROJECT_ID` / `INFISICAL_PATH` env vars the CLI
/// already reads, so configs don't have to encode them.
///
/// The `Command` invocation can't be exercised in tests so the
/// function is `#[mutants::skip]`; the URI parser and output decoder
/// live in [`parse_infisical_uri`] and [`decode_infisical_output`].
#[cfg_attr(test, mutants::skip)]
fn real_resolve_infisical(uri: &str, rest: &str) -> Result<String> {
    let (env, name) = parse_infisical_uri(uri, rest)?;
    let output = std::process::Command::new("infisical")
        .arg("secrets")
        .arg("get")
        .arg(name)
        .arg("--env")
        .arg(env)
        .arg("--plain")
        .output()
        .with_context(|| {
            format!("invoking `infisical` for `{uri}` (is the Infisical CLI installed?)")
        })?;
    decode_infisical_output(env, name, output)
}

/// Pull `(env, name)` out of an `infisical://<env>/<name>` URI.
/// Both halves must be non-empty; anything else is a parse error
/// before any CLI is invoked.
fn parse_infisical_uri<'a>(uri: &str, rest: &'a str) -> Result<(&'a str, &'a str)> {
    rest.split_once('/')
        .filter(|(env, name)| !env.is_empty() && !name.is_empty())
        .ok_or_else(|| anyhow!("infisical URI must be `infisical://<env>/<name>`, got `{uri}`"))
}

/// Decode the `Output` of `infisical secrets get` into the secret
/// value or a contextful error. Symmetric to [`decode_op_output`].
fn decode_infisical_output(env: &str, name: &str, output: std::process::Output) -> Result<String> {
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "`infisical secrets get {name} --env {env} --plain` failed: {}",
            stderr.trim()
        );
    }
    take_stdout(
        output.stdout,
        &format!("infisical secrets get {name} --env {env} --plain"),
    )
}

/// Shell out to the Bitwarden CLI for `bw://<item>` or
/// `bw://<item>/<field>` URIs. The default field is `password`; the
/// extended form lets a config pick `username`, `totp`, `notes`, or
/// any other field `bw get` accepts. The session must already be
/// unlocked (`bw unlock` or `BW_SESSION`); we don't attempt to
/// prompt at plan time.
///
/// `#[mutants::skip]` for the same reason as the other resolvers;
/// the testable surface lives in [`parse_bw_uri`] and
/// [`decode_bw_output`].
#[cfg_attr(test, mutants::skip)]
fn real_resolve_bw(uri: &str, rest: &str) -> Result<String> {
    let (item, field) = parse_bw_uri(uri, rest)?;
    let output = std::process::Command::new("bw")
        .arg("get")
        .arg(field)
        .arg(item)
        .output()
        .with_context(|| {
            format!("invoking `bw` for `{uri}` (is the Bitwarden CLI installed and unlocked?)")
        })?;
    decode_bw_output(item, field, output)
}

/// Pull `(item, field)` out of a `bw://<item>[/<field>]` URI. The
/// field defaults to `"password"` when only the item is given.
/// Empty item or empty field is an error.
fn parse_bw_uri<'a>(uri: &str, rest: &'a str) -> Result<(&'a str, &'a str)> {
    if rest.is_empty() {
        bail!("bitwarden URI must be `bw://<item>[/<field>]`, got `{uri}`");
    }
    let (item, field) = rest
        .split_once('/')
        .map_or((rest, "password"), |(item, field)| (item, field));
    if item.is_empty() || field.is_empty() {
        bail!("bitwarden URI must be `bw://<item>[/<field>]`, got `{uri}`");
    }
    Ok((item, field))
}

/// Decode the `Output` of `bw get <field> <item>` into the secret
/// value or a contextful error.
fn decode_bw_output(item: &str, field: &str, output: std::process::Output) -> Result<String> {
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("`bw get {field} {item}` failed: {}", stderr.trim());
    }
    take_stdout(output.stdout, &format!("bw get {field} {item}"))
}

/// Convert the captured stdout of a secret-fetching command into a
/// `String`, trimming exactly one trailing newline. Centralizes the
/// "UTF-8 + newline normalization" rule so each provider stays a
/// one-liner around `Command::output`.
fn take_stdout(bytes: Vec<u8>, command_desc: &str) -> Result<String> {
    let mut value = String::from_utf8(bytes)
        .with_context(|| format!("`{command_desc}` produced non-UTF-8 output"))?;
    if value.ends_with('\n') {
        value.pop();
    }
    Ok(value)
}

#[cfg(test)]
mod secret_test {
    //! Test-only shim that lets eval-side e2e tests inject a fixed
    //! response for any secret URI without invoking the real CLI.
    //! The override map is scheme-agnostic — `op://`, `infisical://`,
    //! and `bw://` all flow through the same lookup so adding a new
    //! provider doesn't need a new test seam. Thread-local so
    //! concurrent tests don't interfere; each test owns its own URIs
    //! and the [`SecretOverride`] RAII guard cleans up on drop.

    use std::cell::RefCell;
    use std::collections::HashMap;

    thread_local! {
        static OVERRIDES: RefCell<HashMap<String, Result<String, String>>>
            = RefCell::new(HashMap::new());
    }

    pub(super) fn lookup_override(uri: &str) -> Option<Result<String, String>> {
        OVERRIDES.with(|m| m.borrow().get(uri).cloned())
    }

    /// RAII guard that installs a fixed response for `uri` and
    /// removes it on drop, so a panicking assertion can't leave
    /// stale state behind.
    pub struct SecretOverride {
        uri: String,
    }

    impl SecretOverride {
        pub fn ok(uri: &str, value: &str) -> Self {
            OVERRIDES.with(|m| {
                m.borrow_mut()
                    .insert(uri.to_string(), Ok(value.to_string()));
            });
            Self {
                uri: uri.to_string(),
            }
        }

        pub fn err(uri: &str, message: &str) -> Self {
            OVERRIDES.with(|m| {
                m.borrow_mut()
                    .insert(uri.to_string(), Err(message.to_string()));
            });
            Self {
                uri: uri.to_string(),
            }
        }
    }

    impl Drop for SecretOverride {
        fn drop(&mut self) {
            OVERRIDES.with(|m| {
                m.borrow_mut().remove(&self.uri);
            });
        }
    }
}

/// Map an `os_info::Type` onto our 4-variant `OsType` string-union.
/// Anything not in {Linux, Macos, Windows} collapses to `"Unknown"` —
/// the variant list lives in [`stdlib::OS_TYPE_VARIANTS`] so additions
/// require touching both sides. Pure on its input so every arm is
/// reachable from a unit test regardless of the host platform.
const fn map_os_type(t: os_info::Type) -> &'static str {
    use os_info::Type;
    match t {
        // Every Linux flavour os_info knows about — kept exhaustive
        // (rather than `_ => "Linux"`) so a new os_info variant we
        // haven't classified surfaces as "Unknown" until we triage it.
        Type::Linux
        | Type::Alpine
        | Type::Amazon
        | Type::Android
        | Type::Arch
        | Type::Artix
        | Type::CachyOS
        | Type::CentOS
        | Type::Debian
        | Type::EndeavourOS
        | Type::Fedora
        | Type::Garuda
        | Type::Gentoo
        | Type::Kali
        | Type::Mabox
        | Type::Manjaro
        | Type::Mariner
        | Type::Mint
        | Type::NixOS
        | Type::Nobara
        | Type::OpenCloudOS
        | Type::openEuler
        | Type::openSUSE
        | Type::OracleLinux
        | Type::Pop
        | Type::Raspbian
        | Type::Redhat
        | Type::RedHatEnterprise
        | Type::RockyLinux
        | Type::Solus
        | Type::SUSE
        | Type::Ubuntu
        | Type::Ultramarine
        | Type::Uos
        | Type::Void => "Linux",
        Type::Macos => "Macos",
        Type::Windows => "Windows",
        _ => "Unknown",
    }
}

/// Host-OS detection. Thin wrapper around [`map_os_type`]: reads
/// `os_info::get().os_type()` and feeds it through the pure mapping.
/// `#[mutants::skip]` because the input is the runtime host and not
/// reproducible from a test — `map_os_type` carries the mutation
/// surface.
#[cfg_attr(test, mutants::skip)]
fn detect_os_type() -> String {
    map_os_type(os_info::get().os_type()).to_string()
}

/// Map `os_info::Info::architecture()`'s `Option<&str>` onto our
/// `OsArch` string-union. `os_info` returns the kernel's own arch
/// label; we normalize a few common synonyms (`amd64` → `x86_64`,
/// `arm64` → `aarch64`, `i686`/`i386` → `x86`) and fall everything
/// else through to `"Unknown"`. Variant list lives in
/// [`stdlib::OS_ARCH_VARIANTS`].
// Not `const` because str-pattern matching uses `PartialEq` which
// isn't a stable const trait yet (Rust 1.95.0). When `const_eq`
// stabilizes, this can be promoted to `const fn` — exposing it to
// const evaluation is cheap; nothing relies on it today.
fn map_os_arch(arch: Option<&str>) -> &'static str {
    match arch {
        Some("x86_64" | "amd64") => "x86_64",
        Some("aarch64" | "arm64") => "aarch64",
        Some("arm") => "arm",
        Some("x86" | "i386" | "i686") => "x86",
        _ => "Unknown",
    }
}

/// Host-arch detection. Thin wrapper around [`map_os_arch`]; same
/// host-dependency caveat as [`detect_os_type`].
#[cfg_attr(test, mutants::skip)]
fn detect_os_arch() -> String {
    map_os_arch(os_info::get().architecture()).to_string()
}

/// Build a `Template` resource by reading a template file from disk
/// and rendering it with the supplied variables. `path` is the
/// destination (where the rendered text will land at apply time);
/// `source` is the path of the template file (resolved relative to
/// the importing module's source directory). The rendered content is
/// frozen into [`ResourceState::Template`] at eval time so the apply
/// step is a plain write.
fn dispatch_template(args: &[CallArg], env: &Env<'_, '_>) -> Result<Value> {
    let path = call_string(args, env, "path", 0)?;
    let source = call_string(args, env, "source", 1)?;
    let (vars, sensitive) = call_string_map(args, env, "vars", 2)?;
    let resolved = resolve_managed_path(&source, env, "template", "source")?;
    let raw = std::fs::read_to_string(&resolved).with_context(|| {
        format!(
            "could not read template source `{source}` (resolved to `{}`)",
            resolved.display()
        )
    })?;
    let rendered =
        render_template(&raw, &vars).with_context(|| format!("rendering template `{source}`"))?;
    Ok(Value::Resource(ResourceState::Template {
        path: PathBuf::from(path),
        content: rendered,
        sensitive,
    }))
}

/// Resolve a user-supplied path argument and pin it inside the keron
/// root. Applies to the source side of resources `keron apply` owns —
/// the `to` of a symlink and the `source` of a template — because
/// those must live inside the directory the user pointed the CLI at;
/// the project is otherwise free to symlink to or template from
/// arbitrary host paths, which defeats the "the keron dir is the
/// single source of truth" model.
///
/// Resolution rules:
/// - Relative paths are joined against the importing module file's
///   directory, so `to = "./zshrc"` next to `<root>/sub/foo.keron`
///   means `<root>/sub/zshrc`.
/// - Absolute paths are taken as-is — typically produced by
///   interpolating `${keron_root()}` into the string.
/// - The candidate is canonicalized (resolves `..`, follows any
///   intermediate symlinks) and then required to be a descendant of
///   `env.graph.keron_root` (itself canonicalized at run start).
/// - The canonical target replaces the raw user value; the executor
///   and the diff renderer both see the absolute, dereferenced path
///   so a moved symlink does not silently target a different file.
/// - **The leaf must be a real file**, not a symlink. Templating from a
///   symlink or symlinking to another symlink would chain indirection
///   that the user almost certainly didn't intend; we refuse rather
///   than silently follow. Intermediate components may still be
///   symlinks (e.g. a symlinked keron root); only the final component
///   is rejected.
fn resolve_managed_path(raw: &str, env: &Env<'_, '_>, kind: &str, arg: &str) -> Result<PathBuf> {
    let candidate = PathBuf::from(raw);
    let absolute = if candidate.is_absolute() {
        candidate
    } else {
        let ModuleId::File(module_path) = &env.current;
        module_path.parent().map_or(candidate, |p| p.join(raw))
    };
    let leaf_meta = std::fs::symlink_metadata(&absolute).with_context(|| {
        format!(
            "resolving {kind} `{arg}` = `{raw}` (looked for `{}`)",
            absolute.display()
        )
    })?;
    if leaf_meta.file_type().is_symlink() {
        bail!(
            "{kind} `{arg}` = `{raw}` is a symlink (`{}`); keron only manages real files — \
             point at the underlying file instead",
            absolute.display()
        );
    }
    let canonical = std::fs::canonicalize(&absolute).with_context(|| {
        format!(
            "resolving {kind} `{arg}` = `{raw}` (looked for `{}`)",
            absolute.display()
        )
    })?;
    if !canonical.starts_with(&env.graph.keron_root) {
        bail!(
            "{kind} `{arg}` = `{raw}` resolves to `{}`, which is outside the keron root `{}`",
            canonical.display(),
            env.graph.keron_root.display()
        );
    }
    Ok(canonical)
}

/// Render a Tera template against the supplied variable map.
///
/// Missing variables are a hard error: Tera's default behaviour
/// raises a "Variable X not found in context while rendering ..."
/// error from the renderer, which preserves the old hand-rolled
/// `${name}` engine's "typo'd placeholder is a build failure, not
/// silent empty text" guarantee.
///
/// Autoescape is disabled. Dotfile content is not HTML; `&`, `<`,
/// `>`, `"` must pass through verbatim. Tera's default autoescape
/// applies only to `.html` / `.htm` / `.xml` extensions — we
/// register the template under an extension-less name and clear the
/// autoescape list defensively in case that ever changes.
fn render_template(src: &str, vars: &HashMap<String, String>) -> Result<String> {
    const NAME: &str = "__keron_inline__";
    let mut tera = tera::Tera::default();
    tera.functions.clear();
    tera.autoescape_on(Vec::new());
    tera.add_raw_template(NAME, src)
        .map_err(|e| anyhow!("parsing template: {}", format_tera_error(&e)))?;
    let mut ctx = tera::Context::new();
    for (k, v) in vars {
        ctx.insert(k, v);
    }
    tera.render(NAME, &ctx)
        .map_err(|e| anyhow!("rendering template: {}", format_tera_error(&e)))
}

/// Flatten Tera's source chain into a single line. Tera wraps the
/// real cause (e.g. "Variable who not found in context") inside a
/// generic "Failed to render ..." outer error; without the chain
/// walk the user only sees the outer wrapper.
fn format_tera_error(err: &tera::Error) -> String {
    use std::error::Error as _;
    let mut msg = err.to_string();
    let mut source: Option<&dyn std::error::Error> = err.source();
    while let Some(e) = source {
        msg.push_str(": ");
        msg.push_str(&e.to_string());
        source = e.source();
    }
    msg
}

fn bind_params(
    fn_decl: &FnDecl,
    args: &[CallArg],
    env: &Env<'_, '_>,
    call_env: &mut Env<'_, '_>,
) -> Result<()> {
    let mut positional = args.iter().filter(|a| a.name.is_none());
    for param in &fn_decl.params {
        let named = args
            .iter()
            .find(|a| a.name.as_ref().is_some_and(|n| n.node == param.name.node));
        let value = if let Some(arg) = named {
            eval_expr(&arg.value, env)?
        } else if let Some(arg) = positional.next() {
            eval_expr(&arg.value, env)?
        } else if let Some(default) = &param.default {
            eval_expr(default, call_env)?
        } else {
            bail!("missing argument for parameter `{}`", param.name.node);
        };
        call_env.local.insert(param.name.node.clone(), value);
    }
    Ok(())
}

fn call_string(
    args: &[CallArg],
    env: &Env<'_, '_>,
    name: &str,
    positional_idx: usize,
) -> Result<String> {
    let v = call_string_value(args, env, name, positional_idx)?;
    if v.sensitive {
        bail!("sensitive String cannot be used for `{name}`");
    }
    Ok(v.text)
}

struct EvalString {
    text: String,
    sensitive: bool,
}

fn call_string_value(
    args: &[CallArg],
    env: &Env<'_, '_>,
    name: &str,
    positional_idx: usize,
) -> Result<EvalString> {
    let v = eval_call_arg(args, env, name, positional_idx)?;
    match v {
        Value::String { text, sensitive } => Ok(EvalString { text, sensitive }),
        other => bail!("expected String for `{name}`, got {}", other.type_name()),
    }
}

/// Pull a `Map<String, String>` argument out of a call. Used by the
/// `template` intrinsic; the type checker has already proven the map
/// keys and values are strings, so any other shape here means an AST
/// invariant slipped.
fn call_string_map(
    args: &[CallArg],
    env: &Env<'_, '_>,
    name: &str,
    positional_idx: usize,
) -> Result<(HashMap<String, String>, bool)> {
    let v = eval_call_arg(args, env, name, positional_idx)?;
    let Value::Map(entries) = v else {
        bail!(
            "expected Map<String, String> for `{name}`, got {}",
            v.type_name()
        );
    };
    let mut out = HashMap::with_capacity(entries.len());
    let mut sensitive = false;
    for (k, val) in entries {
        let (
            Value::String {
                text: k,
                sensitive: key_sensitive,
            },
            Value::String {
                text: val,
                sensitive: value_sensitive,
            },
        ) = (k, val)
        else {
            bail!("expected Map<String, String> entries for `{name}`");
        };
        if key_sensitive {
            bail!("sensitive String cannot be used as a `{name}` key");
        }
        sensitive |= value_sensitive;
        out.insert(k, val);
    }
    Ok((out, sensitive))
}

/// Resolve a single call arg by name (preferring named over
/// positional) and evaluate it. Shared by `call_string` and
/// `call_string_map`.
fn eval_call_arg(
    args: &[CallArg],
    env: &Env<'_, '_>,
    name: &str,
    positional_idx: usize,
) -> Result<Value> {
    let named = args
        .iter()
        .find(|a| a.name.as_ref().is_some_and(|n| n.node == name));
    let arg = if let Some(a) = named {
        a
    } else {
        args.iter()
            .filter(|a| a.name.is_none())
            .nth(positional_idx)
            .ok_or_else(|| anyhow!("missing argument `{name}`"))?
    };
    eval_expr(&arg.value, env)
}

#[cfg(test)]
mod tests {
    use super::*;
    use keron_modules::{EntrySource, resolve};
    use std::env;
    use std::fs;
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

    static SEQ: AtomicUsize = AtomicUsize::new(0);

    /// Drop-style temp project for evaluator integration tests.
    struct TempProject {
        root: PathBuf,
    }

    impl TempProject {
        fn new(name: &str) -> Self {
            let n = SEQ.fetch_add(1, AtomicOrdering::Relaxed);
            let root =
                env::temp_dir().join(format!("keron-eval-test-{name}-{}-{n}", std::process::id()));
            if root.exists() {
                fs::remove_dir_all(&root).ok();
            }
            fs::create_dir_all(&root).expect("create temp dir");
            // Drop a generic one-placeholder template alongside the
            // entry so the convention `template(path = X, source =
            // "tmpl.tpl", vars = {"body": Y})` works as a direct
            // stand-in for the old `file(path = X, content = Y)`
            // shape. Tests that care about template-level mechanics
            // (multiple placeholders, missing vars, etc.) seed their
            // own template file via `seed_template`.
            fs::write(root.join("tmpl.tpl"), "{{ body }}").expect("seed default template");
            Self { root }
        }

        fn entry(&self, src: &str) -> PathBuf {
            let path = self.root.join("entry.keron");
            fs::write(&path, src).expect("write entry");
            path
        }

        fn seed_template(&self, name: &str, content: &str) {
            fs::write(self.root.join(name), content).expect("seed template");
        }
    }

    impl Drop for TempProject {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    /// Resolve + evaluate a snippet as the entry of a fresh module
    /// graph; return the resulting resource list. The temp project
    /// auto-seeds a `tmpl.tpl` template (single `{{ body }}`
    /// placeholder); tests that need richer templates use
    /// [`run_with_templates`].
    fn run(src: &str) -> Vec<ResourceState> {
        run_with_templates(src, &[])
    }

    /// Same as [`run`] but returns the `keron_root` the harness used
    /// alongside the resource list, so tests can assert against the
    /// concrete root path the intrinsic should have observed.
    fn run_with_root(src: &str) -> (Vec<ResourceState>, PathBuf) {
        let proj = TempProject::new("run-root");
        let entry = proj.entry(src);
        let canonical = fs::canonicalize(&entry).unwrap();
        let base_dir = canonical.parent().unwrap().to_path_buf();
        let keron_root = base_dir.clone();
        let graph = resolve(vec![EntrySource {
            text: src.to_string(),
            base_dir,
            id: keron_modules::ModuleId::File(canonical),
        }])
        .unwrap_or_else(|errs| panic!("resolve failed: {errs:?}"));
        let states = eval_graph(&graph, &keron_root).unwrap_or_else(|e| panic!("eval failed: {e}"));
        (states, keron_root)
    }

    fn run_with_templates(src: &str, templates: &[(&str, &str)]) -> Vec<ResourceState> {
        run_result_with_templates(src, templates).unwrap_or_else(|e| panic!("eval failed: {e}"))
    }

    fn run_result_with_templates(
        src: &str,
        templates: &[(&str, &str)],
    ) -> Result<Vec<ResourceState>> {
        let proj = TempProject::new("run");
        for (name, content) in templates {
            proj.seed_template(name, content);
        }
        let entry = proj.entry(src);
        let canonical = fs::canonicalize(&entry).unwrap();
        let base_dir = canonical.parent().unwrap().to_path_buf();
        let keron_root = base_dir.clone();
        let graph = resolve(vec![EntrySource {
            text: src.to_string(),
            base_dir,
            id: keron_modules::ModuleId::File(canonical),
        }])
        .map_err(|errs| anyhow!("resolve failed: {errs:?}"))?;
        eval_graph(&graph, &keron_root)
    }

    fn first_file_path(states: &[ResourceState]) -> &PathBuf {
        match &states[0] {
            ResourceState::Template { path, .. } => path,
            ResourceState::Symlink { from, .. } => from,
            // The helper is for filesystem-shaped resources; package
            // resources don't have a path, so callers shouldn't reach
            // for it here. Loud failure beats silently picking the
            // name field as a "path".
            ResourceState::Package { manager, name } => {
                panic!(
                    "first_file_path: expected filesystem resource, got Package({manager:?}, {name:?})"
                )
            }
        }
    }

    fn first_file_content(states: &[ResourceState]) -> &str {
        match &states[0] {
            ResourceState::Template { content, .. } => content.as_str(),
            _ => panic!("expected Template"),
        }
    }

    #[test]
    fn value_type_name_returns_canonical_strings() {
        assert_eq!(Value::plain_string(String::new()).type_name(), "String");
        assert_eq!(Value::Int(0).type_name(), "Int");
        assert_eq!(Value::Bool(false).type_name(), "Boolean");
        assert_eq!(Value::Double(0.0).type_name(), "Double");
        assert_eq!(Value::List(Vec::new()).type_name(), "List");
        assert_eq!(Value::Map(Vec::new()).type_name(), "Map");
        assert_eq!(
            Value::Resource(ResourceState::Symlink {
                from: PathBuf::from("/tmp/a"),
                to: PathBuf::from("/tmp/b"),
            })
            .type_name(),
            "Resource"
        );
        assert_eq!(Value::Void.type_name(), "Void");
    }

    #[test]
    fn eval_unary_negates_int() {
        let v = eval_unary(UnaryOp::Neg, Value::Int(5)).unwrap();
        assert!(matches!(v, Value::Int(-5)));
        let v = eval_unary(UnaryOp::Neg, Value::Int(-3)).unwrap();
        assert!(matches!(v, Value::Int(3)));
    }

    #[test]
    fn eval_unary_negates_double() {
        let v = eval_unary(UnaryOp::Neg, Value::Double(2.5)).unwrap();
        let Value::Double(d) = v else {
            panic!("expected Double");
        };
        assert!((d - -2.5).abs() < 1e-9);
    }

    fn int(n: i64) -> Value {
        Value::Int(n)
    }
    fn dbl(d: f64) -> Value {
        Value::Double(d)
    }
    fn s(v: &str) -> Value {
        Value::plain_string(v)
    }
    fn assert_int(v: &Value, expected: i64) {
        match v {
            Value::Int(n) => assert_eq!(*n, expected),
            other => panic!("expected Int({expected}), got {}", other.type_name()),
        }
    }
    fn assert_dbl(v: &Value, expected: f64) {
        match v {
            Value::Double(d) => {
                assert!((d - expected).abs() < 1e-9, "expected {expected}, got {d}");
            }
            _ => panic!("expected Double"),
        }
    }
    fn assert_bool(v: &Value, expected: bool) {
        match v {
            Value::Bool(b) => assert_eq!(*b, expected),
            _ => panic!("expected Bool"),
        }
    }
    fn assert_string(v: &Value, expected: &str) {
        match v {
            Value::String { text, .. } => assert_eq!(text, expected),
            _ => panic!("expected String"),
        }
    }

    #[test]
    fn eval_binop_string_concat() {
        assert_string(&eval_binop(BinOp::Add, s("a"), s("b")).unwrap(), "ab");
    }

    #[test]
    fn eval_binop_int_int() {
        assert_int(&eval_binop(BinOp::Add, int(2), int(3)).unwrap(), 5);
        assert_int(&eval_binop(BinOp::Sub, int(5), int(2)).unwrap(), 3);
        assert_int(&eval_binop(BinOp::Mul, int(3), int(4)).unwrap(), 12);
        assert_int(&eval_binop(BinOp::Div, int(10), int(2)).unwrap(), 5);
        assert_int(&eval_binop(BinOp::Pow, int(2), int(8)).unwrap(), 256);
    }

    #[test]
    fn eval_binop_int_div_by_zero_errors() {
        let e = eval_binop(BinOp::Div, int(1), int(0)).unwrap_err();
        assert!(e.to_string().contains("division by zero"));
    }

    #[test]
    fn eval_binop_double_double() {
        assert_dbl(&eval_binop(BinOp::Add, dbl(1.5), dbl(2.0)).unwrap(), 3.5);
        assert_dbl(&eval_binop(BinOp::Sub, dbl(5.5), dbl(2.0)).unwrap(), 3.5);
        assert_dbl(&eval_binop(BinOp::Mul, dbl(2.0), dbl(3.0)).unwrap(), 6.0);
        assert_dbl(&eval_binop(BinOp::Div, dbl(10.0), dbl(4.0)).unwrap(), 2.5);
        assert_dbl(&eval_binop(BinOp::Pow, dbl(2.0), dbl(3.0)).unwrap(), 8.0);
    }

    #[test]
    fn eval_binop_int_double_promotes() {
        assert_dbl(&eval_binop(BinOp::Add, int(1), dbl(2.5)).unwrap(), 3.5);
        assert_dbl(&eval_binop(BinOp::Sub, int(5), dbl(1.5)).unwrap(), 3.5);
        assert_dbl(&eval_binop(BinOp::Mul, int(2), dbl(2.5)).unwrap(), 5.0);
        assert_dbl(&eval_binop(BinOp::Div, int(10), dbl(4.0)).unwrap(), 2.5);
        assert_dbl(&eval_binop(BinOp::Pow, int(2), dbl(3.0)).unwrap(), 8.0);
    }

    #[test]
    fn eval_binop_double_int_promotes() {
        assert_dbl(&eval_binop(BinOp::Add, dbl(1.5), int(2)).unwrap(), 3.5);
        assert_dbl(&eval_binop(BinOp::Sub, dbl(5.5), int(2)).unwrap(), 3.5);
        assert_dbl(&eval_binop(BinOp::Mul, dbl(2.5), int(2)).unwrap(), 5.0);
        assert_dbl(&eval_binop(BinOp::Div, dbl(10.0), int(4)).unwrap(), 2.5);
        assert_dbl(&eval_binop(BinOp::Pow, dbl(2.0), int(3)).unwrap(), 8.0);
    }

    #[test]
    fn eval_binop_list_concat() {
        let v = eval_binop(
            BinOp::Concat,
            Value::List(vec![int(1), int(2)]),
            Value::List(vec![int(3)]),
        )
        .unwrap();
        let Value::List(items) = v else {
            panic!("expected List");
        };
        assert_eq!(items.len(), 3);
    }

    #[test]
    fn eval_binop_eq_neq() {
        assert_bool(&eval_binop(BinOp::Eq, int(1), int(1)).unwrap(), true);
        assert_bool(&eval_binop(BinOp::Eq, int(1), int(2)).unwrap(), false);
        assert_bool(&eval_binop(BinOp::Neq, int(1), int(2)).unwrap(), true);
        assert_bool(&eval_binop(BinOp::Neq, int(1), int(1)).unwrap(), false);
    }

    #[test]
    fn eval_binop_ordering() {
        assert_bool(&eval_binop(BinOp::Lt, int(1), int(2)).unwrap(), true);
        assert_bool(&eval_binop(BinOp::Lt, int(2), int(1)).unwrap(), false);
        assert_bool(&eval_binop(BinOp::Le, int(1), int(1)).unwrap(), true);
        assert_bool(&eval_binop(BinOp::Le, int(2), int(1)).unwrap(), false);
        assert_bool(&eval_binop(BinOp::Gt, int(2), int(1)).unwrap(), true);
        assert_bool(&eval_binop(BinOp::Gt, int(1), int(2)).unwrap(), false);
        assert_bool(&eval_binop(BinOp::Ge, int(1), int(1)).unwrap(), true);
        assert_bool(&eval_binop(BinOp::Ge, int(0), int(1)).unwrap(), false);
    }

    #[test]
    fn value_eq_each_arm() {
        assert!(value_eq(&s("x"), &s("x")));
        assert!(!value_eq(&s("x"), &s("y")));
        assert!(value_eq(&int(1), &int(1)));
        assert!(!value_eq(&int(1), &int(2)));
        assert!(value_eq(&Value::Bool(true), &Value::Bool(true)));
        assert!(!value_eq(&Value::Bool(true), &Value::Bool(false)));
        assert!(value_eq(&dbl(1.5), &dbl(1.5)));
        assert!(!value_eq(&dbl(1.5), &dbl(2.0)));
        assert!(value_eq(&int(2), &dbl(2.0)));
        assert!(!value_eq(&int(2), &dbl(2.5)));
        assert!(value_eq(&dbl(2.0), &int(2)));
        assert!(!value_eq(&dbl(2.5), &int(2)));
    }

    #[test]
    fn value_eq_falls_through_for_unrelated_types() {
        assert!(!value_eq(&s("1"), &int(1)));
        assert!(!value_eq(&Value::Bool(true), &int(1)));
    }

    #[test]
    fn value_cmp_orders_each_combination() {
        assert_eq!(
            value_cmp(&int(1), &int(2)).unwrap(),
            std::cmp::Ordering::Less
        );
        assert_eq!(
            value_cmp(&dbl(2.0), &dbl(1.0)).unwrap(),
            std::cmp::Ordering::Greater
        );
        assert_eq!(
            value_cmp(&int(1), &dbl(1.0)).unwrap(),
            std::cmp::Ordering::Equal
        );
        assert_eq!(
            value_cmp(&dbl(1.5), &int(1)).unwrap(),
            std::cmp::Ordering::Greater
        );
        assert_eq!(
            value_cmp(&s("a"), &s("b")).unwrap(),
            std::cmp::Ordering::Less
        );
    }

    #[test]
    fn stringify_each_primitive() {
        let mut out = String::new();
        stringify(&s("hi"), &mut out).unwrap();
        assert_eq!(out, "hi");
        out.clear();
        stringify(&int(42), &mut out).unwrap();
        assert_eq!(out, "42");
        out.clear();
        stringify(&Value::Bool(true), &mut out).unwrap();
        assert_eq!(out, "true");
        out.clear();
        stringify(&Value::Bool(false), &mut out).unwrap();
        assert_eq!(out, "false");
        out.clear();
        stringify(&dbl(1.5), &mut out).unwrap();
        assert_eq!(out, "1.5");
    }

    #[test]
    fn stringify_rejects_non_primitive() {
        let mut out = String::new();
        let err = stringify(&Value::List(Vec::new()), &mut out).unwrap_err();
        assert!(err.to_string().contains("cannot interpolate"));
    }

    #[test]
    fn eval_graph_emits_resources_for_reconciles() {
        let states = run(
            "reconcile template(path = \"/x\", source = \"tmpl.tpl\", vars = {\"body\": \"y\"})\n",
        );
        assert_eq!(states.len(), 1);
        assert_eq!(first_file_path(&states), &PathBuf::from("/x"));
        assert_eq!(first_file_content(&states), "y");
    }

    #[test]
    fn eval_graph_returns_empty_when_no_reconciles() {
        let states = run(
            "val f: Template = template(path = \"/x\", source = \"tmpl.tpl\", vars = {\"body\": \"y\"})\n",
        );
        assert!(states.is_empty());
    }

    #[test]
    fn template_rendering_rejects_builtin_functions() {
        let err = run_result_with_templates(
            "reconcile template(path = \"/x\", source = \"tmpl.tpl\", vars = {})\n",
            &[("tmpl.tpl", "{{ get_env(name=\"PATH\") }}")],
        )
        .expect_err("Tera builtins must not be available");
        let msg = format!("{err:#}");
        assert!(msg.contains("get_env"), "error should name get_env: {msg}");
    }

    #[test]
    fn default_param_can_reference_earlier_param_at_runtime() {
        let states = run(
            "fn file(path: String, body: String = path + \" body\"): Template {\n\
             \ttemplate(path = path, source = \"tmpl.tpl\", vars = {\"body\": body})\n\
             }\n\
             reconcile file(\"/x\")\n",
        );
        assert_eq!(first_file_content(&states), "/x body");
    }

    #[test]
    fn push_resources_unwraps_lists() {
        let states = run(
            "val xs: List<Template> = [template(path = \"/a\", source = \"tmpl.tpl\", vars = {\"body\": \"\"}), \
                                    template(path = \"/b\", source = \"tmpl.tpl\", vars = {\"body\": \"\"})]\n\
             reconcile xs\n",
        );
        let paths: Vec<&PathBuf> = states
            .iter()
            .map(|s| match s {
                ResourceState::Template { path, .. } => path,
                _ => unreachable!(),
            })
            .collect();
        assert_eq!(paths, vec![&PathBuf::from("/a"), &PathBuf::from("/b")]);
    }

    #[test]
    fn exec_void_expr_handles_top_level_if() {
        let states = run(
            "if true { reconcile template(path = \"/yes\", source = \"tmpl.tpl\", vars = {\"body\": \"\"}) }\n",
        );
        assert_eq!(states.len(), 1);
        assert_eq!(first_file_path(&states), &PathBuf::from("/yes"));
    }

    #[test]
    fn exec_void_expr_skips_else_branch_when_true() {
        let states = run("if true {\n\
             \treconcile template(path = \"/yes\", source = \"tmpl.tpl\", vars = {\"body\": \"\"})\n\
             } else {\n\
             \treconcile template(path = \"/no\", source = \"tmpl.tpl\", vars = {\"body\": \"\"})\n\
             }\n");
        assert_eq!(states.len(), 1);
        assert_eq!(first_file_path(&states), &PathBuf::from("/yes"));
    }

    #[test]
    fn exec_void_expr_handles_top_level_for() {
        let states = run("for n in [1, 2, 3] {\n\
             \treconcile template(path = \"/${n}\", source = \"tmpl.tpl\", vars = {\"body\": \"\"})\n\
             }\n");
        assert_eq!(states.len(), 3);
        assert_eq!(first_file_path(&states), &PathBuf::from("/1"));
    }

    #[test]
    fn exec_void_block_executes_local_vals_and_reconciles_in_order() {
        // Local val is referenced by a later reconcile; both run via
        // `exec_void_block`. Mutating that to `Ok(())` would skip the
        // reconcile and produce an empty plan.
        let states = run("if true {\n\
             \tval base: String = \"/v\"\n\
             \treconcile template(path = base, source = \"tmpl.tpl\", vars = {\"body\": \"\"})\n\
             }\n");
        assert_eq!(states.len(), 1);
        assert_eq!(first_file_path(&states), &PathBuf::from("/v"));
    }

    #[test]
    fn iterate_runs_body_per_map_entry() {
        let states = run("for (k, v) in {\"a\": 1, \"b\": 2} {\n\
             \treconcile template(path = \"/${k}\", source = \"tmpl.tpl\", vars = {\"body\": \"${v}\"})\n\
             }\n");
        assert_eq!(states.len(), 2);
        // Map iteration order is unspecified — assert on the set of paths.
        let mut paths: Vec<_> = states
            .iter()
            .map(|s| match s {
                ResourceState::Template { path, .. } => path.clone(),
                _ => unreachable!(),
            })
            .collect();
        paths.sort();
        assert_eq!(paths, vec![PathBuf::from("/a"), PathBuf::from("/b")]);
    }

    #[test]
    fn arithmetic_in_interpolation_round_trips() {
        // Encodes binop results in the file path so any drift in
        // eval_binop arithmetic is observable end-to-end.
        let states = run(
            "reconcile template(path = \"/${2 + 3}-${10 - 4}-${2 * 3}-${10 / 2}-${2 ** 8}\", source = \"tmpl.tpl\", vars = {\"body\": \"\"})\n",
        );
        assert_eq!(first_file_path(&states), &PathBuf::from("/5-6-6-5-256"));
    }

    #[test]
    fn double_arithmetic_in_interpolation_round_trips() {
        let states = run("val sum: Double = 1.5 + 2.0\n\
             val diff: Double = 5.5 - 2.0\n\
             val prod: Double = 2.0 * 3.0\n\
             val quot: Double = 10.0 / 4.0\n\
             reconcile template(path = \"/${sum}-${diff}-${prod}-${quot}\", source = \"tmpl.tpl\", vars = {\"body\": \"\"})\n");
        assert_eq!(first_file_path(&states), &PathBuf::from("/3.5-3.5-6-2.5"));
    }

    #[test]
    fn mixed_int_double_arithmetic_round_trips() {
        let states = run("val a: Double = 1 + 2.5\n\
             val b: Double = 5 - 1.5\n\
             val c: Double = 2 * 2.5\n\
             val d: Double = 1.5 * 2\n\
             reconcile template(path = \"/${a}-${b}-${c}-${d}\", source = \"tmpl.tpl\", vars = {\"body\": \"\"})\n");
        assert_eq!(first_file_path(&states), &PathBuf::from("/3.5-3.5-5-3"));
    }

    #[test]
    fn unary_neg_in_interpolation_round_trips() {
        let states = run("val x: Int = -7\n\
             val y: Double = -2.5\n\
             reconcile template(path = \"/${x}\", source = \"tmpl.tpl\", vars = {\"body\": \"${y}\"})\n");
        assert_eq!(first_file_path(&states), &PathBuf::from("/-7"));
        assert_eq!(first_file_content(&states), "-2.5");
    }

    #[test]
    fn equality_observable_via_branching() {
        let states = run("val same: Boolean = 1 == 1\n\
             val diff: Boolean = 1 == 2\n\
             reconcile template(path = if same { \"/yes\" } else { \"/no\" }, source = \"tmpl.tpl\", vars = {\"body\": \"\"})\n\
             reconcile template(path = if diff { \"/yes\" } else { \"/no\" }, source = \"tmpl.tpl\", vars = {\"body\": \"\"})\n");
        let paths: Vec<_> = states
            .iter()
            .map(|s| match s {
                ResourceState::Template { path, .. } => path.clone(),
                _ => unreachable!(),
            })
            .collect();
        assert_eq!(paths, vec![PathBuf::from("/yes"), PathBuf::from("/no")]);
    }

    #[test]
    fn comparison_operators_observable_via_branching() {
        let states = run(
            "reconcile template(path = if 1 < 2 { \"/lt\" } else { \"/ge\" }, source = \"tmpl.tpl\", vars = {\"body\": \"\"})\n\
             reconcile template(path = if 2 <= 2 { \"/le\" } else { \"/gt\" }, source = \"tmpl.tpl\", vars = {\"body\": \"\"})\n\
             reconcile template(path = if 3 > 2 { \"/gt\" } else { \"/le\" }, source = \"tmpl.tpl\", vars = {\"body\": \"\"})\n\
             reconcile template(path = if 2 >= 2 { \"/ge\" } else { \"/lt\" }, source = \"tmpl.tpl\", vars = {\"body\": \"\"})\n",
        );
        let paths: Vec<_> = states
            .iter()
            .map(|s| match s {
                ResourceState::Template { path, .. } => path.clone(),
                _ => unreachable!(),
            })
            .collect();
        assert_eq!(
            paths,
            vec![
                PathBuf::from("/lt"),
                PathBuf::from("/le"),
                PathBuf::from("/gt"),
                PathBuf::from("/ge"),
            ]
        );
    }

    #[test]
    fn string_equality_distinguishes_distinct_values() {
        let states = run(
            "reconcile template(path = if \"a\" == \"a\" { \"/eq\" } else { \"/ne\" }, source = \"tmpl.tpl\", vars = {\"body\": \"\"})\n\
             reconcile template(path = if \"a\" == \"b\" { \"/eq\" } else { \"/ne\" }, source = \"tmpl.tpl\", vars = {\"body\": \"\"})\n",
        );
        let paths: Vec<_> = states
            .iter()
            .map(|s| match s {
                ResourceState::Template { path, .. } => path.clone(),
                _ => unreachable!(),
            })
            .collect();
        assert_eq!(paths, vec![PathBuf::from("/eq"), PathBuf::from("/ne")]);
    }

    #[test]
    fn boolean_equality_distinguishes_distinct_values() {
        let states = run(
            "reconcile template(path = if true == true { \"/eq\" } else { \"/ne\" }, source = \"tmpl.tpl\", vars = {\"body\": \"\"})\n\
             reconcile template(path = if true == false { \"/eq\" } else { \"/ne\" }, source = \"tmpl.tpl\", vars = {\"body\": \"\"})\n",
        );
        let paths: Vec<_> = states
            .iter()
            .map(|s| match s {
                ResourceState::Template { path, .. } => path.clone(),
                _ => unreachable!(),
            })
            .collect();
        assert_eq!(paths, vec![PathBuf::from("/eq"), PathBuf::from("/ne")]);
    }

    #[test]
    fn cross_type_equality_via_int_double_promotion() {
        let states = run(
            "reconcile template(path = if 2 == 2.0 { \"/eq\" } else { \"/ne\" }, source = \"tmpl.tpl\", vars = {\"body\": \"\"})\n\
             reconcile template(path = if 2 == 2.5 { \"/eq\" } else { \"/ne\" }, source = \"tmpl.tpl\", vars = {\"body\": \"\"})\n",
        );
        let paths: Vec<_> = states
            .iter()
            .map(|s| match s {
                ResourceState::Template { path, .. } => path.clone(),
                _ => unreachable!(),
            })
            .collect();
        assert_eq!(paths, vec![PathBuf::from("/eq"), PathBuf::from("/ne")]);
    }

    #[test]
    fn bind_params_resolves_named_arg_by_name() {
        // Named args may appear in any order; bind_params has to
        // match by name. `==` mutated to `!=` on the name match
        // would mis-route both args.
        //
        // NOTE: stdlib intrinsics bypass `bind_params` (they pull
        // args through `call_string` directly), so this test must
        // route through a user-defined fn to exercise the path.
        let states = run("fn pair(left: String, right: String): String {\n\
             \tleft + \"|\" + right\n\
             }\n\
reconcile template(path = pair(right = \"R\", left = \"L\"), source = \"tmpl.tpl\", vars = {\"body\": \"\"})\n");
        assert_eq!(states.len(), 1);
        // With `==` correct, left=L, right=R, output = "L|R".
        // With `==` mutated to `!=`, args swap, output = "R|L".
        assert_eq!(first_file_path(&states), &PathBuf::from("L|R"));
    }

    #[test]
    fn bind_params_uses_default_when_arg_missing() {
        let states = run(
            "fn pick(prefix: String, suffix: String = \"-default\"): String {\n\
             \tprefix + suffix\n\
             }\n\
reconcile template(path = pick(\"a\"), source = \"tmpl.tpl\", vars = {\"body\": \"\"})\n",
        );
        assert_eq!(first_file_path(&states), &PathBuf::from("a-default"));
    }

    #[test]
    fn call_string_falls_back_to_positional() {
        // Each `template` arg resolved positionally (no `name = ...`
        // syntax). Mutating the positional-fallback path in
        // `eval_call_arg` would re-route the args.
        let states = run_with_templates(
            "reconcile template(\"/positional\", \"body.tpl\", {\"body\": \"hi\"})\n",
            &[("body.tpl", "{{ body }}")],
        );
        assert_eq!(first_file_path(&states), &PathBuf::from("/positional"));
        assert_eq!(first_file_content(&states), "hi");
    }

    #[test]
    fn val_eval_succeeds_when_not_in_progress() {
        // The cycle guard short-circuits successful evaluations when
        // `!` is dropped: `HashSet::insert(...)` returns `true` on a
        // fresh key, and without `!` the condition fires on every val
        // eval. This test exercises a plain val reference: it must
        // succeed, which is only possible when the cycle guard is
        // intact.
        let states = run("val tag: String = \"ok\"\n\
             reconcile template(path = \"/${tag}\", source = \"tmpl.tpl\", vars = {\"body\": \"\"})\n");
        assert_eq!(first_file_path(&states), &PathBuf::from("/ok"));
    }

    #[test]
    fn struct_field_access_round_trips() {
        let states = run("struct Host { name: String, port: Int }\n\
             val h: Host = Host(name = \"alpha\", port = 22)\n\
             reconcile template(path = \"/${h.name}-${h.port}\", source = \"tmpl.tpl\", vars = {\"body\": \"\"})\n");
        assert_eq!(first_file_path(&states), &PathBuf::from("/alpha-22"));
    }

    #[test]
    fn struct_construction_positional_and_named_match() {
        let states = run("struct Pair { a: String, b: String }\n\
             val p1: Pair = Pair(\"x\", \"y\")\n\
             val p2: Pair = Pair(b = \"y\", a = \"x\")\n\
             reconcile template(path = \"/${p1.a}-${p1.b}\", source = \"tmpl.tpl\", vars = {\"body\": \"\"})\n\
             reconcile template(path = \"/${p2.a}-${p2.b}\", source = \"tmpl.tpl\", vars = {\"body\": \"\"})\n");
        let paths: Vec<_> = states
            .iter()
            .map(|s| match s {
                ResourceState::Template { path, .. } => path.clone(),
                _ => unreachable!(),
            })
            .collect();
        assert_eq!(paths, vec![PathBuf::from("/x-y"), PathBuf::from("/x-y")]);
    }

    #[test]
    fn match_string_union_drives_branch() {
        let states = run("type Color = \"red\" | \"green\" | \"blue\"\n\
             fn label(c: Color): String {\n\
               match c {\n\
                 \"red\" => \"warm\",\n\
                 \"green\" => \"natural\",\n\
                 \"blue\" => \"cool\",\n\
               }\n\
             }\n\
             val c: Color = \"green\"\n\
             reconcile template(path = \"/${label(c)}\", source = \"tmpl.tpl\", vars = {\"body\": \"\"})\n");
        assert_eq!(first_file_path(&states), &PathBuf::from("/natural"));
    }

    #[test]
    fn match_struct_destructure_binds_fields() {
        let states = run("struct Point { x: Int, y: Int }\n\
             fn axis(p: Point): String {\n\
               match p {\n\
                 Point { x: 0, y: 0 } => \"origin\",\n\
                 Point { x: 0, y } => \"y-axis\",\n\
                 Point { x, y: 0 } => \"x-axis\",\n\
                 _ => \"other\",\n\
               }\n\
             }\n\
             reconcile template(path = \"/${axis(Point(0, 0))}\", source = \"tmpl.tpl\", vars = {\"body\": \"\"})\n\
             reconcile template(path = \"/${axis(Point(3, 0))}\", source = \"tmpl.tpl\", vars = {\"body\": \"\"})\n\
             reconcile template(path = \"/${axis(Point(0, 5))}\", source = \"tmpl.tpl\", vars = {\"body\": \"\"})\n\
             reconcile template(path = \"/${axis(Point(2, 3))}\", source = \"tmpl.tpl\", vars = {\"body\": \"\"})\n");
        let paths: Vec<_> = states
            .iter()
            .map(|s| match s {
                ResourceState::Template { path, .. } => path.clone(),
                _ => unreachable!(),
            })
            .collect();
        assert_eq!(
            paths,
            vec![
                PathBuf::from("/origin"),
                PathBuf::from("/x-axis"),
                PathBuf::from("/y-axis"),
                PathBuf::from("/other"),
            ]
        );
    }

    #[test]
    fn match_with_bind_arm_renames_scrutinee() {
        let states = run("fn label(s: String): String {\n\
               match s {\n\
                 \"\" => \"empty\",\n\
                 other => other,\n\
               }\n\
             }\n\
             reconcile template(path = \"/${label(\"hello\")}\", source = \"tmpl.tpl\", vars = {\"body\": \"\"})\n");
        assert_eq!(first_file_path(&states), &PathBuf::from("/hello"));
    }

    #[test]
    fn union_value_compares_equal_to_string_literal() {
        let states = run("type Mode = \"on\" | \"off\"\n\
             val m: Mode = \"on\"\n\
             reconcile template(path = if m == \"on\" { \"/active\" } else { \"/idle\" }, source = \"tmpl.tpl\", vars = {\"body\": \"\"})\n");
        assert_eq!(first_file_path(&states), &PathBuf::from("/active"));
    }

    #[test]
    fn match_int_literal_pattern_picks_the_exact_arm() {
        // `match` over an Int with literal patterns + wildcard. Each
        // arm must be selected only when the literal *equals* the
        // value; mutating `==` to `!=` in `try_match_pattern`'s Int
        // arm would mis-route every probe.
        let states = run("fn pick(n: Int): String {\n\
               match n {\n\
                 0 => \"zero\",\n\
                 1 => \"one\",\n\
                 _ => \"other\",\n\
               }\n\
             }\n\
             reconcile template(path = \"/${pick(0)}\", source = \"tmpl.tpl\", vars = {\"body\": \"\"})\n\
             reconcile template(path = \"/${pick(1)}\", source = \"tmpl.tpl\", vars = {\"body\": \"\"})\n\
             reconcile template(path = \"/${pick(7)}\", source = \"tmpl.tpl\", vars = {\"body\": \"\"})\n");
        let paths: Vec<_> = states
            .iter()
            .map(|s| match s {
                ResourceState::Template { path, .. } => path.clone(),
                _ => unreachable!(),
            })
            .collect();
        assert_eq!(
            paths,
            vec![
                PathBuf::from("/zero"),
                PathBuf::from("/one"),
                PathBuf::from("/other"),
            ]
        );
    }

    #[test]
    fn match_boolean_literal_pattern_picks_the_exact_arm() {
        // Distinguishes `true` and `false` literal patterns. Mutating
        // the Bool arm of `try_match_pattern` (delete arm, or `==` to
        // `!=`) would route both inputs to the wildcard fallback.
        let states = run("fn label(b: Boolean): String {\n\
               match b {\n\
                 true => \"yes\",\n\
                 false => \"no\",\n\
                 _ => \"unreachable\",\n\
               }\n\
             }\n\
             reconcile template(path = \"/${label(true)}\", source = \"tmpl.tpl\", vars = {\"body\": \"\"})\n\
             reconcile template(path = \"/${label(false)}\", source = \"tmpl.tpl\", vars = {\"body\": \"\"})\n");
        let paths: Vec<_> = states
            .iter()
            .map(|s| match s {
                ResourceState::Template { path, .. } => path.clone(),
                _ => unreachable!(),
            })
            .collect();
        assert_eq!(paths, vec![PathBuf::from("/yes"), PathBuf::from("/no")]);
    }

    #[test]
    fn template_substitutes_vars_into_resource_content() {
        // Render `{{ user }}` and `{{ shell }}` from the supplied
        // vars map and verify the resulting Template resource carries
        // the substituted text. Pins both `dispatch_template`'s arg
        // routing (path / source / vars) and `render_template`'s
        // Tera substitution.
        let states = run_with_templates(
            "reconcile template(\n\
                 \tpath = \"/etc/passwd\",\n\
                 \tsource = \"shell.tpl\",\n\
                 \tvars = {\"user\": \"alice\", \"shell\": \"/bin/zsh\"},\n\
             )\n",
            &[("shell.tpl", "{{ user }}:x:1000:{{ shell }}\n")],
        );
        assert_eq!(states.len(), 1);
        assert_eq!(first_file_path(&states), &PathBuf::from("/etc/passwd"));
        assert_eq!(first_file_content(&states), "alice:x:1000:/bin/zsh\n");
    }

    #[test]
    fn template_unknown_var_errors() {
        // A `{{ name }}` placeholder that isn't in `vars` is a hard
        // failure at apply-eval time — Tera's strict mode flagged on
        // the renderer. Mutating that flag back to `false` would let
        // typo'd placeholders silently render as empty strings.
        let proj = TempProject::new("tmpl-unknown-var");
        proj.seed_template("greet.tpl", "hello {{ who }}");
        let entry = proj.entry(
            "reconcile template(\n\
                 \tpath = \"/x\",\n\
                 \tsource = \"greet.tpl\",\n\
                 \tvars = {},\n\
             )\n",
        );
        let canonical = fs::canonicalize(&entry).unwrap();
        let base_dir = canonical.parent().unwrap().to_path_buf();
        let keron_root = base_dir.clone();
        let graph = resolve(vec![EntrySource {
            text: fs::read_to_string(&entry).unwrap(),
            base_dir,
            id: keron_modules::ModuleId::File(canonical),
        }])
        .unwrap_or_else(|errs| panic!("resolve failed: {errs:?}"));
        let err = eval_graph(&graph, &keron_root).expect_err("missing var should fail");
        assert!(
            err.chain().any(|e| e.to_string().contains("`who`")),
            "got: {err:#}",
        );
    }

    #[test]
    fn template_passes_non_ascii_text_through_unchanged() {
        // Non-ASCII bytes (here: an em-dash and a snowman) must
        // round-trip through the Tera renderer verbatim. The
        // underlying renderer is UTF-8-clean, but a future swap to a
        // byte-indexed implementation would re-open this hole.
        let states = run_with_templates(
            "reconcile template(\n\
                 \tpath = \"/x\",\n\
                 \tsource = \"intl.tpl\",\n\
                 \tvars = {\"who\": \"alice\"},\n\
             )\n",
            &[("intl.tpl", "{{ who }} — ☃\n")],
        );
        assert_eq!(first_file_content(&states), "alice — ☃\n");
    }

    #[test]
    fn template_treats_lone_dollar_as_literal() {
        // Tera assigns no special meaning to `$`; a stray `$` (with
        // or without surrounding text) must round-trip unchanged.
        // Pins the autoescape-off + Tera-parsing contract: a future
        // switch back to a `$`-based mini-language would silently
        // change semantics here.
        let states = run_with_templates(
            "reconcile template(\n\
                 \tpath = \"/x\",\n\
                 \tsource = \"trail.tpl\",\n\
                 \tvars = {},\n\
             )\n",
            &[("trail.tpl", "ends with $")],
        );
        assert_eq!(first_file_content(&states), "ends with $");
    }

    #[test]
    fn template_unterminated_braces_errors() {
        // `{{` with no closing `}}` is a Tera parse error. Pin the
        // failure so a future swap to a more permissive engine
        // doesn't silently swallow the broken placeholder.
        let proj = TempProject::new("tmpl-unterminated");
        proj.seed_template("bad.tpl", "open {{ unfinished");
        let entry =
            proj.entry("reconcile template(path = \"/x\", source = \"bad.tpl\", vars = {})\n");
        let canonical = fs::canonicalize(&entry).unwrap();
        let base_dir = canonical.parent().unwrap().to_path_buf();
        let keron_root = base_dir.clone();
        let graph = resolve(vec![EntrySource {
            text: fs::read_to_string(&entry).unwrap(),
            base_dir,
            id: keron_modules::ModuleId::File(canonical),
        }])
        .unwrap_or_else(|errs| panic!("resolve failed: {errs:?}"));
        let err = eval_graph(&graph, &keron_root).expect_err("unterminated should fail");
        assert!(
            err.chain()
                .any(|e| e.to_string().contains("parsing template")),
            "got: {err:#}"
        );
    }

    #[test]
    fn render_template_substitutes_known_var() {
        let mut vars = HashMap::new();
        vars.insert("name".into(), "alice".into());
        let out = render_template("hello {{ name }}!", &vars).unwrap();
        assert_eq!(out, "hello alice!");
    }

    #[test]
    fn render_template_passes_lone_dollar_through() {
        // `$x` and `$$` have no meaning to Tera; they're literal
        // text. Pin so a `$`-flavoured engine can never sneak back
        // in and turn dotfile shell snippets into rendering errors.
        let vars = HashMap::new();
        let out = render_template("$5 and $$", &vars).unwrap();
        assert_eq!(out, "$5 and $$");
    }

    #[test]
    fn render_template_does_not_autoescape_html_metacharacters() {
        // Dotfiles routinely contain `<`, `>`, `&`, `"` — autoescape
        // would mangle them into HTML entities. Pin that the
        // renderer leaves them alone.
        let mut vars = HashMap::new();
        vars.insert("payload".into(), "a < b && c > d \"q\"".into());
        let out = render_template("{{ payload }}", &vars).unwrap();
        assert_eq!(out, "a < b && c > d \"q\"");
    }

    #[test]
    fn render_template_supports_tera_filters() {
        // The `default-features = false` Tera build still ships the
        // core filter set. Pin that `upper` works so a future
        // accidental flip to a no-filters build is loud.
        let mut vars = HashMap::new();
        vars.insert("user".into(), "alice".into());
        let out = render_template("{{ user | upper }}", &vars).unwrap();
        assert_eq!(out, "ALICE");
    }

    #[test]
    fn template_missing_source_errors() {
        // `source` must point at an existing file. Without one, the
        // intrinsic surfaces a wrapping context line plus the
        // underlying I/O error so the user can locate the typo.
        let proj = TempProject::new("tmpl-missing-source");
        let entry =
            proj.entry("reconcile template(path = \"/x\", source = \"missing.tpl\", vars = {})\n");
        let canonical = fs::canonicalize(&entry).unwrap();
        let base_dir = canonical.parent().unwrap().to_path_buf();
        let keron_root = base_dir.clone();
        let graph = resolve(vec![EntrySource {
            text: fs::read_to_string(&entry).unwrap(),
            base_dir,
            id: keron_modules::ModuleId::File(canonical),
        }])
        .unwrap_or_else(|errs| panic!("resolve failed: {errs:?}"));
        let err = eval_graph(&graph, &keron_root).expect_err("missing source should fail");
        assert!(err.to_string().contains("missing.tpl"), "got: {err:#}");
    }

    #[test]
    fn match_double_literal_pattern_picks_the_exact_arm() {
        // Distinguishes Double literal patterns. Mutating the Double
        // arm — delete, or any of the `<`/`==`/`-`/`/` swaps that
        // cargo-mutants flagged on the EPSILON tolerance check —
        // would mis-route an exact match.
        let states = run("fn label(d: Double): String {\n\
               match d {\n\
                 1.5 => \"half\",\n\
                 2.5 => \"two-half\",\n\
                 _ => \"other\",\n\
               }\n\
             }\n\
             reconcile template(path = \"/${label(1.5)}\", source = \"tmpl.tpl\", vars = {\"body\": \"\"})\n\
             reconcile template(path = \"/${label(2.5)}\", source = \"tmpl.tpl\", vars = {\"body\": \"\"})\n\
             reconcile template(path = \"/${label(7.0)}\", source = \"tmpl.tpl\", vars = {\"body\": \"\"})\n");
        let paths: Vec<_> = states
            .iter()
            .map(|s| match s {
                ResourceState::Template { path, .. } => path.clone(),
                _ => unreachable!(),
            })
            .collect();
        assert_eq!(
            paths,
            vec![
                PathBuf::from("/half"),
                PathBuf::from("/two-half"),
                PathBuf::from("/other"),
            ]
        );
    }

    #[test]
    fn keron_root_intrinsic_returns_the_root_path_threaded_through_eval() {
        // End-to-end pin: the value `keron_root()` returns must equal
        // whatever `eval_graph` was called with. We park the result in
        // a `template` resource so we can read the path back.
        let (states, root) = run_with_root(
            "reconcile template(path = keron_root(), source = \"tmpl.tpl\", vars = {\"body\": \"\"})\n",
        );
        assert_eq!(states.len(), 1);
        let ResourceState::Template { path, .. } = &states[0] else {
            panic!("expected template, got {:?}", states[0]);
        };
        assert_eq!(path, &root);
    }

    #[test]
    fn keron_root_interpolates_into_paths() {
        let (states, root) = run_with_root(
            "reconcile template(path = \"${keron_root()}/sub\", source = \"tmpl.tpl\", vars = {\"body\": \"\"})\n",
        );
        let ResourceState::Template { path, .. } = &states[0] else {
            panic!("expected template, got {:?}", states[0]);
        };
        let expected = root.join("sub");
        assert_eq!(path, &expected);
    }

    #[test]
    fn os_type_intrinsic_returns_one_of_the_documented_variants() {
        // The host's actual OS isn't fixed, but it must collapse into
        // the four-variant `OsType` union — anything else means the
        // dispatcher's fallback was bypassed or a new os_info variant
        // is leaking through.
        let states = run(
            "reconcile template(path = os_type(), source = \"tmpl.tpl\", vars = {\"body\": \"\"})\n",
        );
        let ResourceState::Template { path, .. } = &states[0] else {
            panic!("expected template, got {:?}", states[0]);
        };
        let value = path.to_string_lossy().into_owned();
        assert!(
            stdlib::OS_TYPE_VARIANTS.contains(&value.as_str()),
            "os_type returned `{value}`, not in {:?}",
            stdlib::OS_TYPE_VARIANTS,
        );
    }

    #[test]
    fn os_arch_intrinsic_returns_one_of_the_documented_variants() {
        let states = run(
            "reconcile template(path = os_arch(), source = \"tmpl.tpl\", vars = {\"body\": \"\"})\n",
        );
        let ResourceState::Template { path, .. } = &states[0] else {
            panic!("expected template, got {:?}", states[0]);
        };
        let value = path.to_string_lossy().into_owned();
        assert!(
            stdlib::OS_ARCH_VARIANTS.contains(&value.as_str()),
            "os_arch returned `{value}`, not in {:?}",
            stdlib::OS_ARCH_VARIANTS,
        );
    }

    #[test]
    fn detect_os_type_falls_back_to_unknown_for_unmapped_variants() {
        // Direct dispatcher invariant: every value `detect_os_type`
        // produces must be one of the documented union variants.
        // (We can't force a particular host type from a unit test, but
        // we can pin that whatever the host reports lands in the set.)
        let got = detect_os_type();
        assert!(
            stdlib::OS_TYPE_VARIANTS.contains(&got.as_str()),
            "detect_os_type produced `{got}`, not in {:?}",
            stdlib::OS_TYPE_VARIANTS,
        );
    }

    #[test]
    fn detect_os_arch_falls_back_to_unknown_for_unmapped_arches() {
        let got = detect_os_arch();
        assert!(
            stdlib::OS_ARCH_VARIANTS.contains(&got.as_str()),
            "detect_os_arch produced `{got}`, not in {:?}",
            stdlib::OS_ARCH_VARIANTS,
        );
    }

    #[test]
    fn map_os_type_categorizes_every_linux_flavour() {
        // Drive every `os_info::Type` variant `map_os_type` classifies
        // as Linux. If a variant is dropped from the or-pattern (a
        // mutation cargo-mutants explicitly tries), at least one of
        // these inputs maps to "Unknown" instead and surfaces the
        // regression here.
        use os_info::Type;
        for t in [
            Type::Linux,
            Type::Alpine,
            Type::Amazon,
            Type::Android,
            Type::Arch,
            Type::Artix,
            Type::CachyOS,
            Type::CentOS,
            Type::Debian,
            Type::EndeavourOS,
            Type::Fedora,
            Type::Garuda,
            Type::Gentoo,
            Type::Kali,
            Type::Mabox,
            Type::Manjaro,
            Type::Mariner,
            Type::Mint,
            Type::NixOS,
            Type::Nobara,
            Type::OpenCloudOS,
            Type::openEuler,
            Type::openSUSE,
            Type::OracleLinux,
            Type::Pop,
            Type::Raspbian,
            Type::Redhat,
            Type::RedHatEnterprise,
            Type::RockyLinux,
            Type::Solus,
            Type::SUSE,
            Type::Ubuntu,
            Type::Ultramarine,
            Type::Uos,
            Type::Void,
        ] {
            assert_eq!(map_os_type(t), "Linux", "expected `{t:?}` to map to Linux");
        }
    }

    #[test]
    fn map_os_type_categorizes_macos_and_windows() {
        // Separate arms — each must produce its own variant. If
        // either match arm is deleted, the corresponding case
        // collapses to "Unknown" and the assertion catches it.
        assert_eq!(map_os_type(os_info::Type::Macos), "Macos");
        assert_eq!(map_os_type(os_info::Type::Windows), "Windows");
    }

    #[test]
    fn map_os_type_falls_back_to_unknown_for_unmapped_variants() {
        // `Unknown` is the catch-all. Use a couple of os_info
        // variants that are deliberately *not* in the Linux/Macos/
        // Windows arms (BSD family, etc.) so we exercise the
        // `_ => "Unknown"` branch.
        assert_eq!(map_os_type(os_info::Type::Unknown), "Unknown");
        assert_eq!(map_os_type(os_info::Type::FreeBSD), "Unknown");
        assert_eq!(map_os_type(os_info::Type::DragonFly), "Unknown");
    }

    #[test]
    fn map_os_arch_normalizes_each_arm() {
        // Every accepted input string is part of the public contract
        // (synonyms collapse to canonical variants); pin them all.
        assert_eq!(map_os_arch(Some("x86_64")), "x86_64");
        assert_eq!(map_os_arch(Some("amd64")), "x86_64");
        assert_eq!(map_os_arch(Some("aarch64")), "aarch64");
        assert_eq!(map_os_arch(Some("arm64")), "aarch64");
        assert_eq!(map_os_arch(Some("arm")), "arm");
        assert_eq!(map_os_arch(Some("x86")), "x86");
        assert_eq!(map_os_arch(Some("i386")), "x86");
        assert_eq!(map_os_arch(Some("i686")), "x86");
    }

    #[test]
    fn map_os_arch_falls_back_to_unknown_for_other_inputs() {
        // Anything outside the recognized set must land on Unknown.
        // Both `None` (os_info couldn't detect) and unfamiliar
        // strings (`mips`, `s390x`, etc.) flow through the same arm.
        assert_eq!(map_os_arch(None), "Unknown");
        assert_eq!(map_os_arch(Some("")), "Unknown");
        assert_eq!(map_os_arch(Some("mips")), "Unknown");
        assert_eq!(map_os_arch(Some("s390x")), "Unknown");
        assert_eq!(map_os_arch(Some("powerpc")), "Unknown");
    }

    #[test]
    fn nullable_match_extracts_inhabitant_end_to_end() {
        // End-to-end: a `String?` is destructured via match, and the
        // non-null arm's bind threads the inhabitant into a template
        // path. Pins the whole path Literal::Null → Value::Null →
        // pattern dispatch → bind narrowing → resource construction.
        let states = run("val maybe_path: String? = \"/opt/app\"\n\
             reconcile match maybe_path {\n\
                 null => template(path = \"/opt/fallback\", source = \"tmpl.tpl\", vars = {\"body\": \"\"}),\n\
                 p => template(path = p, source = \"tmpl.tpl\", vars = {\"body\": \"\"}),\n\
             }\n");
        assert_eq!(states.len(), 1);
        let ResourceState::Template { path, .. } = &states[0] else {
            panic!("expected template, got {:?}", states[0]);
        };
        assert_eq!(path, &PathBuf::from("/opt/app"));
    }

    #[test]
    fn nullable_match_takes_null_arm_when_value_is_null() {
        let states = run("val maybe_path: String? = null\n\
             reconcile match maybe_path {\n\
                 null => template(path = \"/opt/fallback\", source = \"tmpl.tpl\", vars = {\"body\": \"\"}),\n\
                 p => template(path = p, source = \"tmpl.tpl\", vars = {\"body\": \"\"}),\n\
             }\n");
        let ResourceState::Template { path, .. } = &states[0] else {
            panic!("expected template, got {:?}", states[0]);
        };
        assert_eq!(path, &PathBuf::from("/opt/fallback"));
    }

    #[test]
    fn nullable_eq_null_is_true_when_value_is_null() {
        // The one ergonomic exception (`x == null`) end-to-end: the
        // result must be `Boolean(true)` for a null value. A template
        // path is the easiest carrier — we drive the boolean into a
        // string-typed branch via `if`.
        let states = run("val maybe: String? = null\n\
             reconcile if maybe == null {\n\
                 template(path = \"/missing\", source = \"tmpl.tpl\", vars = {\"body\": \"\"})\n\
             } else {\n\
                 template(path = \"/present\", source = \"tmpl.tpl\", vars = {\"body\": \"\"})\n\
             }\n");
        let ResourceState::Template { path, .. } = &states[0] else {
            panic!("expected template, got {:?}", states[0]);
        };
        assert_eq!(path, &PathBuf::from("/missing"));
    }

    /// Mint a per-test environment-variable name. Concurrent
    /// `cargo test` threads share the process env, so each
    /// env-touching test owns a unique name to avoid stomping on the
    /// others.
    fn unique_env_name(prefix: &str) -> String {
        let n = SEQ.fetch_add(1, AtomicOrdering::Relaxed);
        format!("KERON_TEST_{prefix}_{}_{n}", std::process::id())
    }

    /// Set an env var for the lifetime of the test process.
    ///
    /// `std::env::set_var` is `unsafe` in edition 2024 because it can
    /// race with concurrent reads from other threads. Each test here
    /// owns a unique variable name (see [`unique_env_name`]), so no
    /// other thread reads the variables we touch — the unsafety is
    /// confined to this single well-scoped helper.
    #[allow(unsafe_code)]
    fn set_env(name: &str, value: &str) {
        // SAFETY: callers pass a name that no other thread reads. The
        // workspace forbids unsafe outside opt-in test sites; the
        // workspace lint is `deny`, not `forbid`, so this `allow` is
        // honoured.
        unsafe { std::env::set_var(name, value) }
    }

    #[test]
    fn env_returns_value_when_variable_is_set() {
        let name = unique_env_name("ENV_SET");
        set_env(&name, "hello");
        let src = format!(
            "reconcile match env(\"{name}\") {{\n\
                 null => template(path = \"/missing\", source = \"tmpl.tpl\", vars = {{\"body\": \"\"}}),\n\
                 v => template(path = v, source = \"tmpl.tpl\", vars = {{\"body\": \"\"}}),\n\
             }}\n",
        );
        let states = run(&src);
        let ResourceState::Template { path, .. } = &states[0] else {
            panic!("expected template, got {:?}", states[0]);
        };
        assert_eq!(path, &PathBuf::from("hello"));
    }

    #[test]
    fn env_returns_null_when_variable_is_unset() {
        let name = unique_env_name("ENV_UNSET");
        let src = format!(
            "reconcile match env(\"{name}\") {{\n\
                 null => template(path = \"/missing\", source = \"tmpl.tpl\", vars = {{\"body\": \"\"}}),\n\
                 v => template(path = v, source = \"tmpl.tpl\", vars = {{\"body\": \"\"}}),\n\
             }}\n",
        );
        let states = run(&src);
        let ResourceState::Template { path, .. } = &states[0] else {
            panic!("expected template, got {:?}", states[0]);
        };
        assert_eq!(path, &PathBuf::from("/missing"));
    }

    #[test]
    fn env_distinguishes_empty_string_from_unset() {
        // The whole reason the return type is `String?` rather than
        // `String` with empty-string fallback: a deliberately-empty
        // value is set, distinct from "absent". Match must take the
        // bind arm (not the `null` arm) even though the value is `""`.
        let name = unique_env_name("ENV_EMPTY");
        set_env(&name, "");
        let src = format!(
            "reconcile match env(\"{name}\") {{\n\
                 null => template(path = \"/unset\", source = \"tmpl.tpl\", vars = {{\"body\": \"\"}}),\n\
                 v => template(path = \"/set\", source = \"tmpl.tpl\", vars = {{\"body\": \"\"}}),\n\
             }}\n",
        );
        let states = run(&src);
        let ResourceState::Template { path, .. } = &states[0] else {
            panic!("expected template, got {:?}", states[0]);
        };
        assert_eq!(path, &PathBuf::from("/set"));
    }

    #[test]
    fn env_eq_null_is_an_is_set_check() {
        // The ergonomic `== null` exception flows through `env(...)`
        // just like any other nullable. Useful for short guards
        // without a full `match`.
        let name = unique_env_name("ENV_PRESENCE");
        set_env(&name, "x");
        let src = format!(
            "reconcile if env(\"{name}\") == null {{\n\
                 template(path = \"/missing\", source = \"tmpl.tpl\", vars = {{\"body\": \"\"}})\n\
             }} else {{\n\
                 template(path = \"/present\", source = \"tmpl.tpl\", vars = {{\"body\": \"\"}})\n\
             }}\n",
        );
        let states = run(&src);
        let ResourceState::Template { path, .. } = &states[0] else {
            panic!("expected template, got {:?}", states[0]);
        };
        assert_eq!(path, &PathBuf::from("/present"));
    }

    /// Mint a per-test `op://` URI so concurrent tests don't share
    /// the same override slot. Pairs with [`unique_secret_uri`] for
    /// other schemes.
    fn unique_op_uri(label: &str) -> String {
        let n = SEQ.fetch_add(1, AtomicOrdering::Relaxed);
        format!("op://k/test/{label}_{}_{n}", std::process::id())
    }

    /// Build a unique URI for any scheme. The scheme + label
    /// combine into a per-test identifier so multiple tests can
    /// share the same scheme without their overrides colliding.
    fn unique_secret_uri(scheme: &str, label: &str) -> String {
        let n = SEQ.fetch_add(1, AtomicOrdering::Relaxed);
        format!("{scheme}://k/test/{label}_{}_{n}", std::process::id())
    }

    #[test]
    fn secret_op_scheme_resolves_via_test_override() {
        // The override is the test seam: real production calls
        // `op read`, but here we hand the dispatcher a fixed value
        // so we can assert the full secret → unwrap_secret pipeline
        // without an `op` binary.
        let uri = unique_op_uri("ok");
        let _g = secret_test::SecretOverride::ok(&uri, "hunter2");
        let states = run_with_templates(
            &format!(
                "val token: Secret = secret(\"{uri}\")\n\
                 reconcile template(path = \"/secret\", source = \"secret.tpl\", vars = {{\"body\": unwrap_secret(token)}})\n",
            ),
            &[("secret.tpl", "{{ body }}")],
        );
        let ResourceState::Template {
            content, sensitive, ..
        } = &states[0]
        else {
            panic!("expected template, got {:?}", states[0]);
        };
        assert_eq!(content, "hunter2");
        assert!(*sensitive);
    }

    #[test]
    fn secret_infisical_scheme_resolves_via_test_override() {
        // The override map is scheme-agnostic, so a fixed value
        // installed for an `infisical://` URI flows through the
        // same `secret(...) → unwrap_secret(...)` pipeline as `op://`.
        let uri = unique_secret_uri("infisical", "ok");
        let _g = secret_test::SecretOverride::ok(&uri, "ifs-value");
        let states = run_with_templates(
            &format!(
                "val token: Secret = secret(\"{uri}\")\n\
                 reconcile template(path = \"/secret\", source = \"secret.tpl\", vars = {{\"body\": unwrap_secret(token)}})\n",
            ),
            &[("secret.tpl", "{{ body }}")],
        );
        let ResourceState::Template {
            content, sensitive, ..
        } = &states[0]
        else {
            panic!("expected template, got {:?}", states[0]);
        };
        assert_eq!(content, "ifs-value");
        assert!(*sensitive);
    }

    #[test]
    fn secret_bw_scheme_resolves_via_test_override() {
        let uri = unique_secret_uri("bw", "ok");
        let _g = secret_test::SecretOverride::ok(&uri, "bw-value");
        let states = run_with_templates(
            &format!(
                "val token: Secret = secret(\"{uri}\")\n\
                 reconcile template(path = \"/secret\", source = \"secret.tpl\", vars = {{\"body\": unwrap_secret(token)}})\n",
            ),
            &[("secret.tpl", "{{ body }}")],
        );
        let ResourceState::Template {
            content, sensitive, ..
        } = &states[0]
        else {
            panic!("expected template, got {:?}", states[0]);
        };
        assert_eq!(content, "bw-value");
        assert!(*sensitive);
    }

    #[test]
    fn secret_resolution_failure_is_a_plan_error() {
        // The dispatcher wraps the underlying error with the URI,
        // so the failing test message names the offending secret.
        let uri = unique_op_uri("fail");
        let _g = secret_test::SecretOverride::err(&uri, "auth required");
        let proj = TempProject::new("secret-fail");
        let src = format!(
            "val token: Secret = secret(\"{uri}\")\n\
             reconcile template(path = unwrap_secret(token), source = \"tmpl.tpl\", vars = {{\"body\": \"\"}})\n",
        );
        let entry = proj.entry(&src);
        let canonical = fs::canonicalize(&entry).unwrap();
        let base_dir = canonical.parent().unwrap().to_path_buf();
        let keron_root = base_dir.clone();
        let graph = resolve(vec![EntrySource {
            text: src,
            base_dir,
            id: keron_modules::ModuleId::File(canonical),
        }])
        .unwrap_or_else(|errs| panic!("resolve failed: {errs:?}"));
        let err = eval_graph(&graph, &keron_root).expect_err("op failure should bubble up");
        let msg = format!("{err:#}");
        assert!(msg.contains(&uri), "error should name the URI: {msg}");
        assert!(
            msg.contains("auth required"),
            "error should include the simulated failure: {msg}",
        );
    }

    #[test]
    fn secret_unsupported_scheme_is_rejected() {
        let proj = TempProject::new("secret-bad-scheme");
        let src = "val tok: Secret = secret(\"file:///etc/secret\")\n\
                   reconcile template(path = unwrap_secret(tok), source = \"tmpl.tpl\", vars = {\"body\": \"\"})\n";
        let entry = proj.entry(src);
        let canonical = fs::canonicalize(&entry).unwrap();
        let base_dir = canonical.parent().unwrap().to_path_buf();
        let keron_root = base_dir.clone();
        let graph = resolve(vec![EntrySource {
            text: src.to_string(),
            base_dir,
            id: keron_modules::ModuleId::File(canonical),
        }])
        .unwrap_or_else(|errs| panic!("resolve failed: {errs:?}"));
        let err = eval_graph(&graph, &keron_root).expect_err("unsupported scheme should fail");
        let msg = format!("{err:#}");
        // The diagnostic must list every scheme we *do* support so a
        // typo in the URI ("opp://" / "vault://" / etc.) surfaces the
        // canonical set rather than silently failing.
        for scheme in ["op://", "infisical://", "bw://"] {
            assert!(
                msg.contains(scheme),
                "unsupported-scheme error should hint at `{scheme}`: {msg}",
            );
        }
    }

    #[test]
    fn secret_unwrap_round_trips_through_template_vars() {
        // Full pipeline: secret → unwrap_secret → template var. The
        // user has explicitly opted into using the value by calling
        // `unwrap_secret`; the rendered content is stored for apply
        // but marked sensitive so plan/diff rendering can redact it.
        let uri = unique_op_uri("template");
        let _g = secret_test::SecretOverride::ok(&uri, "deploy-key-abc");
        let states = run_with_templates(
            &format!(
                "val token: Secret = secret(\"{uri}\")\n\
                 reconcile template(\n\
                     \tpath = \"/etc/auth\",\n\
                     \tsource = \"auth.tpl\",\n\
                     \tvars = {{\"token\": unwrap_secret(token)}},\n\
                 )\n",
            ),
            &[("auth.tpl", "TOKEN={{ token }}\n")],
        );
        let ResourceState::Template {
            content, sensitive, ..
        } = &states[0]
        else {
            panic!("expected template, got {:?}", states[0]);
        };
        assert_eq!(content, "TOKEN=deploy-key-abc\n");
        assert!(*sensitive);
    }

    #[test]
    fn secret_taint_survives_string_concat() {
        let uri = unique_op_uri("concat");
        let _g = secret_test::SecretOverride::ok(&uri, "deploy-key");
        let states = run_with_templates(
            &format!(
                "val token: Secret = secret(\"{uri}\")\n\
                 reconcile template(path = \"/etc/auth\", source = \"auth.tpl\", vars = {{\"token\": unwrap_secret(token) + \"-abc\"}})\n",
            ),
            &[("auth.tpl", "TOKEN={{ token }}\n")],
        );
        let ResourceState::Template {
            content, sensitive, ..
        } = &states[0]
        else {
            panic!("expected template, got {:?}", states[0]);
        };
        assert_eq!(content, "TOKEN=deploy-key-abc\n");
        assert!(*sensitive);
    }

    #[test]
    fn secret_taint_survives_interpolation() {
        let uri = unique_op_uri("interpolation");
        let _g = secret_test::SecretOverride::ok(&uri, "deploy-key");
        let states = run_with_templates(
            &format!(
                "val token: Secret = secret(\"{uri}\")\n\
                 reconcile template(path = \"/etc/auth\", source = \"auth.tpl\", vars = {{\"token\": \"prefix-${{unwrap_secret(token)}}\"}})\n",
            ),
            &[("auth.tpl", "TOKEN={{ token }}\n")],
        );
        let ResourceState::Template {
            content, sensitive, ..
        } = &states[0]
        else {
            panic!("expected template, got {:?}", states[0]);
        };
        assert_eq!(content, "TOKEN=prefix-deploy-key\n");
        assert!(*sensitive);
    }

    /// Drive a manifest that builds a `secret("<uri>")` resource
    /// through the full pipeline and return the eval error. Used by
    /// the URI-validation tests below — no `SecretOverride` is
    /// installed, so the real resolver's parse step fires before any
    /// CLI invocation, which means these tests work on machines
    /// without the underlying CLIs.
    fn eval_secret_uri_err(uri: &str, project_label: &str) -> String {
        let proj = TempProject::new(project_label);
        let src = format!(
            "val tok: Secret = secret(\"{uri}\")\n\
             reconcile template(path = unwrap_secret(tok), source = \"tmpl.tpl\", vars = {{\"body\": \"\"}})\n",
        );
        let entry = proj.entry(&src);
        let canonical = fs::canonicalize(&entry).unwrap();
        let base_dir = canonical.parent().unwrap().to_path_buf();
        let keron_root = base_dir.clone();
        let graph = resolve(vec![EntrySource {
            text: src,
            base_dir,
            id: keron_modules::ModuleId::File(canonical),
        }])
        .unwrap_or_else(|errs| panic!("resolve failed: {errs:?}"));
        let err = eval_graph(&graph, &keron_root).expect_err("URI should fail validation");
        format!("{err:#}")
    }

    #[test]
    fn secret_infisical_uri_requires_env_and_name() {
        // Both halves of the URI must be present — neither
        // `infisical://just-env` nor `infisical:///bare-name` is
        // resolvable, since the CLI needs both.
        for bad in [
            "infisical://just-env-no-name",
            "infisical:///bare-name-no-env",
            "infisical://env-no-trailing-slash/",
        ] {
            let msg = eval_secret_uri_err(bad, "secret-infisical-bad-uri");
            assert!(
                msg.contains("infisical://<env>/<name>"),
                "error should show the expected URI shape for `{bad}`: {msg}",
            );
        }
    }

    #[test]
    fn secret_bw_uri_rejects_empty_item() {
        // `bw://` with nothing after it has no item to fetch; the
        // CLI would fail with "no item specified" anyway, but we
        // catch it at parse time so the diagnostic is clean.
        let msg = eval_secret_uri_err("bw://", "secret-bw-empty");
        assert!(
            msg.contains("bw://<item>"),
            "error should show the expected URI shape: {msg}",
        );
    }

    /// Build a synthetic `std::process::Output` so the decoder tests
    /// below can exercise the success / failure branches without
    /// invoking a real CLI. The status is built via platform-specific
    /// `ExitStatusExt::from_raw`; on Unix the value is a raw wait
    /// status, on Windows it's the exit code.
    fn make_output(success: bool, stdout: &[u8], stderr: &[u8]) -> std::process::Output {
        #[cfg(unix)]
        let status = {
            use std::os::unix::process::ExitStatusExt;
            // Wait-status `0` = exited normally with code 0;
            // `1 << 8` = exited normally with code 1.
            std::process::ExitStatus::from_raw(if success { 0 } else { 1 << 8 })
        };
        #[cfg(windows)]
        let status = {
            use std::os::windows::process::ExitStatusExt;
            std::process::ExitStatus::from_raw(if success { 0 } else { 1 })
        };
        std::process::Output {
            status,
            stdout: stdout.to_vec(),
            stderr: stderr.to_vec(),
        }
    }

    #[test]
    fn take_stdout_returns_utf8_with_trailing_newline_trimmed() {
        // The shared decoder helper handles UTF-8 decoding + a one-
        // newline trim. Pin both behaviours: the payload survives
        // verbatim and exactly one `\n` is removed from the end (a
        // second is left in place).
        let v = take_stdout(b"hello\n".to_vec(), "ctx").expect("utf-8 ok");
        assert_eq!(v, "hello");
        let v = take_stdout(b"hello\n\n".to_vec(), "ctx").expect("utf-8 ok");
        assert_eq!(v, "hello\n");
        let v = take_stdout(b"".to_vec(), "ctx").expect("empty ok");
        assert_eq!(v, "");
        let v = take_stdout(b"no-newline".to_vec(), "ctx").expect("no trailing nl ok");
        assert_eq!(v, "no-newline");
    }

    #[test]
    fn take_stdout_errors_on_non_utf8_with_command_context() {
        // 0xFF is an invalid UTF-8 start byte. The error must
        // mention the command description so the user can locate
        // which CLI produced the garbage.
        let err = take_stdout(vec![0xFF, 0xFE], "op read x").expect_err("not utf-8");
        let msg = format!("{err:#}");
        assert!(msg.contains("op read x"), "missing command context: {msg}");
        assert!(msg.contains("non-UTF-8"), "missing decode hint: {msg}");
    }

    #[test]
    fn decode_op_output_returns_stdout_on_success() {
        let out = make_output(true, b"hunter2\n", b"");
        let v = decode_op_output("op://Vault/Item/x", out).expect("ok");
        assert_eq!(v, "hunter2");
    }

    #[test]
    fn decode_op_output_surfaces_stderr_on_failure() {
        // Failure path: the URI and the trimmed stderr both make it
        // into the diagnostic so the user can locate the offending
        // secret without re-running the CLI by hand.
        let out = make_output(false, b"", b"  auth required  \n");
        let err = decode_op_output("op://X/Y/Z", out).expect_err("status failed");
        let msg = format!("{err:#}");
        assert!(msg.contains("op://X/Y/Z"), "missing uri: {msg}");
        assert!(msg.contains("auth required"), "missing stderr: {msg}");
        assert!(
            !msg.contains("  auth required  "),
            "stderr should be trimmed: {msg}",
        );
    }

    #[test]
    fn parse_infisical_uri_extracts_env_and_name() {
        let (env, name) =
            parse_infisical_uri("infisical://prod/api-key", "prod/api-key").expect("ok");
        assert_eq!(env, "prod");
        assert_eq!(name, "api-key");
    }

    #[test]
    fn parse_infisical_uri_rejects_each_malformed_shape() {
        // Both halves must be non-empty: empty env, empty name, and
        // missing separator each surface the canonical URI shape so
        // the user can fix the typo.
        for (uri, rest) in [
            ("infisical://prod", "prod"),
            ("infisical:///bare-name", "/bare-name"),
            ("infisical://prod/", "prod/"),
        ] {
            let err = parse_infisical_uri(uri, rest).expect_err("malformed URI should fail");
            let msg = format!("{err:#}");
            assert!(
                msg.contains("infisical://<env>/<name>"),
                "missing canonical shape for `{uri}`: {msg}",
            );
        }
    }

    #[test]
    fn decode_infisical_output_returns_stdout_on_success() {
        let out = make_output(true, b"infisical-value\n", b"");
        let v = decode_infisical_output("prod", "api-key", out).expect("ok");
        assert_eq!(v, "infisical-value");
    }

    #[test]
    fn decode_infisical_output_surfaces_stderr_on_failure() {
        let out = make_output(false, b"", b"item not found\n");
        let err = decode_infisical_output("prod", "api-key", out).expect_err("failed");
        let msg = format!("{err:#}");
        assert!(msg.contains("prod"), "missing env: {msg}");
        assert!(msg.contains("api-key"), "missing name: {msg}");
        assert!(msg.contains("item not found"), "missing stderr: {msg}");
    }

    #[test]
    fn parse_bw_uri_defaults_field_to_password() {
        let (item, field) = parse_bw_uri("bw://github-login", "github-login").expect("ok");
        assert_eq!(item, "github-login");
        assert_eq!(field, "password");
    }

    #[test]
    fn parse_bw_uri_extracts_explicit_field() {
        let (item, field) =
            parse_bw_uri("bw://github-login/username", "github-login/username").expect("ok");
        assert_eq!(item, "github-login");
        assert_eq!(field, "username");
    }

    #[test]
    fn parse_bw_uri_rejects_empty_item_or_field() {
        for (uri, rest) in [
            ("bw://", ""),
            ("bw:///username", "/username"),
            ("bw://github-login/", "github-login/"),
        ] {
            let err = parse_bw_uri(uri, rest).expect_err("malformed URI should fail");
            let msg = format!("{err:#}");
            assert!(
                msg.contains("bw://<item>"),
                "missing canonical shape for `{uri}`: {msg}",
            );
        }
    }

    #[test]
    fn decode_bw_output_returns_stdout_on_success() {
        let out = make_output(true, b"super-pw\n", b"");
        let v = decode_bw_output("github-login", "password", out).expect("ok");
        assert_eq!(v, "super-pw");
    }

    #[test]
    fn decode_bw_output_surfaces_stderr_on_failure() {
        let out = make_output(false, b"", b"vault is locked\n");
        let err = decode_bw_output("github-login", "password", out).expect_err("failed");
        let msg = format!("{err:#}");
        assert!(msg.contains("github-login"), "missing item: {msg}");
        assert!(msg.contains("password"), "missing field: {msg}");
        assert!(msg.contains("vault is locked"), "missing stderr: {msg}");
    }

    #[test]
    fn secret_value_debug_redacts_payload() {
        // Manual `Debug` impl is the last line of defence against a
        // leak via `dbg!`, panic backtraces, or any auto-derived
        // Debug elsewhere in the stack. The payload must never
        // appear in the formatted output; the byte length is fine
        // (it's structural and helps "did the resolver get an empty
        // string back?" debugging without exposing content).
        let v = Value::Secret("super-sensitive".into());
        let formatted = format!("{v:?}");
        assert!(
            !formatted.contains("super-sensitive"),
            "Debug must not leak payload: {formatted}",
        );
        assert!(
            formatted.contains("redacted"),
            "Debug should mark the value as redacted: {formatted}",
        );
        // Length leaks structural info only. 15 is `"super-sensitive".len()`.
        assert!(
            formatted.contains("15"),
            "Debug should include the byte length: {formatted}",
        );
    }

    #[test]
    fn sensitive_string_debug_redacts_payload() {
        let v = Value::sensitive_string("deploy-key-abc");
        let formatted = format!("{v:?}");
        assert!(
            !formatted.contains("deploy-key-abc"),
            "Debug must not leak payload: {formatted}",
        );
        assert!(
            formatted.contains("sensitive"),
            "Debug should mark the value as sensitive: {formatted}",
        );
    }

    #[test]
    fn brew_builds_a_package_resource_with_brew_manager() {
        let states = run("reconcile brew(\"ripgrep\")\n");
        assert_eq!(states.len(), 1);
        let ResourceState::Package { manager, name } = &states[0] else {
            panic!("expected Package, got {:?}", states[0]);
        };
        assert_eq!(*manager, PackageManager::Brew);
        assert_eq!(name, "ripgrep");
    }

    #[test]
    fn cargo_builds_a_package_resource_with_cargo_manager() {
        let states = run("reconcile cargo(\"sccache\")\n");
        let ResourceState::Package { manager, name } = &states[0] else {
            panic!("expected Package, got {:?}", states[0]);
        };
        assert_eq!(*manager, PackageManager::Cargo);
        assert_eq!(name, "sccache");
    }

    #[test]
    fn winget_builds_a_package_resource_with_winget_manager() {
        let states = run("reconcile winget(\"Microsoft.PowerShell\")\n");
        let ResourceState::Package { manager, name } = &states[0] else {
            panic!("expected Package, got {:?}", states[0]);
        };
        assert_eq!(*manager, PackageManager::Winget);
        assert_eq!(name, "Microsoft.PowerShell");
    }

    #[test]
    fn empty_package_name_is_rejected_at_eval() {
        // The type checker only proves the name is a `String`, not
        // that it's non-empty; the dispatcher enforces the
        // non-emptiness so an apply step never has to special-case
        // an empty `brew install` invocation. The diagnostic names
        // the manager so the user can locate the offending call.
        let proj = TempProject::new("brew-empty-name");
        let src = "reconcile brew(\"\")\n";
        let entry = proj.entry(src);
        let canonical = fs::canonicalize(&entry).unwrap();
        let base_dir = canonical.parent().unwrap().to_path_buf();
        let keron_root = base_dir.clone();
        let graph = resolve(vec![EntrySource {
            text: src.to_string(),
            base_dir,
            id: keron_modules::ModuleId::File(canonical),
        }])
        .unwrap_or_else(|errs| panic!("resolve failed: {errs:?}"));
        let err = eval_graph(&graph, &keron_root).expect_err("empty name should fail");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("brew") && msg.contains("empty"),
            "diagnostic should name brew + empty: {msg}",
        );
    }

    #[test]
    fn reconcile_can_mix_package_and_filesystem_resources() {
        // The widening rule means a reconcile arm or list can hold
        // packages alongside filesystem resources. Pins that they
        // coexist in the resulting plan in source order. The symlink
        // target is seeded inside the keron root so the new
        // `resolve_managed_path` check passes.
        let states = run_with_templates(
            "reconcile {\n\
                 brew(\"ripgrep\");\n\
                 symlink(from = \"/from\", to = \"./inside\");\n\
                 cargo(\"sccache\");\n\
             }\n",
            &[("inside", "")],
        );
        assert_eq!(states.len(), 3);
        assert!(
            matches!(&states[0], ResourceState::Package { manager: PackageManager::Brew, name } if name == "ripgrep"),
        );
        assert!(matches!(&states[1], ResourceState::Symlink { .. }));
        assert!(
            matches!(&states[2], ResourceState::Package { manager: PackageManager::Cargo, name } if name == "sccache"),
        );
    }

    #[test]
    fn symlink_to_relative_path_resolves_inside_keron_root() {
        // `to = "./zshrc"` reads from the entry's directory; the
        // resolved target is canonical and lives inside the keron
        // root, so the executor never sees the raw user string.
        let states = run_with_templates(
            "reconcile symlink(from = \"/dest\", to = \"./zshrc\")\n",
            &[("zshrc", "export PATH=...")],
        );
        let ResourceState::Symlink { to, .. } = &states[0] else {
            panic!("expected Symlink, got {:?}", states[0]);
        };
        assert!(to.is_absolute(), "to should be canonical: {}", to.display());
        let last = to.file_name().unwrap();
        assert_eq!(last, "zshrc");
    }

    #[test]
    fn symlink_to_absolute_path_inside_keron_root_is_accepted() {
        // Most user code interpolates `keron_root()` to build the `to`
        // argument; the absolute path it produces must still pass the
        // containment check.
        let proj = TempProject::new("symlink-keron-root");
        proj.seed_template("zshrc", "export PATH=...");
        let src = "reconcile symlink(from = \"/dest\", to = \"${keron_root()}/zshrc\")\n";
        let entry = proj.entry(src);
        let canonical = fs::canonicalize(&entry).unwrap();
        let base_dir = canonical.parent().unwrap().to_path_buf();
        let keron_root = base_dir.clone();
        let graph = resolve(vec![EntrySource {
            text: src.into(),
            base_dir,
            id: keron_modules::ModuleId::File(canonical),
        }])
        .unwrap_or_else(|errs| panic!("resolve failed: {errs:?}"));
        let states = eval_graph(&graph, &keron_root).unwrap();
        let ResourceState::Symlink { to, .. } = &states[0] else {
            panic!("expected Symlink");
        };
        assert!(
            to.starts_with(&keron_root),
            "to outside root: {}",
            to.display()
        );
    }

    #[test]
    fn symlink_to_absolute_path_outside_keron_root_is_rejected() {
        // `/etc/hosts` exists on every test host but is not inside
        // the temp keron root. The diagnostic must name the argument,
        // the user value, and the keron root so the user can see
        // exactly what is being refused and why.
        let proj = TempProject::new("symlink-outside");
        let src = "reconcile symlink(from = \"/dest\", to = \"/etc/hosts\")\n";
        let entry = proj.entry(src);
        let canonical = fs::canonicalize(&entry).unwrap();
        let base_dir = canonical.parent().unwrap().to_path_buf();
        let keron_root = base_dir.clone();
        let graph = resolve(vec![EntrySource {
            text: src.into(),
            base_dir,
            id: keron_modules::ModuleId::File(canonical),
        }])
        .unwrap_or_else(|errs| panic!("resolve failed: {errs:?}"));
        let err = eval_graph(&graph, &keron_root).expect_err("path outside root must be refused");
        let msg = format!("{err:#}");
        assert!(msg.contains("symlink"), "should name the kind: {msg}");
        assert!(msg.contains("`to`"), "should name the argument: {msg}");
        assert!(
            msg.contains("/etc/hosts"),
            "should echo the user value: {msg}"
        );
        assert!(
            msg.contains("outside the keron root"),
            "should explain why: {msg}",
        );
    }

    #[test]
    fn symlink_to_dotdot_escape_is_rejected() {
        // `to = "../escape"` is a relative form that lands outside
        // the root after `..` is consumed; canonicalization fails
        // open into the containment check, not silently accepts.
        let proj = TempProject::new("symlink-dotdot");
        // Seed an `escape` file *next to* the keron root so the
        // `../escape` traversal actually points at a real file (so
        // canonicalize succeeds and we exercise the containment
        // check, not just the "file not found" path).
        let parent = proj.root.parent().unwrap();
        let escape = parent.join("keron-test-escape.tmp");
        fs::write(&escape, "x").unwrap();
        let src = "reconcile symlink(from = \"/dest\", to = \"../keron-test-escape.tmp\")\n";
        let entry = proj.entry(src);
        let canonical = fs::canonicalize(&entry).unwrap();
        let base_dir = canonical.parent().unwrap().to_path_buf();
        let keron_root = base_dir.clone();
        let graph = resolve(vec![EntrySource {
            text: src.into(),
            base_dir,
            id: keron_modules::ModuleId::File(canonical),
        }])
        .unwrap_or_else(|errs| panic!("resolve failed: {errs:?}"));
        let err = eval_graph(&graph, &keron_root).expect_err("dotdot escape must be refused");
        assert!(
            format!("{err:#}").contains("outside the keron root"),
            "got: {err:#}",
        );
        let _ = fs::remove_file(&escape);
    }

    #[test]
    fn symlink_to_missing_path_errors_with_locating_context() {
        // The path resolves to a file that does not exist; canonicalize
        // fails. The error chain must mention the kind, the argument
        // name, the user-supplied value, and where we looked — that's
        // what makes the diagnostic locatable rather than the bare
        // io::Error.
        let proj = TempProject::new("symlink-missing");
        let src = "reconcile symlink(from = \"/dest\", to = \"./not-there\")\n";
        let entry = proj.entry(src);
        let canonical = fs::canonicalize(&entry).unwrap();
        let base_dir = canonical.parent().unwrap().to_path_buf();
        let keron_root = base_dir.clone();
        let graph = resolve(vec![EntrySource {
            text: src.into(),
            base_dir,
            id: keron_modules::ModuleId::File(canonical),
        }])
        .unwrap_or_else(|errs| panic!("resolve failed: {errs:?}"));
        let err = eval_graph(&graph, &keron_root).expect_err("missing target must error");
        let msg = format!("{err:#}");
        assert!(msg.contains("symlink"), "kind missing: {msg}");
        assert!(msg.contains("`to`"), "arg name missing: {msg}");
        assert!(msg.contains("not-there"), "value missing: {msg}");
    }

    #[cfg(unix)]
    #[test]
    fn symlink_to_a_symlink_is_rejected() {
        // `to = "./alias"` where `alias` is itself a symlink would
        // chain indirection. Refuse loudly rather than canonicalize
        // through; the user almost certainly meant to point at the
        // underlying file.
        let proj = TempProject::new("symlink-to-symlink");
        let real = proj.root.join("real.txt");
        fs::write(&real, "hi").unwrap();
        std::os::unix::fs::symlink(&real, proj.root.join("alias")).unwrap();
        let src = "reconcile symlink(from = \"/dest\", to = \"./alias\")\n";
        let entry = proj.entry(src);
        let canonical = fs::canonicalize(&entry).unwrap();
        let base_dir = canonical.parent().unwrap().to_path_buf();
        let keron_root = base_dir.clone();
        let graph = resolve(vec![EntrySource {
            text: src.into(),
            base_dir,
            id: keron_modules::ModuleId::File(canonical),
        }])
        .unwrap_or_else(|errs| panic!("resolve failed: {errs:?}"));
        let err = eval_graph(&graph, &keron_root).expect_err("symlink-to-symlink must be refused");
        let msg = format!("{err:#}");
        assert!(msg.contains("symlink"), "kind missing: {msg}");
        assert!(msg.contains("`to`"), "arg name missing: {msg}");
        assert!(
            msg.contains("only manages real files"),
            "real-files-only message missing: {msg}",
        );
    }

    #[cfg(unix)]
    #[test]
    fn template_source_that_is_a_symlink_is_rejected() {
        // Same rule for templates: `source` must be a real file, not
        // a symlink. Without the leaf check, `canonicalize` would
        // silently dereference and the user'd never see that they
        // pointed at a link.
        let proj = TempProject::new("template-source-symlink");
        let real = proj.root.join("real.tpl");
        fs::write(&real, "hi").unwrap();
        std::os::unix::fs::symlink(&real, proj.root.join("alias.tpl")).unwrap();
        let src = "reconcile template(path = \"/dest\", source = \"./alias.tpl\", vars = {})\n";
        let entry = proj.entry(src);
        let canonical = fs::canonicalize(&entry).unwrap();
        let base_dir = canonical.parent().unwrap().to_path_buf();
        let keron_root = base_dir.clone();
        let graph = resolve(vec![EntrySource {
            text: src.into(),
            base_dir,
            id: keron_modules::ModuleId::File(canonical),
        }])
        .unwrap_or_else(|errs| panic!("resolve failed: {errs:?}"));
        let err =
            eval_graph(&graph, &keron_root).expect_err("template-from-symlink must be refused");
        let msg = format!("{err:#}");
        assert!(msg.contains("template"), "kind missing: {msg}");
        assert!(msg.contains("`source`"), "arg name missing: {msg}");
        assert!(
            msg.contains("only manages real files"),
            "real-files-only message missing: {msg}",
        );
    }

    #[test]
    fn template_source_outside_keron_root_is_rejected() {
        // Same containment rule applies to `template(source = ...)`.
        // An absolute path pointing outside the keron root errors
        // before the file is even read.
        let proj = TempProject::new("template-outside");
        let src = "reconcile template(path = \"/dest\", source = \"/etc/hosts\", vars = {\"body\": \"\"})\n";
        let entry = proj.entry(src);
        let canonical = fs::canonicalize(&entry).unwrap();
        let base_dir = canonical.parent().unwrap().to_path_buf();
        let keron_root = base_dir.clone();
        let graph = resolve(vec![EntrySource {
            text: src.into(),
            base_dir,
            id: keron_modules::ModuleId::File(canonical),
        }])
        .unwrap_or_else(|errs| panic!("resolve failed: {errs:?}"));
        let err =
            eval_graph(&graph, &keron_root).expect_err("template outside root must be refused");
        let msg = format!("{err:#}");
        assert!(msg.contains("template"), "kind missing: {msg}");
        assert!(msg.contains("`source`"), "arg name missing: {msg}");
        assert!(
            msg.contains("outside the keron root"),
            "containment message missing: {msg}",
        );
    }
}
