//! Tree-walking evaluator that turns a type-checked `Program` into an
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
//! The type checker has already proven the program sound, so most
//! "type error" branches here are unreachable in well-typed input but
//! kept as `bail!` rather than `unreachable!` to fail loudly if AST
//! invariants ever drift.

#![allow(clippy::redundant_pub_crate)]

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use anyhow::{Context, Result, anyhow, bail};
use keron_lang::{
    BinOp, Block, CallArg, Expr, FnDecl, ForPattern, Item, Literal, MapEntry, Program, Spanned,
    Stmt, StringPart, UnaryOp,
};

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
    Void,
}

impl Value {
    const fn type_name(&self) -> &'static str {
        match self {
            Self::String(_) => "String",
            Self::Int(_) => "Int",
            Self::Bool(_) => "Boolean",
            Self::Double(_) => "Double",
            Self::List(_) => "List",
            Self::Map(_) => "Map",
            Self::Resource(_) => "Resource",
            Self::Void => "Void",
        }
    }
}

/// Top-level declarations, shared across all eval scopes for one
/// program. Vals are stored as their AST node and evaluated lazily;
/// the cache memoises completed evaluations and `in_progress` catches
/// cycles (which the parser/checker should already reject, but
/// double-checked here so a malformed program can't loop the
/// evaluator).
struct TopLevel<'p> {
    val_decls: HashMap<String, &'p Spanned<Expr>>,
    fns: HashMap<String, &'p FnDecl>,
    cache: RefCell<HashMap<String, Value>>,
    in_progress: RefCell<HashSet<String>>,
}

#[derive(Clone)]
struct Env<'a, 'p> {
    top: &'a TopLevel<'p>,
    /// Lexically-scoped bindings (function params, block-local vals,
    /// loop bindings). Take priority over top-level lookup.
    local: HashMap<String, Value>,
}

impl<'a, 'p> Env<'a, 'p> {
    fn new(top: &'a TopLevel<'p>) -> Self {
        Self {
            top,
            local: HashMap::new(),
        }
    }

    fn extended(&self, name: String, value: Value) -> Self {
        let mut local = self.local.clone();
        local.insert(name, value);
        Self {
            top: self.top,
            local,
        }
    }

    fn lookup(&self, name: &str) -> Result<Value> {
        if let Some(v) = self.local.get(name) {
            return Ok(v.clone());
        }
        if let Some(v) = self.top.cache.borrow().get(name) {
            return Ok(v.clone());
        }
        let expr = *self
            .top
            .val_decls
            .get(name)
            .ok_or_else(|| anyhow!("unknown name `{name}`"))?;
        if !self.top.in_progress.borrow_mut().insert(name.to_string()) {
            bail!("cycle while evaluating `val {name}`");
        }
        let v = eval_expr(expr, self)?;
        self.top.in_progress.borrow_mut().remove(name);
        self.top.cache.borrow_mut().insert(name.to_string(), v.clone());
        Ok(v)
    }
}

pub(crate) fn eval_program(program: &Program) -> Result<Vec<ResourceState>> {
    let mut top = TopLevel {
        val_decls: HashMap::new(),
        fns: HashMap::new(),
        cache: RefCell::new(HashMap::new()),
        in_progress: RefCell::new(HashSet::new()),
    };

    // Hoist every top-level val and fn. Forward references are
    // forbidden by the type checker, so the order we encounter them
    // doesn't matter for correctness — only a `reconcile` (or
    // top-level if/for) actually triggers evaluation.
    for item in &program.items {
        match item {
            Item::Val(v) => {
                top.val_decls.insert(v.name.node.clone(), &v.value);
            }
            Item::Fn(f) => {
                top.fns.insert(f.name.node.clone(), f);
            }
            _ => {}
        }
    }

    let env = Env::new(&top);
    let mut out = Vec::new();
    for item in &program.items {
        match item {
            Item::Val(_) | Item::Fn(_) => {}
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

fn exec_void_block(
    block: &Block,
    env: &Env<'_, '_>,
    out: &mut Vec<ResourceState>,
) -> Result<()> {
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
    }
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
        (Le, a, b) => Ok(Value::Bool(value_cmp(&a, &b)? != std::cmp::Ordering::Greater)),
        (Gt, a, b) => Ok(Value::Bool(value_cmp(&a, &b)? == std::cmp::Ordering::Greater)),
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
        (Value::Double(x), Value::Double(y)) => x
            .partial_cmp(y)
            .ok_or_else(|| anyhow!("NaN comparison")),
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

fn eval_call(name: &str, args: &[CallArg], env: &Env<'_, '_>) -> Result<Value> {
    match name {
        "file" => {
            let path = call_string(args, env, "path", 0)?;
            let content = call_string(args, env, "content", 1)?;
            return Ok(Value::Resource(ResourceState::File {
                path: PathBuf::from(path),
                content,
            }));
        }
        "directory" => {
            let path = call_string(args, env, "path", 0)?;
            return Ok(Value::Resource(ResourceState::Directory {
                path: PathBuf::from(path),
            }));
        }
        "symlink" => {
            let from = call_string(args, env, "from", 0)?;
            let to = call_string(args, env, "to", 1)?;
            return Ok(Value::Resource(ResourceState::Symlink {
                from: PathBuf::from(from),
                to: PathBuf::from(to),
            }));
        }
        _ => {}
    }

    let fn_decl = *env
        .top
        .fns
        .get(name)
        .ok_or_else(|| anyhow!("unknown function `{name}`"))?;

    let bindings = bind_params(fn_decl, args, env)?;
    let mut call_env = Env::new(env.top);
    call_env.local = bindings;

    let mut sink = Vec::new();
    let v = eval_block_value(&fn_decl.body, &call_env, &mut sink)?;
    Ok(v)
}

fn bind_params(
    fn_decl: &FnDecl,
    args: &[CallArg],
    env: &Env<'_, '_>,
) -> Result<HashMap<String, Value>> {
    let mut bound = HashMap::new();
    let mut positional = args.iter().filter(|a| a.name.is_none());
    for param in &fn_decl.params {
        let named = args.iter().find(|a| {
            a.name
                .as_ref()
                .is_some_and(|n| n.node == param.name.node)
        });
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
    let v = eval_expr(&arg.value, env)?;
    match v {
        Value::String(s) => Ok(s),
        other => bail!("expected String for `{name}`, got {}", other.type_name()),
    }
}
