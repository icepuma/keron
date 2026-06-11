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

use crate::plan::{PackageManager, ResourceState, ShellKind};

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
    /// Active call depth. Incremented on every user-fn body entry,
    /// decremented when [`CallDepthGuard`] drops. Caps runaway
    /// recursion at [`MAX_CALL_DEPTH`] so `fn loop(): Int { loop() }`
    /// surfaces as a bailed error instead of blowing the Rust stack.
    call_depth: RefCell<usize>,
    /// Tier-1 probe used by `ensure_session_active` to gate `secret()`
    /// resolution. Borrowed for the lifetime of the eval run so the
    /// plan-time prereq pass and eval-time session checks share one
    /// probe — a test that mocks the plan-time gate also mocks the
    /// secret-resolution gate.
    prereq_probe: &'p dyn crate::capability::PrereqProbe,
    /// Per-eval cache of session-state probes. Scoped to one
    /// `eval_graph_with_prereq_probe` call so a second run (LSP,
    /// daemon, integration test) can't reuse a stale "Active" verdict
    /// from a prior run where the user signed out between invocations.
    /// `RefCell` because eval is single-threaded but threads through
    /// many `&Env` borrows.
    session_cache:
        RefCell<HashMap<crate::capability::SessionKind, crate::capability::SessionState>>,
}

/// Hard cap on synchronous user-fn call depth. Generous enough to
/// admit any sensible hand-written recursion; far below the Rust
/// stack size at the default 8 MiB / 1 MiB-per-frame envelope.
const MAX_CALL_DEPTH: usize = 256;

/// RAII drop-guard that clears `ModuleTop::in_progress` if the
/// caller's `Ok` path didn't already do so. Lives at module scope
/// because Clippy's `items_after_statements` forbids declaring
/// types mid-function. See `Env::lookup` for the user.
struct InProgressGuard<'m, 'p> {
    module: &'m ModuleTop<'p>,
    key: String,
    armed: bool,
}

