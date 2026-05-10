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
use std::path::PathBuf;

use anyhow::{Context, Result, anyhow, bail};
use keron_lang::{
    BinOp, Block, CallArg, Expr, FnDecl, ForPattern, IntrinsicId, Item, Literal, MapEntry,
    MatchArm, Pattern, Spanned, Stmt, StringPart, StructDecl, StructPatternField, UnaryOp,
};
use keron_modules::{ModuleGraph, ModuleId, stdlib};

use crate::plan::ResourceState;

#[derive(Debug, Clone)]
enum Value {
    String(String),
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
}

impl Value {
    fn type_name(&self) -> String {
        match self {
            Self::String(_) => "String".into(),
            Self::Int(_) => "Int".into(),
            Self::Bool(_) => "Boolean".into(),
            Self::Double(_) => "Double".into(),
            Self::List(_) => "List".into(),
            Self::Map(_) => "Map".into(),
            Self::Resource(_) => "Resource".into(),
            Self::Struct { name, .. } => name.clone(),
            Self::Void => "Void".into(),
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
            // Vals evaluate in their owning module's scope, with no
            // borrowed locals.
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

pub fn eval_graph(graph: &ModuleGraph) -> Result<Vec<ResourceState>> {
    let mut graph_top = GraphTop {
        modules: HashMap::new(),
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
                // Shorthand `Point { x }` — bind the field's value to
                // a binding named after the field.
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
        Literal::String(s) => Value::String(s.clone()),
        Literal::Int(n) => Value::Int(*n),
        Literal::Boolean(b) => Value::Bool(*b),
        Literal::Double(d) => Value::Double(*d),
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
        (Add, Value::String(a), Value::String(b)) => Ok(Value::String(a + &b)),
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
        (Value::String(x), Value::String(y)) => x == y,
        (Value::Int(x), Value::Int(y)) => x == y,
        (Value::Bool(x), Value::Bool(y)) => x == y,
        (Value::Double(x), Value::Double(y)) => x == y,
        (Value::Int(x), Value::Double(y)) => (*x as f64) == *y,
        (Value::Double(x), Value::Int(y)) => *x == (*y as f64),
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
        (Value::String(x), Value::String(y)) => Ok(x.cmp(y)),
        (a, b) => bail!("ordering on {} / {}", a.type_name(), b.type_name()),
    }
}

fn eval_interpolation(parts: &[StringPart], env: &Env<'_, '_>) -> Result<Value> {
    let mut out = String::new();
    for part in parts {
        match part {
            StringPart::Text(s) => out.push_str(s),
            StringPart::Expr(e) => {
                let v = eval_expr(e, env)?;
                stringify(&v, &mut out)?;
            }
        }
    }
    Ok(Value::String(out))
}

fn stringify(v: &Value, out: &mut String) -> Result<()> {
    use std::fmt::Write as _;
    match v {
        Value::String(s) => out.push_str(s),
        Value::Int(n) => {
            let _ = write!(out, "{n}");
        }
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Double(d) => {
            let _ = write!(out, "{d}");
        }
        other => bail!("cannot interpolate {}", other.type_name()),
    }
    Ok(())
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

    let bindings = bind_params(fn_decl, args, env)?;
    let mut call_env = Env::new(env.graph, origin_id);
    call_env.local = bindings;

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
        IntrinsicId::Directory => {
            let path = call_string(args, env, "path", 0)?;
            Ok(Value::Resource(ResourceState::Directory {
                path: PathBuf::from(path),
            }))
        }
        IntrinsicId::Symlink => {
            let from = call_string(args, env, "from", 0)?;
            let to = call_string(args, env, "to", 1)?;
            Ok(Value::Resource(ResourceState::Symlink {
                from: PathBuf::from(from),
                to: PathBuf::from(to),
            }))
        }
        IntrinsicId::Template => dispatch_template(args, env),
    }
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
    let vars = call_string_map(args, env, "vars", 2)?;
    let resolved = resolve_template_path(&source, env);
    let raw = std::fs::read_to_string(&resolved).with_context(|| {
        format!(
            "could not read template source `{}` (resolved to `{}`)",
            source,
            resolved.display()
        )
    })?;
    let rendered =
        render_template(&raw, &vars).with_context(|| format!("rendering template `{source}`"))?;
    Ok(Value::Resource(ResourceState::Template {
        path: PathBuf::from(path),
        content: rendered,
    }))
}

/// Resolve a template path relative to the importing module's
/// directory. Absolute paths are taken as-is; relative paths join
/// against the module's parent directory. The current module's id is
/// always a [`ModuleId::File`] in v1, so we can always recover that
/// directory.
fn resolve_template_path(path: &str, env: &Env<'_, '_>) -> PathBuf {
    let candidate = PathBuf::from(path);
    if candidate.is_absolute() {
        return candidate;
    }
    let ModuleId::File(module_path) = &env.current;
    match module_path.parent() {
        Some(parent) => parent.join(candidate),
        None => candidate,
    }
}

/// Substitute `${name}` placeholders in `src` with values from
/// `vars`. A placeholder name that isn't in `vars` is an error; an
/// unterminated `${` (no closing `}`) is also an error. Other `$`
/// occurrences pass through literally — including a trailing `$`
/// at the end of input. Char-iteration here keeps the implementation
/// UTF-8-clean without manual byte arithmetic.
fn render_template(src: &str, vars: &HashMap<String, String>) -> Result<String> {
    let mut out = String::with_capacity(src.len());
    let mut chars = src.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '$' && chars.peek() == Some(&'{') {
            chars.next(); // consume the opening `{`
            let mut name = String::new();
            let mut closed = false;
            for nc in chars.by_ref() {
                if nc == '}' {
                    closed = true;
                    break;
                }
                name.push(nc);
            }
            if !closed {
                bail!("unterminated `${{` in template");
            }
            match vars.get(&name) {
                Some(v) => out.push_str(v),
                None => bail!("template variable `{name}` not provided"),
            }
        } else {
            out.push(c);
        }
    }
    Ok(out)
}

fn bind_params(
    fn_decl: &FnDecl,
    args: &[CallArg],
    env: &Env<'_, '_>,
) -> Result<HashMap<String, Value>> {
    let mut bound = HashMap::new();
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
            eval_expr(default, env)?
        } else {
            bail!("missing argument for parameter `{}`", param.name.node);
        };
        bound.insert(param.name.node.clone(), value);
    }
    Ok(bound)
}