impl Drop for InProgressGuard<'_, '_> {
    // `#[mutants::skip]` because the cleanup this guard performs is
    // only observable on the *error* path inside `Env::lookup`: the
    // success path explicitly disarms the guard (sets `armed=false`)
    // and clears `in_progress` itself before the drop runs. To catch
    // a "drop is a no-op" mutation we would need (a) a val whose
    // initializer fails mid-evaluation AND (b) a subsequent re-entry
    // into the same `lookup`. Top-level eval bails on the first
    // error, so (b) is unreachable without bespoke harness rewiring.
    // The guard exists as defensive RAII against future panics /
    // early-exits, not for any currently-observable code path.
    #[cfg_attr(test, mutants::skip)]
    fn drop(&mut self) {
        if self.armed {
            self.module.in_progress.borrow_mut().remove(&self.key);
        }
    }
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
            // RAII so a panic / `?` early-exit from `eval_expr` still
            // clears the in-progress marker. The previous straight-line
            // sequence skipped the `remove` on error and a subsequent
            // lookup of the same val (reachable when two reconciles
            // both reference `x`) reported a spurious cycle.
            let mut guard = InProgressGuard {
                module,
                key: key.clone(),
                armed: true,
            };
            // Top-level vals are evaluated lazily on first use, so an
            // error here can surface far from where the val was written
            // (a different machine, a different OS branch). Anchor it to
            // the val name and source file so the user can find it.
            let v = eval_expr(expr, &module_env).with_context(|| {
                format!(
                    "while evaluating `val {name}` in {}",
                    self.current.0.display()
                )
            })?;
            // Success path: hand the cleanup off to the cache insert
            // below so we don't double-borrow.
            guard.armed = false;
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

/// Convenience entry that wires the production [`LiveEnvProbe`] into
/// [`eval_graph_with_prereq_probe`]. Test-only because the crate's
/// production path (`plan::build_prechecked_plan_with_prereq_probe`)
/// goes straight to the `_with_prereq_probe` variant so the same
/// probe gates plan-time validation *and* secret resolution. Kept
/// non-public so the trimmed test surface doesn't grow a parallel
/// public entry point.
#[cfg(test)]
fn eval_graph(graph: &ModuleGraph, keron_root: &Path) -> Result<Vec<ResourceState>> {
    eval_graph_with_prereq_probe(graph, keron_root, &crate::capability::LiveEnvProbe)
}

/// Variant of [`eval_graph`] that takes a caller-supplied
/// [`crate::capability::PrereqProbe`]. Production calls this directly
/// from `plan::build_prechecked_plan_with_prereq_probe`; tests pass a
/// mock so secret-resolution session checks fire against the same
/// fake probe the plan-time prereq pass uses.
pub fn eval_graph_with_prereq_probe(
    graph: &ModuleGraph,
    keron_root: &Path,
    prereq_probe: &dyn crate::capability::PrereqProbe,
) -> Result<Vec<ResourceState>> {
    let mut graph_top = GraphTop {
        modules: HashMap::new(),
        keron_root: keron_root.to_path_buf(),
        call_depth: RefCell::new(0),
        prereq_probe,
        session_cache: RefCell::new(HashMap::new()),
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
        // A `Void` `match` (the OS-dispatch idiom: `match os_type() {
        // "macos" => if … { reconcile … } else {}, … }`) selects an arm
        // and executes its body into the real sink, so resources gated
        // behind a `match` arm are not silently dropped.
        Expr::Match { scrutinee, arms } => exec_void_match(scrutinee, arms, env, out),
        // Any other `Void` trailing expression (e.g. a call to a
        // `Void`-returning fn) is well-typed per the checker; evaluate
        // it for effect and discard the `Void` result. `Void` fn bodies
        // cannot emit resources (reconciles are rejected in value
        // position), so there is nothing to route to `out`.
        _ => {
            eval_expr(expr, env)?;
            Ok(())
        }
    }
}

/// `Void`-context companion to [`eval_match`]: selects the matching arm
/// exactly the same way, then executes the arm body against the real
/// `out` sink instead of synthesising a value.
fn exec_void_match(
    scrutinee: &Spanned<Expr>,
    arms: &[MatchArm],
    env: &Env<'_, '_>,
    out: &mut Vec<ResourceState>,
) -> Result<()> {
    let val = eval_expr(scrutinee, env)?;
    for arm in arms {
        let mut bindings: HashMap<String, Value> = HashMap::new();
        if !try_match_pattern(&arm.pattern.node, &val, &mut bindings) {
            continue;
        }
        let mut arm_env = env.clone();
        for (n, v) in bindings {
            arm_env.local.insert(n, v);
        }
        if let Some(guard) = &arm.guard {
            match eval_expr(guard, &arm_env)? {
                Value::Bool(true) => {}
                Value::Bool(false) => continue,
                other => bail!(
                    "`match` arm guard was {} (expected Boolean)",
                    other.type_name()
                ),
            }
        }
        return exec_void_expr(&arm.body, &arm_env, out);
    }
    bail!("no `match` arm matched value of type {}", val.type_name())
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
    // Grow the stack per recursion level: a left-deep AST from a long
    // flat operator chain (`1 + 1 + 1 + …`) recurses through
    // `eval_expr` and would otherwise overflow the stack and SIGABRT
    // on a manifest that passed the checker. Cheap when the stack is
    // healthy. (`eval_call` body recursion is already guarded.)
    stacker::maybe_grow(STACK_RED_ZONE, STACK_GROW_SLAB, || {
        eval_expr_inner(expr, env)
    })
}

fn eval_expr_inner(expr: &Spanned<Expr>, env: &Env<'_, '_>) -> Result<Value> {
    match &expr.node {
        Expr::Literal(lit) => Ok(eval_literal(lit)),
        Expr::Unary { op, operand } => eval_unary(*op, eval_expr(operand, env)?),
        Expr::Binary { op, lhs, rhs } => {
            // Short-circuit operators: skip RHS evaluation when the LHS
            // already decides the result. Important when the RHS has
            // observable cost — `secret(...)` shells out — or when the
            // RHS would itself error (e.g. a function call that's only
            // safe to attempt under the LHS guard).
            if let Some(v) = eval_short_circuit(*op, lhs, rhs, env)? {
                return Ok(v);
            }
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
            let mut pairs: Vec<(Value, Value)> = Vec::with_capacity(entries.len());
            for MapEntry { key, value, .. } in entries {
                let k = eval_expr(key, env)?;
                let v = eval_expr(value, env)?;
                // Static duplicate keys are a checker error, but dynamic
                // keys can still collide (`{ env("A"): 1, env("B"): 2 }`
                // with both resolving to the same string). Dedupe
                // last-wins-by-value at the first position — matching
                // `merge`/`with` — so `get`, `keys`, `values`, and
                // template-var lookup all agree on the winner instead of
                // disagreeing (get first-wins vs vars last-wins).
                if let Some(slot) = pairs.iter_mut().find(|(ek, _)| value_eq(ek, &k)) {
                    slot.1 = v;
                } else {
                    pairs.push((k, v));
                }
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
        if !try_match_pattern(&arm.pattern.node, &val, &mut bindings) {
            continue;
        }
        let mut arm_env = env.clone();
        for (n, v) in bindings {
            arm_env.local.insert(n, v);
        }
        // Guards run with pattern bindings in scope; a false guard
        // falls through to the next arm (the pattern's bindings are
        // discarded with `arm_env` on the next iteration).
        if let Some(guard) = &arm.guard {
            match eval_expr(guard, &arm_env)? {
                Value::Bool(true) => {}
                Value::Bool(false) => continue,
                other => bail!(
                    "`match` arm guard was {} (expected Boolean)",
                    other.type_name()
                ),
            }
        }
        return eval_expr(&arm.body, &arm_env);
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

/// Handle binary operators whose RHS must not be evaluated when the
/// LHS already pins the result. Returns `Ok(Some(v))` when the
/// operator was a short-circuit form and the result was determined,
/// `Ok(None)` for any other operator (caller does eager evaluation).
///
/// The type checker has already guaranteed that `&&` / `||` operands
/// are `Boolean` and that the LHS of `??` is nullable, so the runtime
/// `Value` shape is trustworthy; bail messages are defensive cover
/// for evaluator bugs, not user-facing errors.
fn eval_short_circuit(
    op: BinOp,
    lhs: &Spanned<Expr>,
    rhs: &Spanned<Expr>,
    env: &Env<'_, '_>,
) -> Result<Option<Value>> {
    match op {
        BinOp::Coalesce => {
            let l = eval_expr(lhs, env)?;
            if matches!(l, Value::Null) {
                Ok(Some(eval_expr(rhs, env)?))
            } else {
                Ok(Some(l))
            }
        }
        BinOp::And | BinOp::Or => {
            let l = eval_expr(lhs, env)?;
            let Value::Bool(b) = l else {
                bail!(
                    "`{}` LHS was {} (expected Boolean)",
                    op.symbol(),
                    l.type_name()
                );
            };
            // `&&` short-circuits on `false`, `||` short-circuits on `true`.
            let short = matches!(op, BinOp::Or);
            if b == short {
                return Ok(Some(Value::Bool(b)));
            }
            let r = eval_expr(rhs, env)?;
            let Value::Bool(rb) = r else {
                bail!(
                    "`{}` RHS was {} (expected Boolean)",
                    op.symbol(),
                    r.type_name()
                );
            };
            Ok(Some(Value::Bool(rb)))
        }
        _ => Ok(None),
    }
}

fn eval_unary(op: UnaryOp, v: Value) -> Result<Value> {
    match (op, v) {
        (UnaryOp::Neg, Value::Int(n)) => n
            .checked_neg()
            .map(Value::Int)
            .ok_or_else(|| anyhow!("integer overflow in `-{n}` (negating i64::MIN)")),
        (UnaryOp::Neg, Value::Double(d)) => Ok(Value::Double(-d)),
        (UnaryOp::Not, Value::Bool(b)) => Ok(Value::Bool(!b)),
        (op, v) => bail!("unary `{}` on {}", op.symbol(), v.type_name()),
    }
}

fn eval_binop(op: BinOp, l: Value, r: Value) -> Result<Value> {
    let result = eval_binop_inner(op, l, r)?;
    // Uphold the finite-`Double` invariant the rest of the language
    // relies on (the literal parser rejects `inf`/`NaN` for the same
    // reason). Division by zero, overflow to `±inf`, and `NaN`-producing
    // powers (`(-1.0) ** 0.5`) are caught here so they fail with a clear
    // message instead of silently poisoning later comparisons with the
    // span-less `NaN comparison` error.
    if let Value::Double(d) = &result
        && !d.is_finite()
    {
        bail!(
            "`{}` produced a non-finite Double ({d}); Double values must stay finite (division by zero, overflow, or a NaN-producing power)",
            op.symbol()
        );
    }
    Ok(result)
}

#[allow(clippy::cast_precision_loss, clippy::too_many_lines)]
fn eval_binop_inner(op: BinOp, l: Value, r: Value) -> Result<Value> {
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
        (Add, Value::Int(a), Value::Int(b)) => a
            .checked_add(b)
            .map(Value::Int)
            .ok_or_else(|| anyhow!("integer overflow in `{a} + {b}`")),
        (Sub, Value::Int(a), Value::Int(b)) => a
            .checked_sub(b)
            .map(Value::Int)
            .ok_or_else(|| anyhow!("integer overflow in `{a} - {b}`")),
        (Mul, Value::Int(a), Value::Int(b)) => a
            .checked_mul(b)
            .map(Value::Int)
            .ok_or_else(|| anyhow!("integer overflow in `{a} * {b}`")),
        (Div, Value::Int(a), Value::Int(b)) => {
            if b == 0 {
                bail!("division by zero");
            }
            a.checked_div(b)
                .map(Value::Int)
                .ok_or_else(|| anyhow!("integer overflow in `{a} / {b}` (i64::MIN / -1)"))
        }
        (Pow, Value::Int(a), Value::Int(b)) => {
            if b < 0 {
                bail!(
                    "`{a} ** {b}` has a negative exponent; `**` on `Int` requires a non-negative exponent — use `Double` operands for negative powers"
                );
            }
            let exp = u32::try_from(b).with_context(|| {
                format!("`{a} ** {b}` exponent {b} is too large (max {})", u32::MAX)
            })?;
            a.checked_pow(exp)
                .map(Value::Int)
                .ok_or_else(|| anyhow!("integer overflow in `{a} ** {b}`"))
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
        (Pow, Value::Double(a), Value::Int(b)) => {
            let exp = i32::try_from(b)
                .with_context(|| format!("`{a} ** {b}` exponent does not fit in i32"))?;
            Ok(Value::Double(a.powi(exp)))
        }

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

/// Exact ordering of an `i64` against an `f64`, with no `i64 → f64`
/// promotion (which loses precision past 2^53, making e.g.
/// `9007199254740993 == 9007199254740992.0` wrongly true). Returns
/// `None` only for `NaN` (which the finite-`Double` invariant keeps out
/// of arithmetic, but equality on a literal `NaN` would still reach
/// here). The `Double` is compared by its integer and fractional parts
/// separately so both sides stay exact.
//
// `i64::MIN as f64` is exact (-2^63); `y.trunc() as i64` only runs after
// the range checks prove `y ∈ [-2^63, 2^63)`, so neither cast loses
// information despite the lints' general warnings.
#[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
fn cmp_int_double(x: i64, y: f64) -> Option<std::cmp::Ordering> {
    use std::cmp::Ordering;
    if y.is_nan() {
        return None;
    }
    // `i64::MIN` is exactly representable as f64 (-2^63); `i64::MAX` is
    // not, so the upper bound is the first f64 strictly above every
    // i64, namely 2^63.
    if y < i64::MIN as f64 {
        return Some(Ordering::Greater); // every i64 > y
    }
    if y >= -(i64::MIN as f64) {
        return Some(Ordering::Less); // y >= 2^63 > every i64
    }
    // y is now in [-2^63, 2^63): its truncation fits an i64 exactly.
    let y_trunc = y.trunc() as i64;
    match x.cmp(&y_trunc) {
        // Integer parts equal — the fraction of `y` breaks the tie
        // (`x` has none). 0 vs y.fract(): >0 ⇒ x < y, <0 ⇒ x > y.
        Ordering::Equal => 0.0_f64.partial_cmp(&y.fract()),
        ord => Some(ord),
    }
}

fn value_eq(a: &Value, b: &Value) -> bool {
    use std::cmp::Ordering;
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
        (Value::Int(x), Value::Double(y)) => cmp_int_double(*x, *y) == Some(Ordering::Equal),
        (Value::Double(x), Value::Int(y)) => cmp_int_double(*y, *x) == Some(Ordering::Equal),
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

fn value_cmp(a: &Value, b: &Value) -> Result<std::cmp::Ordering> {
    match (a, b) {
        (Value::Int(x), Value::Int(y)) => Ok(x.cmp(y)),
        (Value::Double(x), Value::Double(y)) => {
            x.partial_cmp(y).ok_or_else(|| anyhow!("NaN comparison"))
        }
        // Compared exactly via `cmp_int_double` (no `i64 → f64`
        // promotion) so ordering stays correct past 2^53.
        (Value::Int(x), Value::Double(y)) => {
            cmp_int_double(*x, *y).ok_or_else(|| anyhow!("NaN comparison"))
        }
        (Value::Double(x), Value::Int(y)) => cmp_int_double(*y, *x)
            .map(std::cmp::Ordering::reverse)
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
            StringPart::Expr { expr, indent } => {
                let v = eval_expr(expr, env)?;
                sensitive |= stringify_with_indent(&v, indent.as_deref(), &mut out)?;
            }
        }
    }
    Ok(Value::String {
        text: out,
        sensitive,
    })
}

fn stringify_with_indent(v: &Value, indent: Option<&str>, out: &mut String) -> Result<bool> {
    let mut text = String::new();
    let sensitive = stringify(v, &mut text)?;
    append_with_indent(out, &text, indent.unwrap_or(""));
    Ok(sensitive)
}

fn append_with_indent(out: &mut String, text: &str, indent: &str) {
    if !text.contains('\n') {
        out.push_str(text);
        return;
    }
    let mut lines = text.split('\n');
    if let Some(first) = lines.next() {
        out.push_str(first);
    }
    for line in lines {
        out.push('\n');
        if !indent.is_empty() && !line.is_empty() {
            out.push_str(indent);
        }
        out.push_str(line);
    }
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
        let default_env = Env::new(env.graph, origin.clone());
        return construct_struct(decl, args, env, &default_env);
    }
    if let Some(decl) = module.structs.get(name) {
        // Field defaults must see module scope only — never the
        // caller's locals (params/block vals/match binds), which the
        // checker forbids them from referencing. Passing `env` as the
        // default env would dynamically scope a default that names a
        // module `val` to a same-named caller local, breaking type
        // soundness when their types differ. Build a fresh module-scope
        // env, exactly as the imported-struct path above does.
        let default_env = Env::new(env.graph, env.current.clone());
        return construct_struct(decl, args, env, &default_env);
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

    let _depth = CallDepthGuard::enter(env.graph)?;
    let mut sink = Vec::new();
    // `stacker::maybe_grow` keeps the recursion-limit diagnostic
    // independent of the host thread's stack size. Without it, a
    // 2 MiB cargo-test thread stack runs out of pages well before
    // `MAX_CALL_DEPTH = 256` frames, surfacing as SIGABRT instead of
    // the clean `bail!("call depth exceeded …")` the user expects.
    // 64 KiB red zone / 1 MiB grow slab match the standard rustc /
    // syn idiom; if the red zone is still available we stay on the
    // current stack at zero cost.
    let v = stacker::maybe_grow(STACK_RED_ZONE, STACK_GROW_SLAB, || {
        eval_block_value(&fn_decl.body, &call_env, &mut sink)
    })?;
    Ok(v)
}

/// Red zone for `stacker::maybe_grow` in `eval_call`. Tuned by hand to
/// the canonical rustc / syn idiom (64 KiB); the cargo-mutants `*`-to-
/// `+`/`/` mutations on the inline `64 * 1024` literal mutate the
/// constant value but not any observable runtime behavior under the
/// existing recursion test (the depth guard at `MAX_CALL_DEPTH = 256`
/// bails before the smaller red zone would matter).
#[cfg_attr(test, mutants::skip)]
const STACK_RED_ZONE: usize = 64 * 1024;

/// Slab size for `stacker::maybe_grow` in `eval_call`. Tuned by hand
/// to the canonical rustc / syn idiom (1 MiB). Same equivalence
/// caveat as [`STACK_RED_ZONE`].
#[cfg_attr(test, mutants::skip)]
const STACK_GROW_SLAB: usize = 1024 * 1024;

/// RAII guard around `GraphTop::call_depth`. Increments on
/// construction (bailing if [`MAX_CALL_DEPTH`] would be exceeded),
/// decrements on drop. Using RAII so a `?` early-exit or panic from
/// `eval_block_value` still restores the counter, keeping subsequent
/// recursion budgets accurate.
struct CallDepthGuard<'g, 'p> {
    graph: &'g GraphTop<'p>,
}

impl<'g, 'p> CallDepthGuard<'g, 'p> {
    fn enter(graph: &'g GraphTop<'p>) -> Result<Self> {
        let mut depth = graph.call_depth.borrow_mut();
        if *depth >= MAX_CALL_DEPTH {
            bail!(
                "user function call depth exceeded {MAX_CALL_DEPTH} — likely unbounded recursion"
            );
        }
        *depth += 1;
        Ok(Self { graph })
    }
}

impl Drop for CallDepthGuard<'_, '_> {
    fn drop(&mut self) {
        *self.graph.call_depth.borrow_mut() -= 1;
    }
}

/// Construct a struct value: bind each declared field by name (named
/// arg) or by position (positional arg), then assemble a
/// [`Value::Struct`]. Argument resolution mirrors [`bind_params`] —
/// the type checker has already validated counts and types so a hit
/// here is well-typed by construction.
///
/// Explicit arguments are evaluated in the caller's env. Defaults are
/// evaluated in the declaring module's env, matching the checker's
/// `check_struct_decl` scope and keeping imported constructors from
/// accidentally depending on importer-local names.
fn construct_struct(
    decl: &StructDecl,
    args: &[CallArg],
    arg_env: &Env<'_, '_>,
    default_env: &Env<'_, '_>,
) -> Result<Value> {
    let mut fields: Vec<(String, Value)> = Vec::with_capacity(decl.fields.len());
    let mut positional = args.iter().filter(|a| a.name.is_none());
    for field in &decl.fields {
        let named = args
            .iter()
            .find(|a| a.name.as_ref().is_some_and(|n| n.node == field.name.node));
        let value = if let Some(arg) = named {
            eval_expr(&arg.value, arg_env)?
        } else if let Some(arg) = positional.next() {
            eval_expr(&arg.value, arg_env)?
        } else if let Some(default) = &field.default {
            eval_expr(default, default_env)?
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
            let source = call_string(args, env, "source", 0)?;
            let target = call_string(args, env, "target", 1)?;
            let resolved_source = resolve_managed_path(&source, env, "symlink", "source")?;
            Ok(Value::Resource(ResourceState::Symlink {
                from: PathBuf::from(target),
                to: resolved_source,
            }))
        }
        IntrinsicId::Shell => dispatch_shell(args, env),
        IntrinsicId::Template => dispatch_template(args, env),
        IntrinsicId::KeronRoot => Ok(Value::plain_string(
            env.graph.keron_root.to_string_lossy().into_owned(),
        )),
        IntrinsicId::OsType => Ok(Value::plain_string(detect_os_type())),
        IntrinsicId::OsArch => Ok(Value::plain_string(detect_os_arch())),
        IntrinsicId::Env => {
            let name = call_string(args, env, "name", 0)?;
            Ok(std::env::var(&name).map_or(Value::Null, Value::plain_string))
        }
        IntrinsicId::Secret => {
            let uri = call_string(args, env, "uri", 0)?;
            let value =
                resolve_secret(&uri, env).with_context(|| format!("resolving secret `{uri}`"))?;
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
        IntrinsicId::Cask => dispatch_package(args, env, PackageManager::BrewCask),
        IntrinsicId::Cargo => dispatch_package(args, env, PackageManager::Cargo),
        IntrinsicId::Winget => dispatch_package(args, env, PackageManager::Winget),
        IntrinsicId::Hostname => dispatch_hostname(),
        IntrinsicId::User => dispatch_user(),
        IntrinsicId::HomeDir => dispatch_required_dir("home_dir", dirs::home_dir),
        IntrinsicId::ConfigDir => dispatch_required_dir("config_dir", dirs::config_dir),
        IntrinsicId::CacheDir => dispatch_required_dir("cache_dir", dirs::cache_dir),
        IntrinsicId::DataDir => dispatch_required_dir("data_dir", dirs::data_dir),
        IntrinsicId::StateDir => Ok(dispatch_optional_dir(dirs::state_dir)),
        IntrinsicId::RuntimeDir => Ok(dispatch_optional_dir(dirs::runtime_dir)),
        IntrinsicId::Split => dispatch_split(args, env),
        IntrinsicId::Join => dispatch_join(args, env),
        IntrinsicId::Contains => dispatch_contains(args, env),
        IntrinsicId::Replace => dispatch_replace(args, env),
        IntrinsicId::Trim => dispatch_trim(args, env),
        IntrinsicId::StartsWith => dispatch_starts_with(args, env),
        IntrinsicId::EndsWith => dispatch_ends_with(args, env),
        IntrinsicId::StrLen => dispatch_str_len(args, env),
        IntrinsicId::ListLen => dispatch_list_len(args, env),
        IntrinsicId::ListContains => dispatch_list_contains(args, env),
        IntrinsicId::ListFirst => dispatch_list_endpoint(args, env, ListEndpoint::First),
        IntrinsicId::ListLast => dispatch_list_endpoint(args, env, ListEndpoint::Last),
        IntrinsicId::Sort => dispatch_sort(args, env),
        IntrinsicId::Unique => dispatch_unique(args, env),
        IntrinsicId::IndexOf => dispatch_index_of(args, env),
        IntrinsicId::MapKeys => dispatch_map_projection(args, env, MapProjection::Keys),
        IntrinsicId::MapValues => dispatch_map_projection(args, env, MapProjection::Values),
        IntrinsicId::MapGet => dispatch_map_get(args, env),
        IntrinsicId::MapContains => dispatch_map_contains(args, env),
        IntrinsicId::MapMerge => dispatch_map_merge(args, env),
        IntrinsicId::MapWithout => dispatch_map_without(args, env),
        IntrinsicId::MapWith => dispatch_map_with(args, env),
        IntrinsicId::ParseInt => dispatch_parse_int(args, env),
        IntrinsicId::ParseDouble => dispatch_parse_double(args, env),
        IntrinsicId::PathJoin => dispatch_path_join(args, env),
        IntrinsicId::PathParent => dispatch_path_parent(args, env),
        IntrinsicId::PathBasename => dispatch_path_basename(args, env),
        IntrinsicId::PathExtension => dispatch_path_extension(args, env),
        IntrinsicId::PathIsAbsolute => dispatch_path_is_absolute(args, env),
        IntrinsicId::PathExists => dispatch_path_probe(args, env, PathProbe::Exists),
        IntrinsicId::PathIsDir => dispatch_path_probe(args, env, PathProbe::IsDir),
        IntrinsicId::PathIsFile => dispatch_path_probe(args, env, PathProbe::IsFile),
        IntrinsicId::ReadFile => dispatch_read_file(args, env),
        IntrinsicId::SshKey => dispatch_ssh_key(args, env),
        IntrinsicId::GpgKey => dispatch_gpg_key(args, env),
    }
}

/// Construct an `SshKey` resource from user-supplied material.
///
/// The `private` arg is `Type::Secret` in the stdlib signature, so the
/// type checker guarantees this dispatch sees a `Value::Secret`; we
/// bail loudly on any other shape rather than panic. The secret's
/// payload is moved into [`ResourceState::SshKey::private_key`] as a
/// plain `String` for the executor to write — the marker is enforced
/// at the type-system layer, not by carrying the wrapper through the
/// IR. The resource is treated as always-sensitive by the diff
/// renderer (no opt-out flag).
fn dispatch_ssh_key(args: &[CallArg], env: &Env<'_, '_>) -> Result<Value> {
    let private_path = call_string(args, env, "private_path", 0)?;
    let public_path = call_string(args, env, "public_path", 1)?;
    let private_key = match eval_call_arg(args, env, "private", 2)? {
        Value::Secret(s) => s,
        other => bail!(
            "ssh_key expected `Secret` for `private`, found `{}`",
            other.type_name()
        ),
    };
    let public_key = call_string(args, env, "public", 3)?;
    Ok(Value::Resource(ResourceState::SshKey {
        private_path: PathBuf::from(private_path),
        public_path: PathBuf::from(public_path),
        private_key,
        public_key,
    }))
}

/// Construct a `GpgKey` resource. The `key` arg is `Type::Secret`; we
/// pattern-match on `Value::Secret` for the same reasons documented on
/// [`dispatch_ssh_key`]. The fingerprint is plain `String` — it's
/// already on disk in the user's keyring or surfacing in any `gpg
/// --list-secret-keys` output, so its non-sensitivity is fine.
fn dispatch_gpg_key(args: &[CallArg], env: &Env<'_, '_>) -> Result<Value> {
    let fingerprint = call_string(args, env, "fingerprint", 0)?;
    let key = match eval_call_arg(args, env, "key", 1)? {
        Value::Secret(s) => s,
        other => bail!(
            "gpg_key expected `Secret` for `key`, found `{}`",
            other.type_name()
        ),
    };
    Ok(Value::Resource(ResourceState::GpgKey { fingerprint, key }))
}

/// `read_file(path)` — keron-root-confined UTF-8 read.
///
/// The path must resolve (via the same `resolve_managed_path` that
/// guards `symlink(source = …)` and `template(source = …)`) to a real
/// file inside the keron root. Containment failure, IO error, and
/// invalid-UTF-8 all collapse to `Value::Null` so a `?? "fallback"`
/// site can recover uniformly. This is **load-bearing security**:
/// the type is `String?`, every error path returns `null`, and the
/// resolver — not the dispatch — owns the containment decision.
fn dispatch_read_file(args: &[CallArg], env: &Env<'_, '_>) -> Result<Value> {
    let raw = call_string(args, env, "path", 0)?;
    let Ok(resolved) = resolve_managed_path(&raw, env, "read_file", "path") else {
        return Ok(Value::Null);
    };
    let Ok(bytes) = std::fs::read(&resolved) else {
        return Ok(Value::Null);
    };
    let Ok(text) = String::from_utf8(bytes) else {
        return Ok(Value::Null);
    };
    Ok(Value::plain_string(text))
}

#[derive(Clone, Copy)]
enum PathProbe {
    Exists,
    IsDir,
    IsFile,
}

fn dispatch_path_join(args: &[CallArg], env: &Env<'_, '_>) -> Result<Value> {
    let p = call_string(args, env, "p", 0)?;
    let segment = call_string(args, env, "segment", 1)?;
    let joined = std::path::PathBuf::from(p).join(segment);
    Ok(Value::plain_string(joined.to_string_lossy().into_owned()))
}

fn dispatch_path_parent(args: &[CallArg], env: &Env<'_, '_>) -> Result<Value> {
    let p = call_string(args, env, "p", 0)?;
    Ok(std::path::Path::new(&p)
        .parent()
        // `Path::parent` returns `Some("")` for "foo" (relative, no
        // separator). That's almost never what a dotfile manifest
        // wants — collapse it to `null` so users get a clean
        // signal of "no parent here".
        .filter(|parent| !parent.as_os_str().is_empty())
        .map_or(Value::Null, |parent| {
            Value::plain_string(parent.to_string_lossy().into_owned())
        }))
}

fn dispatch_path_basename(args: &[CallArg], env: &Env<'_, '_>) -> Result<Value> {
    let p = call_string(args, env, "p", 0)?;
    let name = std::path::Path::new(&p)
        .file_name()
        .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
    Ok(Value::plain_string(name))
}

fn dispatch_path_extension(args: &[CallArg], env: &Env<'_, '_>) -> Result<Value> {
    let p = call_string(args, env, "p", 0)?;
    let ext = std::path::Path::new(&p)
        .extension()
        .map_or_else(String::new, |e| e.to_string_lossy().into_owned());
    Ok(Value::plain_string(ext))
}

fn dispatch_path_is_absolute(args: &[CallArg], env: &Env<'_, '_>) -> Result<Value> {
    let p = call_string(args, env, "p", 0)?;
    Ok(Value::Bool(std::path::Path::new(&p).is_absolute()))
}

/// `path_exists` / `path_is_dir` / `path_is_file` intentionally probe
/// the live host filesystem. Relative paths are resolved against the
/// current module's directory. Missing paths, permission errors, and
/// other metadata failures collapse to `false`.
fn dispatch_path_probe(args: &[CallArg], env: &Env<'_, '_>, kind: PathProbe) -> Result<Value> {
    let p = call_string(args, env, "p", 0)?;
    let meta = std::fs::metadata(observation_path(&p, env));
    let answer = match (kind, meta) {
        (PathProbe::Exists, Ok(_)) => true,
        (PathProbe::IsDir, Ok(m)) => m.is_dir(),
        (PathProbe::IsFile, Ok(m)) => m.is_file(),
        (_, Err(_)) => false,
    };
    Ok(Value::Bool(answer))
}

fn observation_path(raw: &str, env: &Env<'_, '_>) -> PathBuf {
    let candidate = PathBuf::from(raw);
    if candidate.is_absolute() {
        return candidate;
    }
    let ModuleId(module_path) = &env.current;
    module_path.parent().map_or(candidate, |p| p.join(raw))
}

#[derive(Clone, Copy)]
enum ListEndpoint {
    First,
    Last,
}

#[derive(Clone, Copy)]
enum MapProjection {
    Keys,
    Values,
}

fn dispatch_list_len(args: &[CallArg], env: &Env<'_, '_>) -> Result<Value> {
    let xs = eval_call_arg(args, env, "xs", 0)?;
    let Value::List(items) = xs else {
        bail!("len(xs): `xs` was {} (expected List)", xs.type_name());
    };
    let n: i64 = items
        .len()
        .try_into()
        .map_err(|_| anyhow!("len(xs): list size exceeds Int range"))?;
    Ok(Value::Int(n))
}

fn dispatch_list_contains(args: &[CallArg], env: &Env<'_, '_>) -> Result<Value> {
    let xs = eval_call_arg(args, env, "xs", 0)?;
    let needle = eval_call_arg(args, env, "x", 1)?;
    let Value::List(items) = xs else {
        bail!(
            "contains(xs, x): `xs` was {} (expected List)",
            xs.type_name()
        );
    };
    Ok(Value::Bool(
        items.iter().any(|item| value_eq(item, &needle)),
    ))
}

/// `first(xs)` / `last(xs)` share an inspection shape — pull the head
/// or tail of a `Value::List`, returning `Value::Null` for an empty
/// list (matching the `T?` signature). The element is cloned because
/// the list itself is consumed by the dispatch.
fn dispatch_list_endpoint(args: &[CallArg], env: &Env<'_, '_>, end: ListEndpoint) -> Result<Value> {
    let xs = eval_call_arg(args, env, "xs", 0)?;
    let Value::List(items) = xs else {
        bail!(
            "{}(xs): `xs` was {} (expected List)",
            match end {
                ListEndpoint::First => "first",
                ListEndpoint::Last => "last",
            },
            xs.type_name()
        );
    };
    Ok(match end {
        ListEndpoint::First => items.into_iter().next().unwrap_or(Value::Null),
        ListEndpoint::Last => items.into_iter().next_back().unwrap_or(Value::Null),
    })
}

fn dispatch_map_projection(
    args: &[CallArg],
    env: &Env<'_, '_>,
    proj: MapProjection,
) -> Result<Value> {
    let m = eval_call_arg(args, env, "m", 0)?;
    let Value::Map(pairs) = m else {
        bail!(
            "{}(m): `m` was {} (expected Map)",
            match proj {
                MapProjection::Keys => "keys",
                MapProjection::Values => "values",
            },
            m.type_name()
        );
    };
    let out: Vec<Value> = pairs
        .into_iter()
        .map(|(k, v)| match proj {
            MapProjection::Keys => k,
            MapProjection::Values => v,
        })
        .collect();
    Ok(Value::List(out))
}

fn dispatch_map_get(args: &[CallArg], env: &Env<'_, '_>) -> Result<Value> {
    let m = eval_call_arg(args, env, "m", 0)?;
    let key = eval_call_arg(args, env, "k", 1)?;
    let default = eval_call_arg(args, env, "default", 2)?;
    let Value::Map(pairs) = m else {
        bail!(
            "get(m, k, default): `m` was {} (expected Map)",
            m.type_name()
        );
    };
    Ok(pairs
        .into_iter()
        .find_map(|(k, v)| value_eq(&k, &key).then_some(v))
        .unwrap_or(default))
}

fn dispatch_map_contains(args: &[CallArg], env: &Env<'_, '_>) -> Result<Value> {
    let m = eval_call_arg(args, env, "m", 0)?;
    let key = eval_call_arg(args, env, "k", 1)?;
    let Value::Map(pairs) = m else {
        bail!(
            "map_contains(m, k): `m` was {} (expected Map)",
            m.type_name()
        );
    };
    Ok(Value::Bool(pairs.iter().any(|(k, _)| value_eq(k, &key))))
}

/// `sort(xs)` — ascending lex order on `String`. The signature
/// constrains `xs` to `List<String>`; any other element type is a
/// type error before dispatch, so destructuring failures here mean
/// AST drift (loud `bail!`, not a silent miss).
fn dispatch_sort(args: &[CallArg], env: &Env<'_, '_>) -> Result<Value> {
    let xs = eval_call_arg(args, env, "xs", 0)?;
    let Value::List(mut items) = xs else {
        bail!("sort(xs): `xs` was {} (expected List)", xs.type_name());
    };
    items.sort_by(|a, b| match (a, b) {
        (Value::String { text: x, .. }, Value::String { text: y, .. }) => x.cmp(y),
        _ => std::cmp::Ordering::Equal,
    });
    Ok(Value::List(items))
}

/// `unique(xs)` — keep first occurrence, drop later duplicates.
/// O(n²) on equality probes; lists in dotfile manifests are small
/// enough that the obvious algorithm wins over any hashing scheme
/// that'd need a `Value`-keyed equivalence.
fn dispatch_unique(args: &[CallArg], env: &Env<'_, '_>) -> Result<Value> {
    let xs = eval_call_arg(args, env, "xs", 0)?;
    let Value::List(items) = xs else {
        bail!("unique(xs): `xs` was {} (expected List)", xs.type_name());
    };
    let mut out: Vec<Value> = Vec::with_capacity(items.len());
    for item in items {
        if !out.iter().any(|seen| value_eq(seen, &item)) {
            out.push(item);
        }
    }
    Ok(Value::List(out))
}

/// `index_of(xs, x)` — position of the first equal element, or
/// `null`. Saturates to `Value::Null` rather than a sentinel `-1`
/// so `??` is the natural recovery path (the whole reason we shaped
/// the signature as `Int?`).
fn dispatch_index_of(args: &[CallArg], env: &Env<'_, '_>) -> Result<Value> {
    let xs = eval_call_arg(args, env, "xs", 0)?;
    let needle = eval_call_arg(args, env, "x", 1)?;
    let Value::List(items) = xs else {
        bail!(
            "index_of(xs, x): `xs` was {} (expected List)",
            xs.type_name()
        );
    };
    let Some(idx) = items.iter().position(|item| value_eq(item, &needle)) else {
        return Ok(Value::Null);
    };
    let n: i64 = idx
        .try_into()
        .map_err(|_| anyhow!("index_of(xs, x): index exceeds Int range"))?;
    Ok(Value::Int(n))
}

/// `merge(a, b)` — last-wins overlay. Preserves `a`'s declaration
/// order for keys that exist in both, then appends `b`'s new keys
/// in their original order. Matches what users expect from a
/// "base config + per-host override" composition.
fn dispatch_map_merge(args: &[CallArg], env: &Env<'_, '_>) -> Result<Value> {
    let a = eval_call_arg(args, env, "a", 0)?;
    let b = eval_call_arg(args, env, "b", 1)?;
    let Value::Map(left) = a else {
        bail!("merge(a, b): `a` was {} (expected Map)", a.type_name());
    };
    let Value::Map(right) = b else {
        bail!("merge(a, b): `b` was {} (expected Map)", b.type_name());
    };
    let mut out: Vec<(Value, Value)> = Vec::with_capacity(left.len() + right.len());
    for (k, v) in left {
        let override_v = right.iter().find_map(|(rk, rv)| {
            if value_eq(rk, &k) {
                Some(rv.clone())
            } else {
                None
            }
        });
        out.push((k, override_v.unwrap_or(v)));
    }
    for (k, v) in right {
        if !out.iter().any(|(ok, _)| value_eq(ok, &k)) {
            out.push((k, v));
        }
    }
    Ok(Value::Map(out))
}

/// `without(m, k)` — drop the binding for `k`. Stable for all other
/// keys; a no-op when `k` is absent (no ambiguity to surface).
fn dispatch_map_without(args: &[CallArg], env: &Env<'_, '_>) -> Result<Value> {
    let m = eval_call_arg(args, env, "m", 0)?;
    let key = eval_call_arg(args, env, "k", 1)?;
    let Value::Map(pairs) = m else {
        bail!("without(m, k): `m` was {} (expected Map)", m.type_name());
    };
    let out: Vec<(Value, Value)> = pairs
        .into_iter()
        .filter(|(k, _)| !value_eq(k, &key))
        .collect();
    Ok(Value::Map(out))
}

/// `with(m, k, v)` — upsert. Preserves `k`'s existing position when
/// already bound (so updates don't reorder a map the caller built
/// in a meaningful order); appends when the key is new.
fn dispatch_map_with(args: &[CallArg], env: &Env<'_, '_>) -> Result<Value> {
    let m = eval_call_arg(args, env, "m", 0)?;
    let key = eval_call_arg(args, env, "k", 1)?;
    let value = eval_call_arg(args, env, "v", 2)?;
    let Value::Map(pairs) = m else {
        bail!("with(m, k, v): `m` was {} (expected Map)", m.type_name());
    };
    let mut out: Vec<(Value, Value)> = Vec::with_capacity(pairs.len() + 1);
    let mut replaced = false;
    for (existing_k, existing_v) in pairs {
        if !replaced && value_eq(&existing_k, &key) {
            out.push((existing_k, value.clone()));
            replaced = true;
        } else {
            out.push((existing_k, existing_v));
        }
    }
    if !replaced {
        out.push((key, value));
    }
    Ok(Value::Map(out))
}

/// `parse_int(s)` — strict signed-integer parse. Rust's
/// `i64::from_str` already rejects leading whitespace, trailing
/// junk, and hex prefixes; we mirror that contract directly.
fn dispatch_parse_int(args: &[CallArg], env: &Env<'_, '_>) -> Result<Value> {
    let s = call_string(args, env, "s", 0)?;
    Ok(s.parse::<i64>().map_or(Value::Null, Value::Int))
}

/// `parse_double(s)` — strict IEEE-754 parse. Rust's `f64::from_str`
/// accepts `"inf"` / `"NaN"`, but the rest of the language assumes
/// `Double` values are finite — so we collapse non-finite parses to
/// `null` as if they were malformed.
fn dispatch_parse_double(args: &[CallArg], env: &Env<'_, '_>) -> Result<Value> {
    let s = call_string(args, env, "s", 0)?;
    let parsed = s.parse::<f64>().ok().filter(|n| n.is_finite());
    Ok(parsed.map_or(Value::Null, Value::Double))
}

/// `hostname()` — read via `gethostname(2)` on Unix and the
/// `$COMPUTERNAME` env on Windows. The Unix path goes through `libc`
/// directly to avoid pulling in a separate crate; the Windows path
/// uses an env var because every winlogon-spawned shell exports it
/// and it sidesteps `windows-sys` for what is purely a string read.
fn dispatch_hostname() -> Result<Value> {
    #[cfg(unix)]
    {
        // 256 bytes covers HOST_NAME_MAX (64 on Linux, 255 on macOS)
        // with room for the trailing NUL.
        let mut buf = vec![0u8; 256];
        // SAFETY: `gethostname` writes at most `buf.len()` bytes into
        // `buf` and NUL-terminates on success; we read only up to the
        // first NUL (or end-of-buffer) afterwards.
        #[allow(unsafe_code)]
        let rc = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) };
        if rc != 0 {
            bail!("gethostname failed: {}", std::io::Error::last_os_error());
        }
        let nul = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
        Ok(Value::plain_string(
            String::from_utf8_lossy(&buf[..nul]).into_owned(),
        ))
    }
    #[cfg(windows)]
    {
        let name = std::env::var("COMPUTERNAME")
            .map_err(|_| anyhow!("hostname unavailable: $COMPUTERNAME is not set"))?;
        Ok(Value::plain_string(name))
    }
}

/// `user()` — login name from `$USER` (Unix) or `$USERNAME` (Windows).
/// We don't fall through to `getpwuid_r` (Unix) because the env var
/// is what every shell-init script also consults; matching that
/// convention keeps `user()` and what the user sees in `$PS1` aligned.
fn dispatch_user() -> Result<Value> {
    let var = if cfg!(windows) { "USERNAME" } else { "USER" };
    let value = std::env::var(var).map_err(|_| anyhow!("user() unavailable: ${var} is not set"))?;
    Ok(Value::plain_string(value))
}

/// Wrap a `dirs::*_dir` helper as a "must-resolve" intrinsic. The
/// `dirs` crate returns `None` when the underlying lookup truly can't
/// produce a path (no `$HOME` and no platform fallback), so an error
/// here genuinely means "this machine can't tell me where its home
/// is" — worth a hard failure instead of a silent empty string.
fn dispatch_required_dir(
    name: &'static str,
    lookup: fn() -> Option<std::path::PathBuf>,
) -> Result<Value> {
    let path =
        lookup().ok_or_else(|| anyhow!("{name}() unavailable: could not determine the path"))?;
    Ok(Value::plain_string(path.to_string_lossy().into_owned()))
}

/// Wrap a `dirs::*_dir` helper that legitimately returns `None` on
/// macOS / Windows (`state_dir`, `runtime_dir`). The return type at
/// the language level is `String?`; users `??` a fallback when they
/// run on a non-Linux host.
fn dispatch_optional_dir(lookup: fn() -> Option<std::path::PathBuf>) -> Value {
    lookup().map_or(Value::Null, |p| {
        Value::plain_string(p.to_string_lossy().into_owned())
    })
}

fn dispatch_split(args: &[CallArg], env: &Env<'_, '_>) -> Result<Value> {
    let s = call_string(args, env, "s", 0)?;
    let sep = call_string(args, env, "sep", 1)?;
    if sep.is_empty() {
        bail!("split(s, sep): `sep` must not be empty");
    }
    let parts: Vec<Value> = s.split(&sep).map(Value::plain_string).collect();
    Ok(Value::List(parts))
}

fn dispatch_join(args: &[CallArg], env: &Env<'_, '_>) -> Result<Value> {
    let xs = eval_call_arg(args, env, "xs", 0)?;
    let sep = call_string(args, env, "sep", 1)?;
    let Value::List(items) = xs else {
        bail!(
            "join(xs, sep): `xs` was {} (expected List<String>)",
            xs.type_name()
        );
    };
    let mut out = String::new();
    let mut sensitive = false;
    for (i, item) in items.into_iter().enumerate() {
        let Value::String {
            text,
            sensitive: si,
        } = item
        else {
            bail!(
                "join(xs, sep): element {i} was {} (expected String)",
                item.type_name()
            );
        };
        if i > 0 {
            out.push_str(&sep);
        }
        sensitive |= si;
        out.push_str(&text);
    }
    Ok(if sensitive {
        Value::sensitive_string(out)
    } else {
        Value::plain_string(out)
    })
}

fn dispatch_contains(args: &[CallArg], env: &Env<'_, '_>) -> Result<Value> {
    let haystack = call_string(args, env, "haystack", 0)?;
    let needle = call_string(args, env, "needle", 1)?;
    Ok(Value::Bool(haystack.contains(&needle)))
}

fn dispatch_replace(args: &[CallArg], env: &Env<'_, '_>) -> Result<Value> {
    let s = call_string_value(args, env, "s", 0)?;
    let from = call_string(args, env, "from", 1)?;
    let to = call_string_value(args, env, "to", 2)?;
    if from.is_empty() {
        bail!("replace(s, from, to): `from` must not be empty");
    }
    let text = s.text.replace(&from, &to.text);
    Ok(if s.sensitive || to.sensitive {
        Value::sensitive_string(text)
    } else {
        Value::plain_string(text)
    })
}

fn dispatch_trim(args: &[CallArg], env: &Env<'_, '_>) -> Result<Value> {
    let s = call_string_value(args, env, "s", 0)?;
    let text = s.text.trim().to_string();
    Ok(if s.sensitive {
        Value::sensitive_string(text)
    } else {
        Value::plain_string(text)
    })
}

fn dispatch_starts_with(args: &[CallArg], env: &Env<'_, '_>) -> Result<Value> {
    let s = call_string(args, env, "s", 0)?;
    let prefix = call_string(args, env, "prefix", 1)?;
    Ok(Value::Bool(s.starts_with(&prefix)))
}

fn dispatch_ends_with(args: &[CallArg], env: &Env<'_, '_>) -> Result<Value> {
    let s = call_string(args, env, "s", 0)?;
    let suffix = call_string(args, env, "suffix", 1)?;
    Ok(Value::Bool(s.ends_with(&suffix)))
}

fn dispatch_str_len(args: &[CallArg], env: &Env<'_, '_>) -> Result<Value> {
    let s = call_string(args, env, "s", 0)?;
    // Count Unicode scalar values, not bytes. The conversion only fails
    // for a string longer than `i64::MAX` chars, which cannot exist.
    let n = i64::try_from(s.chars().count()).context("str_len: string too long")?;
    Ok(Value::Int(n))
}

fn dispatch_shell(args: &[CallArg], env: &Env<'_, '_>) -> Result<Value> {
    let kind = call_string(args, env, "kind", 0)?;
    let name = call_string(args, env, "name", 1)?;
    // `script` may carry a sensitive flag at the value level. We
    // propagate it to the resource so the diff renderer can show a
    // `[sensitive]` hint in the default-mode summary. The hint does
    // not redact content — verbose mode reveals everything by design
    // (see `--verbose-will-reveal-sensitive-content` for the consent
    // story); the hint just tells the operator "this body field is
    // going to print secrets if you opt in."
    let script = call_string_value(args, env, "script", 2)?;
    let kind = ShellKind::parse(&kind)?;
    Ok(Value::Resource(ResourceState::Shell {
        kind,
        name,
        cwd: env.graph.keron_root.clone(),
        script: script.text,
        sensitive: script.sensitive,
    }))
}

/// Construct a `Package` resource. Each of the three package
/// constructors (`brew`/`cargo`/`winget`) routes through here with
/// the manager identity preselected; the only argument is the
/// package name, validated by the type checker as a `String`.
fn dispatch_package(args: &[CallArg], env: &Env<'_, '_>, manager: PackageManager) -> Result<Value> {
    let name = call_string(args, env, "name", 0)?;
    crate::packages::validate_package_name(manager, &name)?;
    // Only brew/cask understand tap qualification; cargo/winget reject
    // a tap_url even if one snuck through the type system (it can't —
    // their stdlib signatures don't accept it — but a defensive bail
    // here is cheap).
    let tap_url = match manager {
        PackageManager::Brew | PackageManager::BrewCask => {
            call_optional_string(args, env, "tap_url", 1)?
        }
        PackageManager::Cargo | PackageManager::Winget => None,
    };
    let tap = build_tap_spec(manager, &name, tap_url)?;
    Ok(Value::Resource(ResourceState::Package {
        manager,
        name,
        tap,
    }))
}

/// Parse `name` into an optional tap segment and validate the
/// shape / URL combo.
///
/// Rules:
///   - bare `name` (no `/`): tap is `None`. `tap_url` here is a user
///     error (URL given for a tap that doesn't exist).
///   - `user/tap/formula` (exactly two slashes, no empty segments):
///     tap is `Some(TapSpec { "user/tap", tap_url })`.
///   - anything else (one slash, three or more, empty segment): hard
///     error — these would silently produce wrong shell invocations.
fn build_tap_spec(
    manager: PackageManager,
    name: &str,
    tap_url: Option<String>,
) -> Result<Option<crate::plan::TapSpec>> {
    let segments: Vec<&str> = name.split('/').collect();
    match segments.as_slice() {
        [_single] => {
            if tap_url.is_some() {
                bail!(
                    "{} call: `tap_url` given but name `{name}` has no `user/tap/` prefix — \
                     drop the URL or qualify the name",
                    manager.kind_label()
                );
            }
            Ok(None)
        }
        [user, tap, _formula] if !user.is_empty() && !tap.is_empty() => {
            if let Some(url) = tap_url.as_deref() {
                crate::packages::brew::validate_tap_url(url)?;
            }
            Ok(Some(crate::plan::TapSpec {
                user_tap: format!("{user}/{tap}"),
                url: tap_url,
            }))
        }
        _ => bail!(
            "{} package name `{name}` must be either a bare formula (`ripgrep`) \
             or a fully-qualified `user/tap/formula` (`icepuma/keron/keron`); \
             one slash or more than two is not accepted",
            manager.kind_label()
        ),
    }
}

/// Dispatch a `secret(uri)` call to the right resolver based on the
/// scheme prefix. Failure to parse, run, or interpret the underlying
/// CLI is a hard error — there's no "gracefully missing secret" use
/// case.
///
/// The supported-schemes list is the canonical reference; adding a
/// new provider means one new arm and one CLI wrapper below.
fn resolve_secret(uri: &str, env: &Env<'_, '_>) -> Result<String> {
    // Test seam: a per-URI override short-circuits all real CLI
    // shell-outs. Keyed on the full URI so a single registry covers
    // every scheme uniformly. Production builds skip this entirely.
    #[cfg(test)]
    if let Some(v) = secret_test::lookup_override(uri) {
        return v.map_err(|msg| anyhow!("{msg}"));
    }

    if uri.starts_with("op://") {
        // Tier-1 prereq: a logged-in 1Password CLI session must exist
        // before any `op read` shell-out fires. Without this gate the
        // user would see a raw `op` error mid-eval; with it they get
        // the structured "1Password CLI session not active → sign in:
        // op signin" diagnostic — or, if the CLI itself is missing,
        // the install-URL diagnostic.
        ensure_session_active(env, crate::capability::SessionKind::OnePassword)?;
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

/// Probe a password-manager session lazily on first `secret()` that
/// needs it, cache the result on `GraphTop` for the rest of this eval,
/// and surface the right tier-1 prereq diagnostic on failure
/// (`SecretCli` if the binary is missing, `SecretSession` if it's
/// present but signed out). Cache scope is the eval run, not the
/// thread — so a second invocation in the same process (LSP, daemon,
/// integration test) re-probes from scratch.
fn ensure_session_active(env: &Env<'_, '_>, kind: crate::capability::SessionKind) -> Result<()> {
    // Split the read and the fallback into separate statements so the
    // immutable `Ref` is dropped before the closure's `borrow_mut()`
    // — otherwise both borrows are alive at once and the RefCell
    // panics. (The naive `.unwrap_or_else(...)` chain holds the read
    // borrow across the closure body.)
    let cached = env.graph.session_cache.borrow().get(&kind).copied();
    let state = cached.unwrap_or_else(|| {
        let probed = env.graph.prereq_probe.session_state(kind);
        env.graph.session_cache.borrow_mut().insert(kind, probed);
        probed
    });
    match state {
        crate::capability::SessionState::Active => Ok(()),
        crate::capability::SessionState::NoSession => Err(anyhow::Error::msg(
            crate::capability::prereq_report(crate::capability::Prerequisite::SecretSession(kind))
                .to_string(),
        )),
        crate::capability::SessionState::NotInstalled => Err(anyhow::Error::msg(
            crate::capability::prereq_report(crate::capability::Prerequisite::SecretCli(kind))
                .to_string(),
        )),
    }
}

/// Shell out to the 1Password CLI for `op://Vault/Item/field` URIs.
/// `op read` accepts the URI verbatim; stdout is the secret value
/// with one trailing newline stripped (matching how the CLI prints).
/// `stdin` is pinned to `/dev/null` so any interactive prompt
/// (biometric, expired session) fails on EOF rather than stealing the
/// parent terminal — defence-in-depth against the case where the
/// session-state probe in `capability::probe_session_state` somehow
/// reported `Active` when it isn't (e.g. a session that expired
/// between probe and read).
///
/// The function itself is `#[mutants::skip]` because the
/// `Command::new("op")` invocation can't be exercised in tests
/// without the CLI on `$PATH`; the testable logic — status / stdout
/// decoding — lives in [`decode_op_output`].
#[cfg_attr(test, mutants::skip)]
fn real_resolve_op(uri: &str) -> Result<String> {
    let output = std::process::Command::new("op")
        .arg("read")
        .arg(uri)
        .stdin(std::process::Stdio::null())
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
/// Both halves must be non-empty and must not begin with `-`,
/// because both are forwarded as positional args to the
/// `infisical` CLI; a leading `-` would be parsed as a flag and
/// could exfiltrate or overwrite arbitrary state.
fn parse_infisical_uri<'a>(uri: &str, rest: &'a str) -> Result<(&'a str, &'a str)> {
    let (env, name) = rest
        .split_once('/')
        .filter(|(env, name)| !env.is_empty() && !name.is_empty())
        .ok_or_else(|| anyhow!("infisical URI must be `infisical://<env>/<name>`, got `{uri}`"))?;
    if env.starts_with('-') || name.starts_with('-') {
        bail!(
            "infisical URI components must not begin with `-` (would be parsed as a CLI flag), got `{uri}`"
        );
    }
    Ok((env, name))
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
/// Empty item or empty field is an error. Neither may begin with
/// `-` — both are forwarded as positional args to `bw get` and a
/// leading dash would be parsed as a flag.
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
    if item.starts_with('-') || field.starts_with('-') {
        bail!(
            "bitwarden URI components must not begin with `-` (would be parsed as a CLI flag), got `{uri}`"
        );
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
    // Strip a single trailing line terminator. On Windows the output
    // ends `\r\n`; popping only `\n` would leave a stray `\r` on the
    // captured value (e.g. a secret would gain an invisible carriage
    // return).
    if value.ends_with('\n') {
        value.pop();
        if value.ends_with('\r') {
            value.pop();
        }
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

fn detect_os_type() -> String {
    crate::platform::detect_os_family().label().to_string()
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
/// and rendering it with the supplied variables. `source` is the path
/// of the template file (resolved relative to the importing module's
/// source directory); `target` is where the rendered text will land at
/// apply time. The rendered content is frozen into
/// [`ResourceState::Template`] at eval time so the apply step is a
/// plain write.
fn dispatch_template(args: &[CallArg], env: &Env<'_, '_>) -> Result<Value> {
    let source = call_string(args, env, "source", 0)?;
    let target = call_string(args, env, "target", 1)?;
    let (vars, sensitive) = call_string_map(args, env, "vars", 2)?;
    let resolved = resolve_managed_path(&source, env, "template", "source")?;
    let raw = std::fs::read_to_string(&resolved).with_context(|| {
        format!(
            "could not read template source `{source}` (resolved to `{}`)",
            resolved.display()
        )
    })?;
    let rendered = render_template(&source, &raw, &vars)
        .with_context(|| format!("rendering template `{source}`"))?;
    Ok(Value::Resource(ResourceState::Template {
        path: PathBuf::from(target),
        content: rendered,
        sensitive,
    }))
}

/// Resolve a user-supplied path argument and pin it inside the keron
/// root. Applies to the source side of resources `keron apply` owns —
/// the `source` of a symlink and the `source` of a template — because
/// those must live inside the directory the user pointed the CLI at;
/// the project is otherwise free to symlink to or template from
/// arbitrary host paths, which defeats the "the keron dir is the
/// single source of truth" model.
///
/// Resolution rules:
/// - Relative paths are joined against the importing module file's
///   directory, so `source = "./zshrc"` next to
///   `<root>/sub/foo.keron` means `<root>/sub/zshrc`.
/// - Absolute paths are taken as-is — typically produced by
///   interpolating `${keron_root()}` into the string.
/// - The candidate is canonicalized (resolves `..`, follows any
///   intermediate symlinks) and then required to be a descendant of
///   `env.graph.keron_root` (itself canonicalized at run start).
/// - The canonical target replaces the raw user value; the executor
///   and the diff renderer both see the absolute, dereferenced path
///   so a moved symlink does not silently target a different file.
/// - **The leaf must not be a symlink**. Templating from a symlink or
///   symlinking to another symlink would chain indirection that the user
///   almost certainly didn't intend; we refuse rather than silently
///   follow. Intermediate components may still be symlinks (e.g. a
///   symlinked keron root); only the final component is rejected.
fn resolve_managed_path(raw: &str, env: &Env<'_, '_>, kind: &str, arg: &str) -> Result<PathBuf> {
    let candidate = PathBuf::from(raw);
    let absolute = if candidate.is_absolute() {
        candidate
    } else {
        let ModuleId(module_path) = &env.current;
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
fn render_template(name: &str, src: &str, vars: &HashMap<String, String>) -> Result<String> {
    let mut tera = tera::Tera::default();
    tera.functions.clear();
    tera.autoescape_on(Vec::new());
    tera.add_raw_template(name, src)
        .map_err(|e| anyhow!("parsing template: {}", format_tera_error(&e)))?;
    let mut ctx = tera::Context::new();
    for (k, v) in vars {
        ctx.insert(k, v);
    }
    tera.render(name, &ctx)
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

/// Like [`call_string`] but tolerates the arg being omitted *or*
/// supplied as `null`. Intrinsics receive their args slice raw — they
/// don't go through [`bind_params`] — so defaults declared on the
/// stdlib signature aren't substituted; this helper papers over that
/// for `String? = null` intrinsic params.
fn call_optional_string(
    args: &[CallArg],
    env: &Env<'_, '_>,
    name: &str,
    positional_idx: usize,
) -> Result<Option<String>> {
    let named = args
        .iter()
        .find(|a| a.name.as_ref().is_some_and(|n| n.node == name));
    let arg = if let Some(a) = named {
        a
    } else {
        let Some(a) = args.iter().filter(|a| a.name.is_none()).nth(positional_idx) else {
            return Ok(None);
        };
        a
    };
    match eval_expr(&arg.value, env)? {
        Value::Null => Ok(None),
        Value::String { text, sensitive } => {
            if sensitive {
                bail!("sensitive String cannot be used for `{name}`");
            }
            Ok(Some(text))
        }
        other => bail!("expected String? for `{name}`, got {}", other.type_name()),
    }
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
#[path = "eval_tests.rs"]
mod tests;