fn call_string(
    args: &[CallArg],
    env: &Env<'_, '_>,
    name: &str,
    positional_idx: usize,
) -> Result<String> {
    let v = eval_call_arg(args, env, name, positional_idx)?;
    match v {
        Value::String(s) => Ok(s),
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
) -> Result<HashMap<String, String>> {
    let v = eval_call_arg(args, env, name, positional_idx)?;
    let Value::Map(entries) = v else {
        bail!(
            "expected Map<String, String> for `{name}`, got {}",
            v.type_name()
        );
    };
    let mut out = HashMap::with_capacity(entries.len());
    for (k, val) in entries {
        let (Value::String(k), Value::String(val)) = (k, val) else {
            bail!("expected Map<String, String> entries for `{name}`");
        };
        out.insert(k, val);
    }
    Ok(out)
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
            // replacement for the old `template(path = X, source = \"tmpl.tpl\", vars = {\"body\": Y})`
            // shape. Tests that care about template-level mechanics
            // (multiple placeholders, missing vars, etc.) seed their
            // own template file via `seed_template`.
            fs::write(root.join("tmpl.tpl"), "${body}").expect("seed default template");
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
    /// auto-seeds a `tmpl.tpl` template (single `${body}`
    /// placeholder); tests that need richer templates use
    /// [`run_with_templates`].
    fn run(src: &str) -> Vec<ResourceState> {
        run_with_templates(src, &[])
    }

    fn run_with_templates(src: &str, templates: &[(&str, &str)]) -> Vec<ResourceState> {
        let proj = TempProject::new("run");
        for (name, content) in templates {
            proj.seed_template(name, content);
        }
        let entry = proj.entry(src);
        let canonical = fs::canonicalize(&entry).unwrap();
        let base_dir = canonical.parent().unwrap().to_path_buf();
        let graph = resolve(vec![EntrySource {
            text: src.to_string(),
            base_dir,
            id: keron_modules::ModuleId::File(canonical),
        }])
        .unwrap_or_else(|errs| panic!("resolve failed: {errs:?}"));
        eval_graph(&graph).unwrap_or_else(|e| panic!("eval failed: {e}"))
    }

    fn first_file_path(states: &[ResourceState]) -> &PathBuf {
        match &states[0] {
            ResourceState::Template { path, .. } | ResourceState::Directory { path } => path,
            ResourceState::Symlink { from, .. } => from,
        }
    }

    fn first_file_content(states: &[ResourceState]) -> &str {
        match &states[0] {
            ResourceState::Template { content, .. } => content.as_str(),
            _ => panic!("expected Template"),
        }
    }

    // ---------- type_name ----------

    #[test]
    fn value_type_name_returns_canonical_strings() {
        assert_eq!(Value::String(String::new()).type_name(), "String");
        assert_eq!(Value::Int(0).type_name(), "Int");
        assert_eq!(Value::Bool(false).type_name(), "Boolean");
        assert_eq!(Value::Double(0.0).type_name(), "Double");
        assert_eq!(Value::List(Vec::new()).type_name(), "List");
        assert_eq!(Value::Map(Vec::new()).type_name(), "Map");
        assert_eq!(
            Value::Resource(ResourceState::Directory {
                path: PathBuf::from("/tmp"),
            })
            .type_name(),
            "Resource"
        );
        assert_eq!(Value::Void.type_name(), "Void");
    }

    // ---------- eval_unary ----------

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

    // ---------- eval_binop arithmetic ----------

    fn int(n: i64) -> Value {
        Value::Int(n)
    }
    fn dbl(d: f64) -> Value {
        Value::Double(d)
    }
    fn s(v: &str) -> Value {
        Value::String(v.into())
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
            Value::String(s) => assert_eq!(s, expected),
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

    // ---------- value_eq ----------

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

    // ---------- value_cmp ----------

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

    // ---------- stringify ----------

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

    // ---------- end-to-end via run() ----------

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

    // ---------- bind_params / call_string ----------

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
            &[("body.tpl", "${body}")],
        );
        assert_eq!(first_file_path(&states), &PathBuf::from("/positional"));
        assert_eq!(first_file_content(&states), "hi");
    }

    // ---------- cycle detection ----------

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

    // ---------- structs / unions / match ----------

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

    // ---------- template intrinsic ----------

    #[test]
    fn template_substitutes_vars_into_resource_content() {
        // Render `${user}` and `${shell}` from the supplied vars map
        // and verify the resulting Template resource carries the
        // substituted text. Pins both `dispatch_template`'s arg
        // routing (path / source / vars) and `render_template`'s
        // placeholder substitution.
        let states = run_with_templates(
            "reconcile template(\n\
                 \tpath = \"/etc/passwd\",\n\
                 \tsource = \"shell.tpl\",\n\
                 \tvars = {\"user\": \"alice\", \"shell\": \"/bin/zsh\"},\n\
             )\n",
            &[("shell.tpl", "${user}:x:1000:${shell}\n")],
        );
        assert_eq!(states.len(), 1);
        assert_eq!(first_file_path(&states), &PathBuf::from("/etc/passwd"));
        assert_eq!(first_file_content(&states), "alice:x:1000:/bin/zsh\n");
    }

    #[test]
    fn template_unknown_var_errors() {
        // A `${name}` placeholder that isn't in `vars` is a hard
        // failure at apply-eval time. Mutating `render_template`'s
        // `bail!` to `Ok(...)` would silently emit text containing
        // an empty placeholder.
        let proj = TempProject::new("tmpl-unknown-var");
        proj.seed_template("greet.tpl", "hello ${who}");
        let entry = proj.entry(
            "reconcile template(\n\
                 \tpath = \"/x\",\n\
                 \tsource = \"greet.tpl\",\n\
                 \tvars = {},\n\
             )\n",
        );
        let canonical = fs::canonicalize(&entry).unwrap();
        let base_dir = canonical.parent().unwrap().to_path_buf();
        let graph = resolve(vec![EntrySource {
            text: fs::read_to_string(&entry).unwrap(),
            base_dir,
            id: keron_modules::ModuleId::File(canonical),
        }])
        .unwrap_or_else(|errs| panic!("resolve failed: {errs:?}"));
        let err = eval_graph(&graph).expect_err("missing var should fail");
        assert!(
            err.chain().any(|e| e.to_string().contains("`who`")),
            "got: {err:#}",
        );
    }

    #[test]
    fn template_passes_non_ascii_text_through_unchanged() {
        // Non-ASCII bytes (here: an em-dash and a snowman) used to
        // trip a hand-rolled byte-index walker. The char-iteration
        // form should hand them through verbatim.
        let states = run_with_templates(
            "reconcile template(\n\
                 \tpath = \"/x\",\n\
                 \tsource = \"intl.tpl\",\n\
                 \tvars = {\"who\": \"alice\"},\n\
             )\n",
            &[("intl.tpl", "${who} — ☃\n")],
        );
        assert_eq!(first_file_content(&states), "alice — ☃\n");
    }

    #[test]
    fn template_treats_trailing_dollar_as_literal() {
        // A `$` at end-of-input has no `{` follower, so it must pass
        // through literally. (Old byte-walker tripped on the boundary
        // check.)
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
    fn template_unterminated_dollar_brace_errors() {
        // `${` with no closing `}` is a hard error. Without the
        // closed-flag check, render_template would either swallow
        // the rest of the template or panic on missing-key lookup.
        let proj = TempProject::new("tmpl-unterminated");
        proj.seed_template("bad.tpl", "open ${unfinished");
        let entry =
            proj.entry("reconcile template(path = \"/x\", source = \"bad.tpl\", vars = {})\n");
        let canonical = fs::canonicalize(&entry).unwrap();
        let base_dir = canonical.parent().unwrap().to_path_buf();
        let graph = resolve(vec![EntrySource {
            text: fs::read_to_string(&entry).unwrap(),
            base_dir,
            id: keron_modules::ModuleId::File(canonical),
        }])
        .unwrap_or_else(|errs| panic!("resolve failed: {errs:?}"));
        let err = eval_graph(&graph).expect_err("unterminated should fail");
        assert!(
            err.chain().any(|e| e.to_string().contains("unterminated")),
            "got: {err:#}"
        );
    }

    #[test]
    fn render_template_substitutes_known_var() {
        let mut vars = HashMap::new();
        vars.insert("name".into(), "alice".into());
        let out = render_template("hello ${name}!", &vars).unwrap();
        assert_eq!(out, "hello alice!");
    }

    #[test]
    fn render_template_passes_lone_dollar_through() {
        // `$x` (no `{`) is a literal `$x`; no var lookup happens.
        let vars = HashMap::new();
        let out = render_template("$5 and $$", &vars).unwrap();
        assert_eq!(out, "$5 and $$");
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
        let graph = resolve(vec![EntrySource {
            text: fs::read_to_string(&entry).unwrap(),
            base_dir,
            id: keron_modules::ModuleId::File(canonical),
        }])
        .unwrap_or_else(|errs| panic!("resolve failed: {errs:?}"));
        let err = eval_graph(&graph).expect_err("missing source should fail");
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
}
