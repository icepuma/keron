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
        // entry so the convention
        // `template(source = "tmpl.tpl", target = X, vars = {"body": Y})`
        // works as a direct stand-in for the old
        // `file(target = X, content = Y)` shape. Tests that care
        // about template-level mechanics seed their own template
        // file via `seed_template`.
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
        id: keron_modules::ModuleId(canonical),
    }])
    .unwrap_or_else(|errs| panic!("resolve failed: {errs:?}"));
    let states = eval_graph(&graph, &keron_root).unwrap_or_else(|e| panic!("eval failed: {e}"));
    (states, keron_root)
}

fn run_with_templates(src: &str, templates: &[(&str, &str)]) -> Vec<ResourceState> {
    run_result_with_templates(src, templates).unwrap_or_else(|e| panic!("eval failed: {e}"))
}

fn run_result_with_templates(src: &str, templates: &[(&str, &str)]) -> Result<Vec<ResourceState>> {
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
        id: keron_modules::ModuleId(canonical),
    }])
    .map_err(|errs| anyhow!("resolve failed: {errs:?}"))?;
    eval_graph(&graph, &keron_root)
}

fn first_file_path(states: &[ResourceState]) -> &PathBuf {
    match &states[0] {
        ResourceState::Template { path, .. } => path,
        ResourceState::Symlink { from, .. } => from,
        ResourceState::SshKey { private_path, .. } => private_path,
        // The helper is for filesystem-shaped resources; package
        // resources don't have a path, so callers shouldn't reach
        // for it here. Loud failure beats silently picking the
        // name field as a "path".
        ResourceState::Package { manager, name, .. } => {
            panic!(
                "first_file_path: expected filesystem resource, got Package({manager:?}, {name:?})"
            )
        }
        ResourceState::Shell { name, .. } => {
            panic!("first_file_path: expected filesystem resource, got Shell({name:?})")
        }
        ResourceState::Tap(spec) => {
            panic!(
                "first_file_path: expected filesystem resource, got Tap({user_tap:?})",
                user_tap = spec.user_tap
            )
        }
        ResourceState::GpgKey { fingerprint, .. } => {
            panic!("first_file_path: expected filesystem resource, got GpgKey({fingerprint})")
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
fn eval_unary_logical_not_flips_bool() {
    let v = eval_unary(UnaryOp::Not, Value::Bool(true)).unwrap();
    assert!(matches!(v, Value::Bool(false)));
    let v = eval_unary(UnaryOp::Not, Value::Bool(false)).unwrap();
    assert!(matches!(v, Value::Bool(true)));
}

#[test]
fn eval_unary_not_on_non_bool_errors() {
    let e = eval_unary(UnaryOp::Not, Value::Int(1)).unwrap_err();
    assert!(e.to_string().contains("unary `!`"));
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
fn eval_binop_int_add_overflow_errors() {
    let e = eval_binop(BinOp::Add, int(i64::MAX), int(1)).unwrap_err();
    assert!(e.to_string().contains("overflow"), "got: {e}");
}

#[test]
fn eval_binop_int_sub_overflow_errors() {
    let e = eval_binop(BinOp::Sub, int(i64::MIN), int(1)).unwrap_err();
    assert!(e.to_string().contains("overflow"), "got: {e}");
}

#[test]
fn eval_binop_int_mul_overflow_errors() {
    let e = eval_binop(BinOp::Mul, int(i64::MAX), int(2)).unwrap_err();
    assert!(e.to_string().contains("overflow"), "got: {e}");
}

#[test]
fn eval_binop_int_div_min_by_neg_one_errors() {
    let e = eval_binop(BinOp::Div, int(i64::MIN), int(-1)).unwrap_err();
    assert!(e.to_string().contains("overflow"), "got: {e}");
}

#[test]
fn eval_binop_int_pow_overflow_errors() {
    let e = eval_binop(BinOp::Pow, int(2), int(64)).unwrap_err();
    assert!(e.to_string().contains("overflow"), "got: {e}");
}

#[test]
fn eval_binop_int_pow_negative_exponent_has_clear_message() {
    let e = eval_binop(BinOp::Pow, int(2), int(-1)).unwrap_err();
    let msg = e.to_string();
    assert!(msg.contains("negative exponent"), "got: {msg}");
    assert!(
        !msg.contains("u32"),
        "must not leak the u32 conversion: {msg}"
    );
}

#[test]
fn eval_binop_double_div_by_zero_is_non_finite_error() {
    let e = eval_binop(BinOp::Div, dbl(1.0), dbl(0.0)).unwrap_err();
    assert!(e.to_string().contains("non-finite"), "got: {e}");
    let e = eval_binop(BinOp::Div, dbl(0.0), dbl(0.0)).unwrap_err();
    assert!(e.to_string().contains("non-finite"), "got: {e}");
}

#[test]
fn eval_binop_double_pow_nan_is_non_finite_error() {
    // (-1.0) ** 0.5 is NaN.
    let e = eval_binop(BinOp::Pow, dbl(-1.0), dbl(0.5)).unwrap_err();
    assert!(e.to_string().contains("non-finite"), "got: {e}");
}

#[test]
fn int_double_equality_is_exact_past_2_53() {
    // 2^53 + 1 is not representable as f64; promoting the i64 would
    // make this wrongly equal. The exact comparison keeps it false.
    let big = (1_i64 << 53) + 1;
    let as_dbl = 2.0_f64.powi(53); // exactly 2^53
    assert!(!value_eq(&Value::Int(big), &Value::Double(as_dbl)));
    // The exact integer value still compares equal.
    assert!(value_eq(&Value::Int(1 << 53), &Value::Double(as_dbl)));
    // Ordering past the gap is correct too.
    assert_eq!(
        value_cmp(&Value::Int(big), &Value::Double(as_dbl)).unwrap(),
        std::cmp::Ordering::Greater
    );
}

#[test]
fn take_stdout_strips_crlf_not_just_lf() {
    assert_eq!(
        take_stdout(b"secret\r\n".to_vec(), "cmd").unwrap(),
        "secret"
    );
    assert_eq!(take_stdout(b"secret\n".to_vec(), "cmd").unwrap(), "secret");
    // A lone trailing CR (no LF) is left intact — only a full line
    // terminator is stripped.
    assert_eq!(
        take_stdout(b"secret\r".to_vec(), "cmd").unwrap(),
        "secret\r"
    );
}

#[test]
fn eval_unary_neg_int_min_errors() {
    let e = eval_unary(UnaryOp::Neg, Value::Int(i64::MIN)).unwrap_err();
    assert!(e.to_string().contains("overflow"), "got: {e}");
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
fn call_depth_resets_between_sequential_top_level_calls() {
    // Pins CallDepthGuard::drop: without the `-= 1` on drop (or
    // with it flipped to `+=`/`*=`/no-op), depth would grow
    // monotonically across sequential user-fn calls and the
    // 257th call would bail with the recursion limit. 300
    // independent reconciles each invoke one user fn at depth 1;
    // with the guard restored, depth returns to 0 between calls
    // and all 300 succeed.
    use std::fmt::Write as _;
    let mut src = String::from("fn one(): Int { 1 }\n");
    for i in 0..300 {
        writeln!(
                src,
                "reconcile template(source = \"tmpl.tpl\", target = \"/k{i}-${{one()}}\", vars = {{\"body\": \"\"}})",
            )
            .unwrap();
    }
    let states = run_result_with_templates(&src, &[("tmpl.tpl", "")])
        .expect("300 sequential top-level user-fn calls must succeed within MAX_CALL_DEPTH");
    assert_eq!(states.len(), 300, "expected one state per reconcile");
}

#[test]
fn runtime_recursion_bails_before_blowing_the_stack() {
    // `fn loop(): Symlink { loop() }` is well-typed (the body's
    // recursive call has the right return type), but evaluating
    // it without the depth guard would unwind the Rust stack.
    // The guard surfaces a clean error well below the OS limit.
    let err = run_result_with_templates(
        "fn loop(): Symlink { loop() }\n\
             reconcile loop()\n",
        &[],
    )
    .expect_err("unbounded recursion must error, not panic");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("call depth exceeded"),
        "expected depth-guard message, got: {msg}"
    );
}

#[test]
fn eval_graph_emits_resources_for_reconciles() {
    let states = run(
        "reconcile template(source = \"tmpl.tpl\", target = \"/x\", vars = {\"body\": \"y\"})\n",
    );
    assert_eq!(states.len(), 1);
    assert_eq!(first_file_path(&states), &PathBuf::from("/x"));
    assert_eq!(first_file_content(&states), "y");
}

#[test]
fn eval_graph_returns_empty_when_no_reconciles() {
    let states = run(
        "val f: Template = template(source = \"tmpl.tpl\", target = \"/x\", vars = {\"body\": \"y\"})\n",
    );
    assert!(states.is_empty());
}

#[test]
fn template_rendering_rejects_builtin_functions() {
    let err = run_result_with_templates(
        "reconcile template(source = \"tmpl.tpl\", target = \"/x\", vars = {})\n",
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
             \ttemplate(source = \"tmpl.tpl\", target = path, vars = {\"body\": body})\n\
             }\n\
             reconcile file(\"/x\")\n",
    );
    assert_eq!(first_file_content(&states), "/x body");
}

#[test]
fn string_predicates_and_length_evaluate() {
    // starts_with / ends_with gate on host name shape; len
    // counts chars not bytes (`é` is two UTF-8 bytes, one char).
    let states = run("val a: Boolean = starts_with(\"work-laptop\", \"work-\")\n\
             val b: Boolean = ends_with(\"host.local\", \".local\")\n\
             val n: Int = len(\"héllo\")\n\
             reconcile template(source = \"tmpl.tpl\", target = \"/x\", vars = {\"body\": \"${a} ${b} ${n}\"})\n");
    assert_eq!(first_file_content(&states), "true true 5");
}

#[test]
fn struct_field_default_sees_module_scope_not_caller_local() {
    // The default `greeting` must resolve to the module `val`
    // ("module"), never to the same-named fn param ("param"). Caller
    // scope leaking in would be dynamic scoping and, with a
    // different param type, a type-soundness hole.
    let states = run("val greeting: String = \"module\"\n\
             struct Msg { text: String = greeting }\n\
             fn make(greeting: String): Msg { Msg {} }\n\
             reconcile template(source = \"tmpl.tpl\", target = \"/x\", vars = {\"body\": make(\"param\").text})\n");
    assert_eq!(first_file_content(&states), "module");
}

#[test]
fn void_match_at_top_level_executes_selected_arm_resources() {
    // A `Void` `match` used as a block trailing must run the matched
    // arm's reconciles instead of silently dropping them.
    let states = run("for x in [1] {\n\
             \tmatch x {\n\
             \t\t1 => if true { reconcile template(source = \"tmpl.tpl\", target = \"/x\", vars = {\"body\": \"y\"}) } else {},\n\
             \t\t_ => if false {} else {},\n\
             \t}\n\
             }\n");
    assert_eq!(states.len(), 1, "matched arm's resource should be emitted");
    assert_eq!(first_file_content(&states), "y");
}

#[test]
fn for_in_void_fn_body_evaluates_instead_of_bailing() {
    // A `for` in a `Void` fn body is admitted by the checker; the
    // evaluator must run it for effect and return `Void` rather than
    // bailing with "`for` is not a value expression". The loop body is
    // value-context (no reconciles allowed), so it emits nothing — the
    // reconcile that actually produces the resource is at top level.
    let states = run("fn noop(): Void { for x in [1, 2, 3] { val y = x } }\n\
             reconcile template(source = \"tmpl.tpl\", target = \"/x\", vars = {\"body\": \"ok\"})\n\
             val _unused: Void = noop()\n");
    assert_eq!(states.len(), 1, "top-level reconcile still emits");
    assert_eq!(first_file_content(&states), "ok");
}

#[test]
fn push_resources_unwraps_lists() {
    let states = run(
        "val xs: List<Template> = [template(source = \"tmpl.tpl\", target = \"/a\", vars = {\"body\": \"\"}), \
                                    template(source = \"tmpl.tpl\", target = \"/b\", vars = {\"body\": \"\"})]\n\
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
        "if true { reconcile template(source = \"tmpl.tpl\", target = \"/yes\", vars = {\"body\": \"\"}) }\n",
    );
    assert_eq!(states.len(), 1);
    assert_eq!(first_file_path(&states), &PathBuf::from("/yes"));
}

#[test]
fn exec_void_expr_skips_else_branch_when_true() {
    let states = run("if true {\n\
             \treconcile template(source = \"tmpl.tpl\", target = \"/yes\", vars = {\"body\": \"\"})\n\
             } else {\n\
             \treconcile template(source = \"tmpl.tpl\", target = \"/no\", vars = {\"body\": \"\"})\n\
             }\n");
    assert_eq!(states.len(), 1);
    assert_eq!(first_file_path(&states), &PathBuf::from("/yes"));
}

#[test]
fn exec_void_block_runs_multiple_effect_statements() {
    // Several gated effect statements in one exec-void block all
    // reach the *real* sink — the eval arm for `Stmt::Expr` must stay
    // in lockstep with `reject_reconcile_in_exec_void_block`.
    let states = run("if true {\n\
             \tif true {\n\
             \t\treconcile template(source = \"tmpl.tpl\", target = \"/one\", vars = {\"body\": \"\"})\n\
             \t}\n\
             \tfor n in [2, 3] {\n\
             \t\treconcile template(source = \"tmpl.tpl\", target = \"/${n}\", vars = {\"body\": \"\"})\n\
             \t}\n\
             \treconcile template(source = \"tmpl.tpl\", target = \"/four\", vars = {\"body\": \"\"})\n\
             }\n");
    assert_eq!(states.len(), 4, "all four gated resources must land");
    assert_eq!(first_file_path(&states), &PathBuf::from("/one"));
}

#[test]
fn exec_void_expr_handles_top_level_for() {
    let states = run("for n in [1, 2, 3] {\n\
             \treconcile template(source = \"tmpl.tpl\", target = \"/${n}\", vars = {\"body\": \"\"})\n\
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
             \treconcile template(source = \"tmpl.tpl\", target = base, vars = {\"body\": \"\"})\n\
             }\n");
    assert_eq!(states.len(), 1);
    assert_eq!(first_file_path(&states), &PathBuf::from("/v"));
}

#[test]
fn iterate_runs_body_per_map_entry() {
    let states = run("for (k, v) in {\"a\": 1, \"b\": 2} {\n\
             \treconcile template(source = \"tmpl.tpl\", target = \"/${k}\", vars = {\"body\": \"${v}\"})\n\
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
        "reconcile template(source = \"tmpl.tpl\", target = \"/${2 + 3}-${10 - 4}-${2 * 3}-${10 / 2}-${2 ** 8}\", vars = {\"body\": \"\"})\n",
    );
    assert_eq!(first_file_path(&states), &PathBuf::from("/5-6-6-5-256"));
}

#[test]
fn double_arithmetic_in_interpolation_round_trips() {
    let states = run("val sum: Double = 1.5 + 2.0\n\
             val diff: Double = 5.5 - 2.0\n\
             val prod: Double = 2.0 * 3.0\n\
             val quot: Double = 10.0 / 4.0\n\
             reconcile template(source = \"tmpl.tpl\", target = \"/${sum}-${diff}-${prod}-${quot}\", vars = {\"body\": \"\"})\n");
    assert_eq!(first_file_path(&states), &PathBuf::from("/3.5-3.5-6-2.5"));
}

#[test]
fn mixed_int_double_arithmetic_round_trips() {
    let states = run("val a: Double = 1 + 2.5\n\
             val b: Double = 5 - 1.5\n\
             val c: Double = 2 * 2.5\n\
             val d: Double = 1.5 * 2\n\
             reconcile template(source = \"tmpl.tpl\", target = \"/${a}-${b}-${c}-${d}\", vars = {\"body\": \"\"})\n");
    assert_eq!(first_file_path(&states), &PathBuf::from("/3.5-3.5-5-3"));
}

#[test]
fn unary_neg_in_interpolation_round_trips() {
    let states = run("val x: Int = -7\n\
             val y: Double = -2.5\n\
             reconcile template(source = \"tmpl.tpl\", target = \"/${x}\", vars = {\"body\": \"${y}\"})\n");
    assert_eq!(first_file_path(&states), &PathBuf::from("/-7"));
    assert_eq!(first_file_content(&states), "-2.5");
}

#[test]
fn equality_observable_via_branching() {
    let states = run("val same: Boolean = 1 == 1\n\
             val diff: Boolean = 1 == 2\n\
             reconcile template(source = \"tmpl.tpl\", target = if same { \"/yes\" } else { \"/no\" }, vars = {\"body\": \"\"})\n\
             reconcile template(source = \"tmpl.tpl\", target = if diff { \"/yes\" } else { \"/no\" }, vars = {\"body\": \"\"})\n");
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
        "reconcile template(source = \"tmpl.tpl\", target = if 1 < 2 { \"/lt\" } else { \"/ge\" }, vars = {\"body\": \"\"})\n\
             reconcile template(source = \"tmpl.tpl\", target = if 2 <= 2 { \"/le\" } else { \"/gt\" }, vars = {\"body\": \"\"})\n\
             reconcile template(source = \"tmpl.tpl\", target = if 3 > 2 { \"/gt\" } else { \"/le\" }, vars = {\"body\": \"\"})\n\
             reconcile template(source = \"tmpl.tpl\", target = if 2 >= 2 { \"/ge\" } else { \"/lt\" }, vars = {\"body\": \"\"})\n",
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
fn short_circuit_and_or_observable_via_branching() {
    // Pin the four truth-table rows of `&&` and `||`. Catches:
    //   - the `delete match arm BinOp::And | BinOp::Or` mutation
    //     (would surface as eval_short_circuit returning None and
    //     a downstream "expected Boolean" type error)
    //   - the `== with !=` mutation on the short-circuit comparison
    //     (would invert which side triggers the early-return; e.g.
    //     `false && _` would no longer short-circuit and `true && _`
    //     would short-circuit on the left)
    let states = run(
        "reconcile template(source = \"tmpl.tpl\", target = if true && true { \"/tt\" } else { \"/no\" }, vars = {\"body\": \"\"})\n\
             reconcile template(source = \"tmpl.tpl\", target = if true && false { \"/no\" } else { \"/tf\" }, vars = {\"body\": \"\"})\n\
             reconcile template(source = \"tmpl.tpl\", target = if false || true { \"/ft\" } else { \"/no\" }, vars = {\"body\": \"\"})\n\
             reconcile template(source = \"tmpl.tpl\", target = if false || false { \"/no\" } else { \"/ff\" }, vars = {\"body\": \"\"})\n",
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
            PathBuf::from("/tt"),
            PathBuf::from("/tf"),
            PathBuf::from("/ft"),
            PathBuf::from("/ff"),
        ]
    );
}

#[test]
fn string_equality_distinguishes_distinct_values() {
    let states = run(
        "reconcile template(source = \"tmpl.tpl\", target = if \"a\" == \"a\" { \"/eq\" } else { \"/ne\" }, vars = {\"body\": \"\"})\n\
             reconcile template(source = \"tmpl.tpl\", target = if \"a\" == \"b\" { \"/eq\" } else { \"/ne\" }, vars = {\"body\": \"\"})\n",
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
        "reconcile template(source = \"tmpl.tpl\", target = if true == true { \"/eq\" } else { \"/ne\" }, vars = {\"body\": \"\"})\n\
             reconcile template(source = \"tmpl.tpl\", target = if true == false { \"/eq\" } else { \"/ne\" }, vars = {\"body\": \"\"})\n",
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
        "reconcile template(source = \"tmpl.tpl\", target = if 2 == 2.0 { \"/eq\" } else { \"/ne\" }, vars = {\"body\": \"\"})\n\
             reconcile template(source = \"tmpl.tpl\", target = if 2 == 2.5 { \"/eq\" } else { \"/ne\" }, vars = {\"body\": \"\"})\n",
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
    reconcile template(source = \"tmpl.tpl\", target = pair(right = \"R\", left = \"L\"), vars = {\"body\": \"\"})\n");
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
    reconcile template(source = \"tmpl.tpl\", target = pick(\"a\"), vars = {\"body\": \"\"})\n",
    );
    assert_eq!(first_file_path(&states), &PathBuf::from("a-default"));
}

#[test]
fn call_string_falls_back_to_positional() {
    // Each `template` arg resolved positionally (no `name = ...`
    // syntax). Mutating the positional-fallback path in
    // `eval_call_arg` would re-route the args.
    let states = run_with_templates(
        "reconcile template(\"body.tpl\", \"/positional\", {\"body\": \"hi\"})\n",
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
             reconcile template(source = \"tmpl.tpl\", target = \"/${tag}\", vars = {\"body\": \"\"})\n");
    assert_eq!(first_file_path(&states), &PathBuf::from("/ok"));
}

#[test]
fn struct_field_access_round_trips() {
    let states = run("struct Host { name: String, port: Int }\n\
             val h: Host = Host { name: \"alpha\", port: 22 }\n\
             reconcile template(source = \"tmpl.tpl\", target = \"/${h.name}-${h.port}\", vars = {\"body\": \"\"})\n");
    assert_eq!(first_file_path(&states), &PathBuf::from("/alpha-22"));
}

#[test]
fn struct_construction_field_order_is_free() {
    let states = run("struct Pair { a: String, b: String }\n\
             val p1: Pair = Pair { a: \"x\", b: \"y\" }\n\
             val p2: Pair = Pair { b: \"y\", a: \"x\" }\n\
             reconcile template(source = \"tmpl.tpl\", target = \"/${p1.a}-${p1.b}\", vars = {\"body\": \"\"})\n\
             reconcile template(source = \"tmpl.tpl\", target = \"/${p2.a}-${p2.b}\", vars = {\"body\": \"\"})\n");
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
             reconcile template(source = \"tmpl.tpl\", target = \"/${label(c)}\", vars = {\"body\": \"\"})\n");
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
             reconcile template(source = \"tmpl.tpl\", target = \"/${axis(Point { x: 0, y: 0 })}\", vars = {\"body\": \"\"})\n\
             reconcile template(source = \"tmpl.tpl\", target = \"/${axis(Point { x: 3, y: 0 })}\", vars = {\"body\": \"\"})\n\
             reconcile template(source = \"tmpl.tpl\", target = \"/${axis(Point { x: 0, y: 5 })}\", vars = {\"body\": \"\"})\n\
             reconcile template(source = \"tmpl.tpl\", target = \"/${axis(Point { x: 2, y: 3 })}\", vars = {\"body\": \"\"})\n");
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
             reconcile template(source = \"tmpl.tpl\", target = \"/${label(\"hello\")}\", vars = {\"body\": \"\"})\n");
    assert_eq!(first_file_path(&states), &PathBuf::from("/hello"));
}

#[test]
fn union_value_compares_equal_to_string_literal() {
    let states = run("type Mode = \"on\" | \"off\"\n\
             val m: Mode = \"on\"\n\
             reconcile template(source = \"tmpl.tpl\", target = if m == \"on\" { \"/active\" } else { \"/idle\" }, vars = {\"body\": \"\"})\n");
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
             reconcile template(source = \"tmpl.tpl\", target = \"/${pick(0)}\", vars = {\"body\": \"\"})\n\
             reconcile template(source = \"tmpl.tpl\", target = \"/${pick(1)}\", vars = {\"body\": \"\"})\n\
             reconcile template(source = \"tmpl.tpl\", target = \"/${pick(7)}\", vars = {\"body\": \"\"})\n");
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
             reconcile template(source = \"tmpl.tpl\", target = \"/${label(true)}\", vars = {\"body\": \"\"})\n\
             reconcile template(source = \"tmpl.tpl\", target = \"/${label(false)}\", vars = {\"body\": \"\"})\n");
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
    // routing (source / target / vars) and `render_template`'s
    // Tera substitution.
    let states = run_with_templates(
        "reconcile template(\n\
                 \tsource = \"shell.tpl\",\n\
                 \ttarget = \"/etc/passwd\",\n\
                 \tvars = {\"user\": \"alice\", \"shell\": \"/bin/zsh\"},\n\
             )\n",
        &[("shell.tpl", "{{ user }}:x:1000:{{ shell }}\n")],
    );
    assert_eq!(states.len(), 1);
    assert_eq!(first_file_path(&states), &PathBuf::from("/etc/passwd"));
    assert_eq!(first_file_content(&states), "alice:x:1000:/bin/zsh\n");
}

#[test]
fn multiline_string_flows_into_template_vars() {
    let states = run(r#"val body = """
  alpha
    beta
  """

reconcile template(source = "tmpl.tpl", target = "/msg", vars = {"body": body})
"#);
    assert_eq!(first_file_content(&states), "alpha\n  beta");
}

#[test]
fn multiline_interpolation_indents_continuation_lines() {
    let states = run(r#"val chunk = """
  one
  two
  """

val body = """
  block:
    ${chunk}
  """

reconcile template(source = "tmpl.tpl", target = "/msg", vars = {"body": body})
"#);
    assert_eq!(first_file_content(&states), "block:\n  one\n  two");
}

#[test]
fn multiline_interpolation_preserves_empty_continuation_lines() {
    let states = run(concat!(
        "val chunk = \"\"\"\n",
        "  one\n",
        "    \n",
        "  two\n",
        "  \"\"\"\n\n",
        "val body = \"\"\"\n",
        "  block:\n",
        "    ${chunk}\n",
        "  \"\"\"\n\n",
        "reconcile template(source = \"tmpl.tpl\", target = \"/msg\", vars = {\"body\": body})\n",
    ));
    assert_eq!(first_file_content(&states), "block:\n  one\n\n  two");
}

#[test]
fn raw_multiline_string_keeps_shell_syntax_literal() {
    let states = run(r##"val body = r#"""
  export PATH="${HOME}/bin:$PATH"
  line\n
  """#

reconcile template(source = "tmpl.tpl", target = "/msg", vars = {"body": body})
"##);
    assert_eq!(
        first_file_content(&states),
        "export PATH=\"${HOME}/bin:$PATH\"\nline\\n"
    );
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
                 \tsource = \"greet.tpl\",\n\
                 \ttarget = \"/x\",\n\
                 \tvars = {},\n\
             )\n",
    );
    let canonical = fs::canonicalize(&entry).unwrap();
    let base_dir = canonical.parent().unwrap().to_path_buf();
    let keron_root = base_dir.clone();
    let graph = resolve(vec![EntrySource {
        text: fs::read_to_string(&entry).unwrap(),
        base_dir,
        id: keron_modules::ModuleId(canonical),
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
                 \tsource = \"intl.tpl\",\n\
                 \ttarget = \"/x\",\n\
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
                 \tsource = \"trail.tpl\",\n\
                 \ttarget = \"/x\",\n\
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
        proj.entry("reconcile template(source = \"bad.tpl\", target = \"/x\", vars = {})\n");
    let canonical = fs::canonicalize(&entry).unwrap();
    let base_dir = canonical.parent().unwrap().to_path_buf();
    let keron_root = base_dir.clone();
    let graph = resolve(vec![EntrySource {
        text: fs::read_to_string(&entry).unwrap(),
        base_dir,
        id: keron_modules::ModuleId(canonical),
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
    let out = render_template("tmpl.tpl", "hello {{ name }}!", &vars).unwrap();
    assert_eq!(out, "hello alice!");
}

#[test]
fn render_template_passes_lone_dollar_through() {
    // `$x` and `$$` have no meaning to Tera; they're literal
    // text. Pin so a `$`-flavoured engine can never sneak back
    // in and turn dotfile shell snippets into rendering errors.
    let vars = HashMap::new();
    let out = render_template("tmpl.tpl", "$5 and $$", &vars).unwrap();
    assert_eq!(out, "$5 and $$");
}

#[test]
fn render_template_does_not_autoescape_html_metacharacters() {
    // Dotfiles routinely contain `<`, `>`, `&`, `"` — autoescape
    // would mangle them into HTML entities. Pin that the
    // renderer leaves them alone.
    let mut vars = HashMap::new();
    vars.insert("payload".into(), "a < b && c > d \"q\"".into());
    let out = render_template("tmpl.tpl", "{{ payload }}", &vars).unwrap();
    assert_eq!(out, "a < b && c > d \"q\"");
}

#[test]
fn render_template_supports_tera_filters() {
    // The `default-features = false` Tera build still ships the
    // core filter set. Pin that `upper` works so a future
    // accidental flip to a no-filters build is loud.
    let mut vars = HashMap::new();
    vars.insert("user".into(), "alice".into());
    let out = render_template("tmpl.tpl", "{{ user | upper }}", &vars).unwrap();
    assert_eq!(out, "ALICE");
}

#[test]
fn render_template_error_message_names_the_user_template() {
    // Pre-fix: error mentioned only the internal placeholder
    // `__keron_inline__`. Now it names the user-visible source.
    let vars = HashMap::new();
    let err =
        render_template("dotfiles/zshrc.tpl", "{{ missing }}", &vars).expect_err("missing var");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("dotfiles/zshrc.tpl"),
        "error should name the user template: {msg}"
    );
    assert!(
        !msg.contains("__keron_inline__"),
        "internal placeholder leaked: {msg}"
    );
}

#[test]
fn template_missing_source_errors() {
    // `source` must point at an existing file. Without one, the
    // intrinsic surfaces a wrapping context line plus the
    // underlying I/O error so the user can locate the typo.
    let proj = TempProject::new("tmpl-missing-source");
    let entry =
        proj.entry("reconcile template(source = \"missing.tpl\", target = \"/x\", vars = {})\n");
    let canonical = fs::canonicalize(&entry).unwrap();
    let base_dir = canonical.parent().unwrap().to_path_buf();
    let keron_root = base_dir.clone();
    let graph = resolve(vec![EntrySource {
        text: fs::read_to_string(&entry).unwrap(),
        base_dir,
        id: keron_modules::ModuleId(canonical),
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
             reconcile template(source = \"tmpl.tpl\", target = \"/${label(1.5)}\", vars = {\"body\": \"\"})\n\
             reconcile template(source = \"tmpl.tpl\", target = \"/${label(2.5)}\", vars = {\"body\": \"\"})\n\
             reconcile template(source = \"tmpl.tpl\", target = \"/${label(7.0)}\", vars = {\"body\": \"\"})\n");
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
        "reconcile template(source = \"tmpl.tpl\", target = keron_root(), vars = {\"body\": \"\"})\n",
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
        "reconcile template(source = \"tmpl.tpl\", target = \"${keron_root()}/sub\", vars = {\"body\": \"\"})\n",
    );
    let ResourceState::Template { path, .. } = &states[0] else {
        panic!("expected template, got {:?}", states[0]);
    };
    // Build `expected` via the same string-concat the manifest used.
    // On Windows `fs::canonicalize` returns a verbatim UNC path
    // (`\\?\C:\...`) inside which `/` is a literal character — so
    // `root.join("sub")` (backslash) compares unequal to the
    // interpolated `<root>/sub`. Mirror the manifest's interpolation
    // to keep this an apples-to-apples assertion on both platforms.
    let expected: PathBuf = format!("{}/sub", root.display()).into();
    assert_eq!(path, &expected);
}

#[test]
fn os_type_intrinsic_returns_one_of_the_documented_variants() {
    // The host's actual OS isn't fixed, but it must collapse into
    // the four-variant `OsType` union — anything else means the
    // dispatcher's fallback was bypassed or a new os_info variant
    // is leaking through.
    let states = run(
        "reconcile template(source = \"tmpl.tpl\", target = os_type(), vars = {\"body\": \"\"})\n",
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
        "reconcile template(source = \"tmpl.tpl\", target = os_arch(), vars = {\"body\": \"\"})\n",
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
                 null => template(source = \"tmpl.tpl\", target = \"/opt/fallback\", vars = {\"body\": \"\"}),\n\
                 p => template(source = \"tmpl.tpl\", target = p, vars = {\"body\": \"\"}),\n\
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
                 null => template(source = \"tmpl.tpl\", target = \"/opt/fallback\", vars = {\"body\": \"\"}),\n\
                 p => template(source = \"tmpl.tpl\", target = p, vars = {\"body\": \"\"}),\n\
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
                 template(source = \"tmpl.tpl\", target = \"/missing\", vars = {\"body\": \"\"})\n\
             } else {\n\
                 template(source = \"tmpl.tpl\", target = \"/present\", vars = {\"body\": \"\"})\n\
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
                 null => template(source = \"tmpl.tpl\", target = \"/missing\", vars = {{\"body\": \"\"}}),\n\
                 v => template(source = \"tmpl.tpl\", target = v, vars = {{\"body\": \"\"}}),\n\
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
                 null => template(source = \"tmpl.tpl\", target = \"/missing\", vars = {{\"body\": \"\"}}),\n\
                 v => template(source = \"tmpl.tpl\", target = v, vars = {{\"body\": \"\"}}),\n\
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
                 null => template(source = \"tmpl.tpl\", target = \"/unset\", vars = {{\"body\": \"\"}}),\n\
                 v => template(source = \"tmpl.tpl\", target = \"/set\", vars = {{\"body\": \"\"}}),\n\
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
                 template(source = \"tmpl.tpl\", target = \"/missing\", vars = {{\"body\": \"\"}})\n\
             }} else {{\n\
                 template(source = \"tmpl.tpl\", target = \"/present\", vars = {{\"body\": \"\"}})\n\
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
                 reconcile template(source = \"secret.tpl\", target = \"/secret\", vars = {{\"body\": unwrap_secret(token)}})\n",
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
                 reconcile template(source = \"secret.tpl\", target = \"/secret\", vars = {{\"body\": unwrap_secret(token)}})\n",
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
                 reconcile template(source = \"secret.tpl\", target = \"/secret\", vars = {{\"body\": unwrap_secret(token)}})\n",
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
             reconcile template(source = \"tmpl.tpl\", target = unwrap_secret(token), vars = {{\"body\": \"\"}})\n",
    );
    let entry = proj.entry(&src);
    let canonical = fs::canonicalize(&entry).unwrap();
    let base_dir = canonical.parent().unwrap().to_path_buf();
    let keron_root = base_dir.clone();
    let graph = resolve(vec![EntrySource {
        text: src,
        base_dir,
        id: keron_modules::ModuleId(canonical),
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

/// Minimal `PrereqProbe` for eval-side tests: pretends every
/// package manager is available (eval tests don't reach the
/// plan-time prereq pass) and returns whatever `SessionState`
/// the test configured for each kind.
struct StubPrereqProbe {
    sessions: HashMap<crate::capability::SessionKind, crate::capability::SessionState>,
}

impl StubPrereqProbe {
    fn new() -> Self {
        Self {
            sessions: HashMap::new(),
        }
    }
    fn with(
        mut self,
        kind: crate::capability::SessionKind,
        state: crate::capability::SessionState,
    ) -> Self {
        self.sessions.insert(kind, state);
        self
    }
}

impl crate::capability::PrereqProbe for StubPrereqProbe {
    fn package_manager_available(&self, _pm: PackageManager) -> bool {
        true
    }
    fn session_state(
        &self,
        kind: crate::capability::SessionKind,
    ) -> crate::capability::SessionState {
        self.sessions
            .get(&kind)
            .copied()
            .unwrap_or(crate::capability::SessionState::NotInstalled)
    }
}

/// Eval the manifest at `src` with a caller-supplied prereq probe.
/// Mirror of `run_with_templates`'s graph-build dance but using
/// `eval_graph_with_prereq_probe` so the test can mock session
/// state without touching the host `op` binary.
fn eval_with_probe(
    src: &str,
    templates: &[(&str, &str)],
    probe: &dyn crate::capability::PrereqProbe,
) -> Result<Vec<ResourceState>> {
    let proj = TempProject::new("secret-session-probe");
    for (name, body) in templates {
        proj.seed_template(name, body);
    }
    let entry = proj.entry(src);
    let canonical = fs::canonicalize(&entry).unwrap();
    let base_dir = canonical.parent().unwrap().to_path_buf();
    let keron_root = base_dir.clone();
    let graph = resolve(vec![EntrySource {
        text: src.to_string(),
        base_dir,
        id: keron_modules::ModuleId(canonical),
    }])
    .unwrap_or_else(|errs| panic!("resolve failed: {errs:?}"));
    eval_graph_with_prereq_probe(&graph, &keron_root, probe)
}

#[test]
fn secret_op_session_inactive_surfaces_signin_diagnostic() {
    // No SecretOverride is installed — that's deliberate. The
    // dispatcher must fall past the test seam, hit
    // `ensure_session_active`, and fail with the tier-1 prereq
    // diagnostic *before* `real_resolve_op` shells out.
    let uri = unique_op_uri("session-inactive");
    let src = format!(
        "val token: Secret = secret(\"{uri}\")\n\
             reconcile template(source = \"tmpl.tpl\", target = unwrap_secret(token), vars = {{\"body\": \"\"}})\n",
    );
    let probe = StubPrereqProbe::new().with(
        crate::capability::SessionKind::OnePassword,
        crate::capability::SessionState::NoSession,
    );
    let err = eval_with_probe(&src, &[], &probe)
        .expect_err("inactive 1Password session must surface as a prereq error");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("1Password CLI session not active"),
        "diagnostic should name the session prereq: {msg}"
    );
    assert!(
        msg.contains("op signin"),
        "diagnostic should surface the signin command: {msg}"
    );
    assert!(
        !msg.contains("→ install:"),
        "NoSession diagnostic should NOT include install URL — the CLI is present: {msg}"
    );
}

#[test]
fn secret_op_cli_missing_surfaces_install_diagnostic() {
    // Distinct from the session-inactive path: the CLI binary
    // itself is missing, so the diagnostic must surface the
    // install URL and omit the signin command (there's nothing
    // to sign into yet).
    let uri = unique_op_uri("cli-missing");
    let src = format!(
        "val token: Secret = secret(\"{uri}\")\n\
             reconcile template(source = \"tmpl.tpl\", target = unwrap_secret(token), vars = {{\"body\": \"\"}})\n",
    );
    let probe = StubPrereqProbe::new().with(
        crate::capability::SessionKind::OnePassword,
        crate::capability::SessionState::NotInstalled,
    );
    let err = eval_with_probe(&src, &[], &probe)
        .expect_err("missing 1Password CLI must surface as a prereq error");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("1Password CLI is not installed"),
        "diagnostic should name the missing CLI: {msg}"
    );
    assert!(
        msg.contains("https://developer.1password.com/docs/cli/get-started/"),
        "NotInstalled diagnostic must include the install URL: {msg}"
    );
    assert!(
        !msg.contains("op signin"),
        "NotInstalled diagnostic must NOT prompt for signin — there's no CLI to sign into: {msg}"
    );
}

#[test]
fn secret_op_session_active_falls_through_to_resolver() {
    // With the session marked active, dispatch reaches the real
    // resolver — but the SecretOverride short-circuits before
    // `op read` would actually run, so we can prove the gate
    // *opens* without depending on a host `op` binary.
    let uri = unique_op_uri("session-active");
    let _override = secret_test::SecretOverride::ok(&uri, "resolved-value");
    let probe = StubPrereqProbe::new().with(
        crate::capability::SessionKind::OnePassword,
        crate::capability::SessionState::Active,
    );
    let src = format!(
        "val token: Secret = secret(\"{uri}\")\n\
             reconcile template(source = \"secret.tpl\", target = \"/secret\", vars = {{\"body\": unwrap_secret(token)}})\n",
    );
    let states = eval_with_probe(&src, &[("secret.tpl", "{{ body }}")], &probe)
        .expect("active session should let resolution proceed");
    let ResourceState::Template { content, .. } = &states[0] else {
        panic!("expected template, got {:?}", states[0]);
    };
    assert_eq!(content, "resolved-value");
}

#[test]
fn secret_unsupported_scheme_is_rejected() {
    let proj = TempProject::new("secret-bad-scheme");
    let src = "val tok: Secret = secret(\"file:///etc/secret\")\n\
                   reconcile template(source = \"tmpl.tpl\", target = unwrap_secret(tok), vars = {\"body\": \"\"})\n";
    let entry = proj.entry(src);
    let canonical = fs::canonicalize(&entry).unwrap();
    let base_dir = canonical.parent().unwrap().to_path_buf();
    let keron_root = base_dir.clone();
    let graph = resolve(vec![EntrySource {
        text: src.to_string(),
        base_dir,
        id: keron_modules::ModuleId(canonical),
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
                     \tsource = \"auth.tpl\",\n\
                     \ttarget = \"/etc/auth\",\n\
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
                 reconcile template(source = \"auth.tpl\", target = \"/etc/auth\", vars = {{\"token\": unwrap_secret(token) + \"-abc\"}})\n",
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
fn ssh_key_intrinsic_threads_secret_into_resource_state() {
    // End-to-end: a stubbed `secret(op://…)` flows directly into
    // `ssh_key(private = …)`. The resulting ResourceState should
    // carry the unwrapped key bytes verbatim with no String
    // round-trip (the planner / executor treat the variant as
    // inherently sensitive).
    let uri = unique_op_uri("sshkey");
    let _g = secret_test::SecretOverride::ok(&uri, "PRIVATE-KEY-BLOB");
    let states = run(&format!(
        "reconcile ssh_key(\n\
             \tprivate_path = \"/home/u/.ssh/id_ed25519\",\n\
             \tpublic_path = \"/home/u/.ssh/id_ed25519.pub\",\n\
             \tprivate = secret(\"{uri}\"),\n\
             \tpublic = \"ssh-ed25519 AAAA u@host\",\n\
             )\n",
    ));
    let ResourceState::SshKey {
        private_path,
        public_path,
        private_key,
        public_key,
    } = &states[0]
    else {
        panic!("expected SshKey, got {:?}", states[0]);
    };
    assert_eq!(private_path, &PathBuf::from("/home/u/.ssh/id_ed25519"));
    assert_eq!(public_path, &PathBuf::from("/home/u/.ssh/id_ed25519.pub"));
    assert_eq!(private_key, "PRIVATE-KEY-BLOB");
    assert_eq!(public_key, "ssh-ed25519 AAAA u@host");
}

#[test]
fn gpg_key_intrinsic_threads_secret_into_resource_state() {
    let uri = unique_op_uri("gpgkey");
    let _g = secret_test::SecretOverride::ok(&uri, "ARMORED-BLOB");
    let states = run(&format!(
        "reconcile gpg_key(fingerprint = \"ABCD1234\", key = secret(\"{uri}\"))\n",
    ));
    let ResourceState::GpgKey { fingerprint, key } = &states[0] else {
        panic!("expected GpgKey, got {:?}", states[0]);
    };
    assert_eq!(fingerprint, "ABCD1234");
    assert_eq!(key, "ARMORED-BLOB");
}

#[test]
fn secret_taint_survives_interpolation() {
    let uri = unique_op_uri("interpolation");
    let _g = secret_test::SecretOverride::ok(&uri, "deploy-key");
    let states = run_with_templates(
        &format!(
            "val token: Secret = secret(\"{uri}\")\n\
                 reconcile template(source = \"auth.tpl\", target = \"/etc/auth\", vars = {{\"token\": \"prefix-${{unwrap_secret(token)}}\"}})\n",
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
             reconcile template(source = \"tmpl.tpl\", target = unwrap_secret(tok), vars = {{\"body\": \"\"}})\n",
    );
    let entry = proj.entry(&src);
    let canonical = fs::canonicalize(&entry).unwrap();
    let base_dir = canonical.parent().unwrap().to_path_buf();
    let keron_root = base_dir.clone();
    let graph = resolve(vec![EntrySource {
        text: src,
        base_dir,
        id: keron_modules::ModuleId(canonical),
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
        std::process::ExitStatus::from_raw(u32::from(!success))
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
    let (env, name) = parse_infisical_uri("infisical://prod/api-key", "prod/api-key").expect("ok");
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
fn parse_infisical_uri_rejects_leading_dash_in_components() {
    for (uri, rest) in [
        ("infisical://-evil/api-key", "-evil/api-key"),
        (
            "infisical://prod/--output-file=/tmp/dump",
            "prod/--output-file=/tmp/dump",
        ),
    ] {
        let err = parse_infisical_uri(uri, rest).expect_err("flag-shaped component");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("must not begin with `-`"),
            "expected flag rejection, got: {msg}",
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
fn parse_bw_uri_rejects_leading_dash_in_components() {
    for (uri, rest) in [
        ("bw://--help", "--help"),
        ("bw://item/-flag", "item/-flag"),
        ("bw://-evil/password", "-evil/password"),
    ] {
        let err = parse_bw_uri(uri, rest).expect_err("flag-shaped component");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("must not begin with `-`"),
            "expected flag rejection, got: {msg}",
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
fn brew_qualified_name_builds_tap_spec_with_user_and_tap() {
    // Pins the happy-path arm of build_tap_spec: a three-segment
    // brew name with non-empty user + tap produces a TapSpec on the
    // resulting Package. Catches the `&& with ||` mutation on the
    // match guard (would allow empty-user / empty-tap inputs to
    // wrongly synthesise a TapSpec).
    let states = run("reconcile brew(\"icepuma/keron/keron\")\n");
    let ResourceState::Package { tap, .. } = &states[0] else {
        panic!("expected Package, got {:?}", states[0]);
    };
    let spec = tap.as_ref().expect("qualified name must produce TapSpec");
    assert_eq!(spec.user_tap, "icepuma/keron");
    assert!(spec.url.is_none());
}

#[test]
fn brew_rejects_empty_user_segment_in_qualified_name() {
    // The match-guard `!user.is_empty() && !tap.is_empty()` is
    // load-bearing: a name like "/tap/formula" splits to ["", "tap",
    // "formula"] and must NOT be accepted as a valid TapSpec. A
    // mutation that flips the guard to `true` or the `&&` to `||`
    // (with an empty user but non-empty tap) would silently build a
    // bogus TapSpec with `user_tap = "/tap"` and either crash the
    // executor or shell out the wrong `brew tap` command.
    let err = run_result_with_templates("reconcile brew(\"/icepuma/keron\")\n", &[])
        .expect_err("empty user segment must bail");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("must be either a bare formula") || msg.contains("one slash or more than two"),
        "expected qualified-name diagnostic, got: {msg}",
    );
}

#[test]
fn brew_qualified_name_with_tap_url_propagates_url_into_tap_spec() {
    // Drives `call_optional_string` along the named-arg branch via
    // the `tap_url = "..."` keyword. The function returns the value
    // of the named arg if its name matches; the `== with !=` mutation
    // on the name comparison would route the lookup to the wrong
    // arg (or fall through to the positional path) and the URL
    // would silently drop off the resulting TapSpec.
    let states = run(
        "reconcile brew(\"icepuma/keron/keron\", tap_url = \"https://github.com/icepuma/keron\")\n",
    );
    let ResourceState::Package { tap, .. } = &states[0] else {
        panic!("expected Package, got {:?}", states[0]);
    };
    let spec = tap.as_ref().expect("qualified name must produce TapSpec");
    assert_eq!(
        spec.url.as_deref(),
        Some("https://github.com/icepuma/keron"),
    );
}

#[test]
fn brew_builds_a_package_resource_with_brew_manager() {
    let states = run("reconcile brew(\"ripgrep\")\n");
    assert_eq!(states.len(), 1);
    let ResourceState::Package { manager, name, .. } = &states[0] else {
        panic!("expected Package, got {:?}", states[0]);
    };
    assert_eq!(*manager, PackageManager::Brew);
    assert_eq!(name, "ripgrep");
}

#[test]
fn cargo_builds_a_package_resource_with_cargo_manager() {
    let states = run("reconcile cargo(\"sccache\")\n");
    let ResourceState::Package { manager, name, .. } = &states[0] else {
        panic!("expected Package, got {:?}", states[0]);
    };
    assert_eq!(*manager, PackageManager::Cargo);
    assert_eq!(name, "sccache");
}

#[test]
fn winget_builds_a_package_resource_with_winget_manager() {
    let states = run("reconcile winget(\"Microsoft.PowerShell\")\n");
    let ResourceState::Package { manager, name, .. } = &states[0] else {
        panic!("expected Package, got {:?}", states[0]);
    };
    assert_eq!(*manager, PackageManager::Winget);
    assert_eq!(name, "Microsoft.PowerShell");
}

#[test]
fn shell_builds_a_shell_resource_with_root_cwd() {
    let (states, root) = run_with_root(
        "reconcile shell(kind = \"sh\", name = \"refresh-font-cache\", script = \"echo one\\necho two\\n\")\n",
    );
    assert_eq!(states.len(), 1);
    let ResourceState::Shell {
        kind,
        name,
        cwd,
        script,
        sensitive,
    } = &states[0]
    else {
        panic!("expected Shell, got {:?}", states[0]);
    };
    assert_eq!(*kind, ShellKind::Sh);
    assert_eq!(name, "refresh-font-cache");
    assert_eq!(cwd, &root);
    assert_eq!(script, "echo one\necho two\n");
    // A plain shell script with no `secret(...)` inputs is not
    // sensitive at the resource layer.
    assert!(!sensitive);
}

#[test]
fn shell_script_with_secret_marks_resource_sensitive() {
    // The shell `script` carrying a sensitive value at the input
    // layer (`Value::String { sensitive: true }`) propagates to
    // the resource's `sensitive` flag. The diff renderer uses
    // that flag to attach a `[sensitive]` hint to the default-mode
    // summary so an operator scanning the plan can see "this body
    // field is going to print secrets if I opt in to verbose."
    // It does NOT redact — verbose mode reveals everything, per
    // `--verbose-will-reveal-sensitive-content`.
    let uri = "op://vault/shell/token";
    let _g = secret_test::SecretOverride::ok(uri, "secret-token");
    let states = run(&format!(
        "val token = secret(\"{uri}\")\n\
             reconcile shell(kind = \"sh\", name = \"with-secret\", script = \"TOKEN=${{unwrap_secret(token)}}\")\n",
    ));
    let ResourceState::Shell {
        script, sensitive, ..
    } = &states[0]
    else {
        panic!("expected Shell, got {:?}", states[0]);
    };
    assert_eq!(script, "TOKEN=secret-token");
    assert!(*sensitive);
}

#[test]
fn shell_name_rejects_sensitive_string() {
    let uri = "op://vault/shell/name";
    let _g = secret_test::SecretOverride::ok(uri, "secret-name");
    let err = run_result_with_templates(
            &format!(
                "val token = secret(\"{uri}\")\n\
                 reconcile shell(kind = \"sh\", name = unwrap_secret(token), script = \"echo ok\")\n",
            ),
            &[],
        )
        .expect_err("sensitive name should fail");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("sensitive String cannot be used for `name`"),
        "got: {msg}"
    );
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
        id: keron_modules::ModuleId(canonical),
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
                 brew(\"ripgrep\")\n\
                 symlink(source = \"./inside\", target = \"/from\")\n\
                 cargo(\"sccache\")\n\
             }\n",
        &[("inside", "")],
    );
    assert_eq!(states.len(), 3);
    assert!(
        matches!(&states[0], ResourceState::Package { manager: PackageManager::Brew, name, .. } if name == "ripgrep"),
    );
    assert!(matches!(&states[1], ResourceState::Symlink { .. }));
    assert!(
        matches!(&states[2], ResourceState::Package { manager: PackageManager::Cargo, name, .. } if name == "sccache"),
    );
}

#[test]
fn symlink_source_relative_path_resolves_inside_keron_root() {
    // `source = "./zshrc"` reads from the entry's directory; the
    // resolved source is canonical and lives inside the keron
    // root, so the executor never sees the raw user string.
    let states = run_with_templates(
        "reconcile symlink(source = \"./zshrc\", target = \"/dest\")\n",
        &[("zshrc", "export PATH=...")],
    );
    let ResourceState::Symlink { to, .. } = &states[0] else {
        panic!("expected Symlink, got {:?}", states[0]);
    };
    assert!(
        to.is_absolute(),
        "source should be canonical: {}",
        to.display()
    );
    let last = to.file_name().unwrap();
    assert_eq!(last, "zshrc");
}

// Containment-rule tests below pin Unix-style absolute paths
// (`/etc/hosts`, `${keron_root()}/...`). On Windows
// `fs::canonicalize` returns a verbatim `\\?\` UNC path in which
// `/` is a literal character, and `/etc/hosts` is not absolute
// (no drive letter), so the assertions don't reflect the same
// semantics. Gate the cluster to unix until we grow Windows-shaped
// companions.
#[cfg(unix)]
#[test]
fn symlink_source_absolute_path_inside_keron_root_is_accepted() {
    // Most user code interpolates `keron_root()` to build the `source`
    // argument; the absolute path it produces must still pass the
    // containment check.
    let proj = TempProject::new("symlink-keron-root");
    proj.seed_template("zshrc", "export PATH=...");
    let src = "reconcile symlink(source = \"${keron_root()}/zshrc\", target = \"/dest\")\n";
    let entry = proj.entry(src);
    let canonical = fs::canonicalize(&entry).unwrap();
    let base_dir = canonical.parent().unwrap().to_path_buf();
    let keron_root = base_dir.clone();
    let graph = resolve(vec![EntrySource {
        text: src.into(),
        base_dir,
        id: keron_modules::ModuleId(canonical),
    }])
    .unwrap_or_else(|errs| panic!("resolve failed: {errs:?}"));
    let states = eval_graph(&graph, &keron_root).unwrap();
    let ResourceState::Symlink { to, .. } = &states[0] else {
        panic!("expected Symlink");
    };
    assert!(
        to.starts_with(&keron_root),
        "source outside root: {}",
        to.display()
    );
}

#[cfg(unix)]
#[test]
fn symlink_source_absolute_path_outside_keron_root_is_rejected() {
    // `/etc/hosts` exists on every test host but is not inside
    // the temp keron root. The diagnostic must name the argument,
    // the user value, and the keron root so the user can see
    // exactly what is being refused and why.
    let proj = TempProject::new("symlink-outside");
    let src = "reconcile symlink(source = \"/etc/hosts\", target = \"/dest\")\n";
    let entry = proj.entry(src);
    let canonical = fs::canonicalize(&entry).unwrap();
    let base_dir = canonical.parent().unwrap().to_path_buf();
    let keron_root = base_dir.clone();
    let graph = resolve(vec![EntrySource {
        text: src.into(),
        base_dir,
        id: keron_modules::ModuleId(canonical),
    }])
    .unwrap_or_else(|errs| panic!("resolve failed: {errs:?}"));
    let err = eval_graph(&graph, &keron_root).expect_err("path outside root must be refused");
    let msg = format!("{err:#}");
    assert!(msg.contains("symlink"), "should name the kind: {msg}");
    assert!(msg.contains("`source`"), "should name the argument: {msg}");
    assert!(
        msg.contains("/etc/hosts"),
        "should echo the user value: {msg}"
    );
    assert!(
        msg.contains("outside the keron root"),
        "should explain why: {msg}",
    );
}

#[cfg(unix)]
#[test]
fn symlink_source_dotdot_escape_is_rejected() {
    // `source = "../escape"` is a relative form that lands outside
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
    let src = "reconcile symlink(source = \"../keron-test-escape.tmp\", target = \"/dest\")\n";
    let entry = proj.entry(src);
    let canonical = fs::canonicalize(&entry).unwrap();
    let base_dir = canonical.parent().unwrap().to_path_buf();
    let keron_root = base_dir.clone();
    let graph = resolve(vec![EntrySource {
        text: src.into(),
        base_dir,
        id: keron_modules::ModuleId(canonical),
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
fn symlink_source_missing_path_errors_with_locating_context() {
    // The path resolves to a file that does not exist; canonicalize
    // fails. The error chain must mention the kind, the argument
    // name, the user-supplied value, and where we looked — that's
    // what makes the diagnostic locatable rather than the bare
    // io::Error.
    let proj = TempProject::new("symlink-missing");
    let src = "reconcile symlink(source = \"./not-there\", target = \"/dest\")\n";
    let entry = proj.entry(src);
    let canonical = fs::canonicalize(&entry).unwrap();
    let base_dir = canonical.parent().unwrap().to_path_buf();
    let keron_root = base_dir.clone();
    let graph = resolve(vec![EntrySource {
        text: src.into(),
        base_dir,
        id: keron_modules::ModuleId(canonical),
    }])
    .unwrap_or_else(|errs| panic!("resolve failed: {errs:?}"));
    let err = eval_graph(&graph, &keron_root).expect_err("missing source must error");
    let msg = format!("{err:#}");
    assert!(msg.contains("symlink"), "kind missing: {msg}");
    assert!(msg.contains("`source`"), "arg name missing: {msg}");
    assert!(msg.contains("not-there"), "value missing: {msg}");
}

#[cfg(unix)]
#[test]
fn symlink_source_that_is_symlink_is_rejected() {
    // `source = "./alias"` where `alias` is itself a symlink would
    // chain indirection. Refuse loudly rather than canonicalize
    // through; the user almost certainly meant to point at the
    // underlying file.
    let proj = TempProject::new("symlink-to-symlink");
    let real = proj.root.join("real.txt");
    fs::write(&real, "hi").unwrap();
    std::os::unix::fs::symlink(&real, proj.root.join("alias")).unwrap();
    let src = "reconcile symlink(source = \"./alias\", target = \"/dest\")\n";
    let entry = proj.entry(src);
    let canonical = fs::canonicalize(&entry).unwrap();
    let base_dir = canonical.parent().unwrap().to_path_buf();
    let keron_root = base_dir.clone();
    let graph = resolve(vec![EntrySource {
        text: src.into(),
        base_dir,
        id: keron_modules::ModuleId(canonical),
    }])
    .unwrap_or_else(|errs| panic!("resolve failed: {errs:?}"));
    let err = eval_graph(&graph, &keron_root).expect_err("symlink-to-symlink must be refused");
    let msg = format!("{err:#}");
    assert!(msg.contains("symlink"), "kind missing: {msg}");
    assert!(msg.contains("`source`"), "arg name missing: {msg}");
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
    let src = "reconcile template(source = \"./alias.tpl\", target = \"/dest\", vars = {})\n";
    let entry = proj.entry(src);
    let canonical = fs::canonicalize(&entry).unwrap();
    let base_dir = canonical.parent().unwrap().to_path_buf();
    let keron_root = base_dir.clone();
    let graph = resolve(vec![EntrySource {
        text: src.into(),
        base_dir,
        id: keron_modules::ModuleId(canonical),
    }])
    .unwrap_or_else(|errs| panic!("resolve failed: {errs:?}"));
    let err = eval_graph(&graph, &keron_root).expect_err("template-from-symlink must be refused");
    let msg = format!("{err:#}");
    assert!(msg.contains("template"), "kind missing: {msg}");
    assert!(msg.contains("`source`"), "arg name missing: {msg}");
    assert!(
        msg.contains("only manages real files"),
        "real-files-only message missing: {msg}",
    );
}

#[cfg(unix)]
#[test]
fn template_source_outside_keron_root_is_rejected() {
    // Same containment rule applies to `template(source = ...)`.
    // An absolute path pointing outside the keron root errors
    // before the file is even read.
    let proj = TempProject::new("template-outside");
    let src = "reconcile template(source = \"/etc/hosts\", target = \"/dest\", vars = {\"body\": \"\"})\n";
    let entry = proj.entry(src);
    let canonical = fs::canonicalize(&entry).unwrap();
    let base_dir = canonical.parent().unwrap().to_path_buf();
    let keron_root = base_dir.clone();
    let graph = resolve(vec![EntrySource {
        text: src.into(),
        base_dir,
        id: keron_modules::ModuleId(canonical),
    }])
    .unwrap_or_else(|errs| panic!("resolve failed: {errs:?}"));
    let err = eval_graph(&graph, &keron_root).expect_err("template outside root must be refused");
    let msg = format!("{err:#}");
    assert!(msg.contains("template"), "kind missing: {msg}");
    assert!(msg.contains("`source`"), "arg name missing: {msg}");
    assert!(
        msg.contains("outside the keron root"),
        "containment message missing: {msg}",
    );
}

/// Capture the rendered target path of `template(... target = X)`
/// so an intrinsic's string return value can be observed end-to-end
/// without leaving the evaluator. Trims any trailing newlines that
/// `trim` itself might leave in place.
fn first_target_path(src: &str) -> String {
    let states = run(src);
    let ResourceState::Template { path, .. } = &states[0] else {
        panic!("expected template, got {:?}", states[0]);
    };
    path.to_string_lossy().into_owned()
}

#[test]
fn promoted_list_element_divides_as_double() {
    // THE case the promotion span table exists for: `[10, 2.5]` types
    // as `List<Double>`, so `first(...)` must yield `10.0`, and the
    // division below must be float division (4.0), not integer
    // division. Without the eval-side coercion, the runtime Int would
    // silently take the `(Div, Int, Int)` path.
    let value = first_target_path(
        r#"val xs = [10, 2.5]
               val head: Double = first(xs) ?? 0.0
               val q: Double = head / 2.5
               reconcile template(source = "tmpl.tpl", target = "q=${q}", vars = {"body": ""})
            "#,
    );
    assert!(value.ends_with("q=4"), "got `{value}`");
}

#[test]
fn promoted_if_branch_yields_double() {
    // Only one branch runs; the coercion applies at the `if` span, so
    // the Int branch arrives as a Double.
    let value = first_target_path(
        r#"val n = if true { 1 } else { 2.5 }
               val q: Double = n / 2
               reconcile template(source = "tmpl.tpl", target = "q=${q}", vars = {"body": ""})
            "#,
    );
    assert!(value.ends_with("q=0.5"), "got `{value}`");
}

#[test]
fn promoted_int_literal_into_double_slot_divides_as_double() {
    let value = first_target_path(
        r#"val x: Double = 1
               val q: Double = x / 2
               reconcile template(source = "tmpl.tpl", target = "q=${q}", vars = {"body": ""})
            "#,
    );
    assert!(value.ends_with("q=0.5"), "got `{value}`");
}

#[test]
fn split_then_join_round_trips_with_the_same_separator() {
    let value = first_target_path(
        "reconcile template(source = \"tmpl.tpl\", \
             target = join(split(\"a:b:c\", \":\"), \":\"), \
             vars = {\"body\": \"\"})\n",
    );
    assert!(
        value.ends_with("a:b:c"),
        "split/join round-trip should preserve content, got `{value}`",
    );
}

#[test]
fn join_with_different_separator_changes_glue_not_pieces() {
    let value = first_target_path(
        "reconcile template(source = \"tmpl.tpl\", \
             target = join(split(\"a:b:c\", \":\"), \"-\"), \
             vars = {\"body\": \"\"})\n",
    );
    assert!(value.ends_with("a-b-c"), "got `{value}`");
}

#[test]
fn contains_returns_true_for_substring_match() {
    let states = run("reconcile template(source = \"tmpl.tpl\", \
             target = if contains(\"/usr/local/bin\", \"local\") { \"yes\" } else { \"no\" }, \
             vars = {\"body\": \"\"})\n");
    let ResourceState::Template { path, .. } = &states[0] else {
        panic!("expected template, got {:?}", states[0]);
    };
    assert!(path.ends_with("yes"), "got `{}`", path.display());
}

#[test]
fn replace_swaps_every_occurrence_of_a_fixed_string() {
    let value = first_target_path(
        "reconcile template(source = \"tmpl.tpl\", \
             target = replace(\"a-b-c-d\", \"-\", \"_\"), \
             vars = {\"body\": \"\"})\n",
    );
    assert!(value.ends_with("a_b_c_d"), "got `{value}`");
}

#[test]
fn trim_drops_surrounding_whitespace_but_keeps_inner() {
    let value = first_target_path(
        "reconcile template(source = \"tmpl.tpl\", \
             target = trim(\"   hello world   \"), \
             vars = {\"body\": \"\"})\n",
    );
    assert!(value.ends_with("hello world"), "got `{value}`");
}

#[test]
fn split_rejects_empty_separator() {
    let res = run_result_with_templates(
        "reconcile template(source = \"tmpl.tpl\", \
             target = join(split(\"abc\", \"\"), \"\"), \
             vars = {\"body\": \"\"})\n",
        &[],
    );
    let err = res.expect_err("empty separator must error");
    let msg = format!("{err:#}");
    assert!(msg.contains("`sep` must not be empty"), "got: {msg}");
}

#[test]
fn join_with_one_sensitive_element_marks_whole_result_sensitive() {
    // Pins the `sensitive |= si` accumulator inside dispatch_join:
    // joining any list that contains a sensitive String must
    // produce a sensitive String. A `|= -> &= ` mutation would
    // require *every* element to be sensitive (`false & true = false`)
    // and the result would render unredacted in default-mode diffs.
    let uri = "op://vault/test/join";
    let _g = secret_test::SecretOverride::ok(uri, "secret-suffix");
    let states = run_with_templates(
        &format!(
            "val token: Secret = secret(\"{uri}\")\n\
                 val plain: String = \"plain\"\n\
                 val joined: String = join([plain, unwrap_secret(token)], \"-\")\n\
                 reconcile template(source = \"tmpl.tpl\", target = \"/joined\", vars = {{\"body\": joined}})\n",
        ),
        &[],
    );
    let ResourceState::Template {
        content, sensitive, ..
    } = &states[0]
    else {
        panic!("expected template, got {:?}", states[0]);
    };
    assert_eq!(content, "plain-secret-suffix");
    assert!(
        *sensitive,
        "join must promote a partially-sensitive list to a sensitive String",
    );
}

#[test]
fn replace_propagates_sensitivity_from_replacement_string() {
    // Pins the `s.sensitive || to.sensitive` arm in dispatch_replace.
    // The source string is plain, the replacement is sensitive: the
    // result MUST be sensitive — the post-replace text now embeds
    // the secret. A `|| -> &&` mutation would require BOTH to be
    // sensitive and the result would silently drop its sensitive
    // flag.
    let uri = "op://vault/test/replace";
    let _g = secret_test::SecretOverride::ok(uri, "secret-token");
    let states = run_with_templates(
        &format!(
            "val token: Secret = secret(\"{uri}\")\n\
                 val plain: String = \"hello WORLD\"\n\
                 val out: String = replace(plain, \"WORLD\", unwrap_secret(token))\n\
                 reconcile template(source = \"tmpl.tpl\", target = \"/replaced\", vars = {{\"body\": out}})\n",
        ),
        &[],
    );
    let ResourceState::Template {
        content, sensitive, ..
    } = &states[0]
    else {
        panic!("expected template, got {:?}", states[0]);
    };
    assert_eq!(content, "hello secret-token");
    assert!(
        *sensitive,
        "replace must propagate sensitivity from the `to` argument",
    );
}

#[test]
fn replace_rejects_empty_from() {
    let res = run_result_with_templates(
        "reconcile template(source = \"tmpl.tpl\", \
             target = replace(\"abc\", \"\", \"_\"), \
             vars = {\"body\": \"\"})\n",
        &[],
    );
    let err = res.expect_err("empty `from` must error");
    let msg = format!("{err:#}");
    assert!(msg.contains("`from` must not be empty"), "got: {msg}");
}

#[test]
fn hostname_returns_a_non_empty_string() {
    // The host's name varies, but `gethostname` should yield
    // *something* on every supported platform — verify the result
    // is at least a non-empty path component.
    let value = first_target_path(
        "reconcile template(source = \"tmpl.tpl\", \
             target = hostname(), \
             vars = {\"body\": \"\"})\n",
    );
    let last = std::path::Path::new(&value).file_name().unwrap_or_default();
    assert!(!last.is_empty(), "hostname() was empty: `{value}`");
}

#[test]
fn len_intrinsic_counts_list_elements() {
    // Raw string so nested keron `"..."` literals inside the
    // interpolation don't need backslash-escaping — keron rejects
    // `\"` inside expression position (it's only legal inside a
    // keron string literal), and double-escaping through Rust
    // string syntax makes the source unreadable.
    let value = first_target_path(
        r#"val n: Int = len(split("a:b:c:d", ":"))
               reconcile template(source = "tmpl.tpl", target = "n=${n}", vars = {"body": ""})
            "#,
    );
    assert!(value.ends_with("n=4"), "got `{value}`");
}

#[test]
fn contains_list_returns_true_for_present_element() {
    let value = first_target_path(
        "reconcile template(source = \"tmpl.tpl\", \
             target = if contains(split(\"a:b:c\", \":\"), \"b\") { \"hit\" } else { \"miss\" }, \
             vars = {\"body\": \"\"})\n",
    );
    assert!(value.ends_with("hit"), "got `{value}`");
}

#[test]
fn contains_list_returns_false_for_absent_element() {
    let value = first_target_path(
        "reconcile template(source = \"tmpl.tpl\", \
             target = if contains(split(\"a:b:c\", \":\"), \"z\") { \"hit\" } else { \"miss\" }, \
             vars = {\"body\": \"\"})\n",
    );
    assert!(value.ends_with("miss"), "got `{value}`");
}

#[test]
fn first_and_last_return_null_for_empty_list_through_coalesce() {
    // `??` and pattern resolution are exercised in earlier tests;
    // here we just need to confirm `first` / `last` return `Null`
    // for an empty list. The string-literal escape soup gets
    // unmanageable if we inline interpolations + nested coalesce,
    // so each step is parked in its own `val`.
    let value = first_target_path(
        "val xs: List<String> = []\n\
             val head: String = first(xs) ?? \"empty-first\"\n\
             val tail: String = last(xs) ?? \"empty-last\"\n\
             reconcile template(source = \"tmpl.tpl\", \
             target = \"${head}/${tail}\", \
             vars = {\"body\": \"\"})\n",
    );
    assert!(value.ends_with("empty-first/empty-last"), "got `{value}`");
}

#[test]
fn first_and_last_pick_correct_endpoints_for_non_empty_list() {
    let value = first_target_path(
        "val parts: List<String> = split(\"a:b:c\", \":\")\n\
             val head: String = first(parts) ?? \"\"\n\
             val tail: String = last(parts) ?? \"\"\n\
             reconcile template(source = \"tmpl.tpl\", \
             target = \"${head}/${tail}\", \
             vars = {\"body\": \"\"})\n",
    );
    assert!(value.ends_with("a/c"), "got `{value}`");
}

#[test]
fn map_get_returns_bound_value_when_key_exists() {
    let value = first_target_path(
        "val m: Map<String, String> = {\"k\": \"hit\"}\n\
             reconcile template(source = \"tmpl.tpl\", \
             target = get(m, \"k\", \"miss\"), \
             vars = {\"body\": \"\"})\n",
    );
    assert!(value.ends_with("hit"), "got `{value}`");
}

#[test]
fn map_get_falls_back_to_default_when_key_absent() {
    let value = first_target_path(
        "val m: Map<String, String> = {\"k\": \"hit\"}\n\
             reconcile template(source = \"tmpl.tpl\", \
             target = get(m, \"missing\", \"fallback\"), \
             vars = {\"body\": \"\"})\n",
    );
    assert!(value.ends_with("fallback"), "got `{value}`");
}

#[test]
fn map_keys_and_values_return_declared_order() {
    let value = first_target_path(
        r#"val m: Map<String, String> = {"a": "1", "b": "2"}
               val ks: String = join(keys(m), ",")
               val vs: String = join(values(m), ",")
               reconcile template(source = "tmpl.tpl", target = "${ks}-${vs}", vars = {"body": ""})
            "#,
    );
    assert!(value.ends_with("a,b-1,2"), "got `{value}`");
}

#[test]
fn contains_map_distinguishes_present_and_absent_keys() {
    let value = first_target_path(
        r#"val m: Map<String, String> = {"a": "1"}
               val present: Boolean = contains(m, "a")
               val absent: Boolean = contains(m, "z")
               reconcile template(source = "tmpl.tpl", target = "${present}-${absent}", vars = {"body": ""})
            "#,
    );
    assert!(value.ends_with("true-false"), "got `{value}`");
}

// `path_join` delegates to `PathBuf::join` which uses the platform
// separator. The literal `/a/b` expectation is Unix-shaped; on
// Windows the same call would produce `/a\b`. Gate to unix.
#[cfg(unix)]
#[test]
fn path_join_appends_a_relative_segment_with_a_separator() {
    let value = first_target_path(
        r#"val out: String = path_join("/a", "b")
               reconcile template(source = "tmpl.tpl", target = out, vars = {"body": ""})
            "#,
    );
    assert_eq!(value, "/a/b");
}

#[cfg(unix)]
#[test]
fn path_join_replaces_when_segment_is_absolute() {
    // Matches `PathBuf::join`: an absolute `segment` discards the
    // base. Documenting the behaviour pins it against a regression
    // that would silently glue two absolute paths.
    let value = first_target_path(
        r#"val out: String = path_join("/a", "/b")
               reconcile template(source = "tmpl.tpl", target = out, vars = {"body": ""})
            "#,
    );
    assert_eq!(value, "/b");
}

#[test]
fn path_parent_returns_null_for_root_through_coalesce() {
    let value = first_target_path(
        r#"val out: String = path_parent("/") ?? "<root>"
               reconcile template(source = "tmpl.tpl", target = out, vars = {"body": ""})
            "#,
    );
    assert!(value.ends_with("<root>"), "got `{value}`");
}

#[test]
fn path_parent_strips_the_final_component() {
    let value = first_target_path(
        r#"val out: String = path_parent("/a/b/c.txt") ?? ""
               reconcile template(source = "tmpl.tpl", target = out, vars = {"body": ""})
            "#,
    );
    assert_eq!(value, "/a/b");
}

#[test]
fn path_basename_and_extension_split_the_final_component() {
    let value = first_target_path(
        r#"val name: String = path_basename("/a/b/c.txt") ?? "none"
               val ext: String = path_extension("/a/b/c.txt") ?? "none"
               reconcile template(source = "tmpl.tpl", target = "${name}|${ext}", vars = {"body": ""})
            "#,
    );
    assert!(value.ends_with("c.txt|txt"), "got `{value}`");
}

// `path_is_absolute` delegates to `Path::is_absolute`. On Windows
// `/a/b` is "rooted but not absolute" (absolute needs a drive
// letter like `C:\`), so the literal would be `false|false` there.
// Gate to unix; a Windows-specific variant can be added later if
// we want to pin Windows semantics too.
#[cfg(unix)]
#[test]
fn path_is_absolute_distinguishes_absolute_and_relative() {
    let value = first_target_path(
        r#"val abs: Boolean = path_is_absolute("/a/b")
               val rel: Boolean = path_is_absolute("a/b")
               reconcile template(source = "tmpl.tpl", target = "${abs}|${rel}", vars = {"body": ""})
            "#,
    );
    assert!(value.ends_with("true|false"), "got `{value}`");
}

#[test]
fn path_exists_returns_true_for_the_keron_root_and_false_for_missing_paths() {
    let value = first_target_path(
        r#"val here: Boolean = path_exists(keron_root())
               val gone: Boolean = path_exists("/this/should/not/exist-xyz-keron-test")
               reconcile template(source = "tmpl.tpl", target = "${here}|${gone}", vars = {"body": ""})
            "#,
    );
    assert!(value.ends_with("true|false"), "got `{value}`");
}

#[test]
fn path_probes_can_observe_existing_files_outside_keron_root() {
    let proj = TempProject::new("path-probe-outside-root");
    let outside = env::temp_dir().join(format!(
        "keron-path-probe-outside-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, AtomicOrdering::Relaxed),
    ));
    fs::write(&outside, "outside").expect("seed outside file");
    let escaped = outside.to_string_lossy().replace('\\', "\\\\");
    let src = format!(
        r#"val inside: Boolean = path_exists("tmpl.tpl")
               val outside: Boolean = path_exists("{escaped}")
               reconcile template(source = "tmpl.tpl", target = "${{inside}}|${{outside}}", vars = {{"body": ""}})
            "#
    );
    let entry = proj.entry(&src);
    let canonical = fs::canonicalize(&entry).unwrap();
    let base_dir = canonical.parent().unwrap().to_path_buf();
    let graph = resolve(vec![EntrySource {
        text: src,
        base_dir: base_dir.clone(),
        id: keron_modules::ModuleId(canonical),
    }])
    .unwrap_or_else(|errs| panic!("resolve failed: {errs:?}"));
    let states = eval_graph(&graph, &base_dir).unwrap_or_else(|e| panic!("eval failed: {e}"));
    let _ = fs::remove_file(&outside);
    let ResourceState::Template { path, .. } = &states[0] else {
        panic!("expected template, got {:?}", states[0]);
    };
    assert!(path.to_string_lossy().ends_with("true|true"));
}

#[test]
fn path_is_dir_and_is_file_route_metadata_to_the_right_predicate() {
    // The keron root is itself a directory (every test harness
    // sets it that way); the templated file we just wrote in
    // earlier tests would be a regular file. We only assert the
    // directory side here because the file-side input would need
    // disk setup beyond the test scaffold.
    let value = first_target_path(
        r#"val dir: Boolean = path_is_dir(keron_root())
               val file: Boolean = path_is_file(keron_root())
               reconcile template(source = "tmpl.tpl", target = "${dir}|${file}", vars = {"body": ""})
            "#,
    );
    assert!(value.ends_with("true|false"), "got `{value}`");
}

#[test]
fn home_dir_matches_dirs_home_dir_at_eval_time() {
    let value = first_target_path(
        "reconcile template(source = \"tmpl.tpl\", \
             target = home_dir(), \
             vars = {\"body\": \"\"})\n",
    );
    let expected = dirs::home_dir()
        .expect("dirs::home_dir must resolve on this host")
        .to_string_lossy()
        .into_owned();
    assert_eq!(value, expected);
}

/// `read_file` is the only round-2 stdlib intrinsic that touches
/// arbitrary user-supplied paths, and the keron-root containment
/// guard is the load-bearing security property. The four tests
/// below pin every leg of that guard: in-root success, missing
/// file null, absolute-outside-root null, and `..` escape null.
/// Drop fixtures via the existing `run_with_templates` seam
/// (which writes to `root/<name>`) so the file lives at a path
/// inside the canonicalized keron root the evaluator sees.
#[test]
fn read_file_returns_contents_for_file_inside_keron_root() {
    let states = run_with_templates(
        r#"val s: String = read_file("./snippet.txt") ?? "<missing>"
               reconcile template(source = "tmpl.tpl", target = s, vars = {"body": ""})
            "#,
        &[("snippet.txt", "INSIDE-ROOT")],
    );
    let ResourceState::Template { path, .. } = &states[0] else {
        panic!("expected template, got {:?}", states[0]);
    };
    assert!(
        path.ends_with("INSIDE-ROOT"),
        "expected contents `INSIDE-ROOT`, got `{}`",
        path.display(),
    );
}

#[test]
fn read_file_returns_null_when_file_is_missing() {
    // No `snippet.txt` seeded — `resolve_managed_path` errors on
    // `symlink_metadata`, which collapses to `null` and the
    // fallback string takes over.
    let value = first_target_path(
        r#"val s: String = read_file("./does-not-exist") ?? "MISSING"
               reconcile template(source = "tmpl.tpl", target = s, vars = {"body": ""})
            "#,
    );
    assert!(value.ends_with("MISSING"), "got `{value}`");
}

#[cfg(unix)]
#[test]
fn read_file_returns_null_for_absolute_path_outside_keron_root() {
    // `/etc/passwd` exists on every supported host but lives
    // outside the temp keron root, so containment must reject it
    // and the fallback string must win. If this ever returns the
    // real file's contents the security guard has regressed.
    let value = first_target_path(
        r#"val s: String = read_file("/etc/passwd") ?? "BLOCKED"
               reconcile template(source = "tmpl.tpl", target = s, vars = {"body": ""})
            "#,
    );
    assert!(value.ends_with("BLOCKED"), "got `{value}`");
}

#[test]
fn read_file_returns_null_for_dotdot_escape_outside_keron_root() {
    // `..` ascends past the temp keron root; canonicalize +
    // `starts_with` rejects, collapsing to null. Pins the
    // canonical-form leg of the containment check independently
    // of the absolute-path leg above.
    let value = first_target_path(
        r#"val s: String = read_file("../../../../etc/passwd") ?? "ESCAPED"
               reconcile template(source = "tmpl.tpl", target = s, vars = {"body": ""})
            "#,
    );
    assert!(value.ends_with("ESCAPED"), "got `{value}`");
}

#[test]
fn sort_orders_strings_ascending() {
    let value = first_target_path(
        r#"val out: String = join(sort(["c", "a", "b"]), ",")
               reconcile template(source = "tmpl.tpl", target = out, vars = {"body": ""})
            "#,
    );
    assert!(value.ends_with("a,b,c"), "got `{value}`");
}

#[test]
fn sort_orders_ints_numerically() {
    // Generic `sort` routes through `value_cmp`, so Ints order
    // numerically (10 after 2), not lexicographically.
    let value = first_target_path(
        r#"val nums: List<Int> = sort([10, 2, 1])
               val out: String = "${first(nums) ?? 0}-${last(nums) ?? 0}"
               reconcile template(source = "tmpl.tpl", target = out, vars = {"body": ""})
            "#,
    );
    assert!(value.ends_with("1-10"), "got `{value}`");
}

#[test]
fn unique_preserves_first_occurrence_order() {
    let value = first_target_path(
        r#"val out: String = join(unique(["a", "b", "a", "c", "b"]), ",")
               reconcile template(source = "tmpl.tpl", target = out, vars = {"body": ""})
            "#,
    );
    assert!(value.ends_with("a,b,c"), "got `{value}`");
}

#[test]
fn index_of_returns_position_when_present() {
    let value = first_target_path(
        r#"val xs: List<String> = ["a", "b", "c"]
               val i: Int = index_of(xs, "b") ?? -1
               reconcile template(source = "tmpl.tpl", target = "${i}", vars = {"body": ""})
            "#,
    );
    assert!(value.ends_with('1'), "got `{value}`");
}

#[test]
fn index_of_returns_null_when_absent_routes_to_fallback() {
    let value = first_target_path(
        r#"val xs: List<String> = ["a", "b"]
               val i: Int = index_of(xs, "z") ?? -1
               reconcile template(source = "tmpl.tpl", target = "${i}", vars = {"body": ""})
            "#,
    );
    assert!(value.ends_with("-1"), "got `{value}`");
}

#[test]
fn merge_overlays_right_over_left_and_preserves_order() {
    let value = first_target_path(
        r#"val a: Map<String, String> = {"x": "1", "y": "2"}
               val b: Map<String, String> = {"y": "9", "z": "3"}
               val m: Map<String, String> = merge(a, b)
               val ks: String = join(keys(m), ",")
               val vs: String = join(values(m), ",")
               reconcile template(source = "tmpl.tpl", target = "${ks}:${vs}", vars = {"body": ""})
            "#,
    );
    assert!(value.ends_with("x,y,z:1,9,3"), "got `{value}`");
}

#[test]
fn without_drops_named_key_only() {
    let value = first_target_path(
        r#"val m: Map<String, String> = {"a": "1", "b": "2", "c": "3"}
               val out: Map<String, String> = without(m, "b")
               reconcile template(source = "tmpl.tpl", target = join(keys(out), ","), vars = {"body": ""})
            "#,
    );
    assert!(value.ends_with("a,c"), "got `{value}`");
}

#[test]
fn with_upserts_preserving_existing_key_position() {
    let value = first_target_path(
        r#"val m: Map<String, String> = {"a": "1", "b": "2"}
               val out: Map<String, String> = with(m, "a", "9")
               val ks: String = join(keys(out), ",")
               val vs: String = join(values(out), ",")
               reconcile template(source = "tmpl.tpl", target = "${ks}:${vs}", vars = {"body": ""})
            "#,
    );
    assert!(value.ends_with("a,b:9,2"), "got `{value}`");
}

#[test]
fn with_appends_new_keys_at_end() {
    let value = first_target_path(
        r#"val m: Map<String, String> = {"a": "1"}
               val out: Map<String, String> = with(m, "z", "9")
               reconcile template(source = "tmpl.tpl", target = join(keys(out), ","), vars = {"body": ""})
            "#,
    );
    assert!(value.ends_with("a,z"), "got `{value}`");
}

#[test]
fn parse_int_returns_value_for_valid_input() {
    let value = first_target_path(
        r#"val n: Int = parse_int("42") ?? -1
               reconcile template(source = "tmpl.tpl", target = "${n}", vars = {"body": ""})
            "#,
    );
    assert!(value.ends_with("42"), "got `{value}`");
}

#[test]
fn parse_int_returns_null_for_malformed_input_routes_to_fallback() {
    let value = first_target_path(
        r#"val n: Int = parse_int("not-a-number") ?? -1
               reconcile template(source = "tmpl.tpl", target = "${n}", vars = {"body": ""})
            "#,
    );
    assert!(value.ends_with("-1"), "got `{value}`");
}

#[test]
fn parse_double_rejects_non_finite_input() {
    // `"inf"` parses as `f64::INFINITY` via `str::parse`, but the
    // dispatch screens non-finite values to `null` so the rest of
    // the language can rely on Double arithmetic.
    let value = first_target_path(
        r#"val d: Double = parse_double("inf") ?? 7.0
               reconcile template(source = "tmpl.tpl", target = "${d}", vars = {"body": ""})
            "#,
    );
    assert!(value.ends_with('7'), "got `{value}`");
}

#[test]
fn struct_field_default_fills_in_when_arg_is_omitted() {
    // Positional construction supplies only the required field;
    // the two defaulted fields fill in via their declared
    // expressions. Routing the values into a template var pins
    // both the eval path and that defaults are evaluated in the
    // outer env (concat is a constant expression here).
    let value = first_target_path(
        r#"struct Server {
                 host: String,
                 port: Int = 8080,
                 protocol: String = "https" + ""
               }
               val s: Server = Server { host: "api.example.com" }
               reconcile template(
                 source = "tmpl.tpl",
                 target = "${s.host}:${s.port}/${s.protocol}",
                 vars = {"body": ""},
               )
            "#,
    );
    assert!(
        value.ends_with("api.example.com:8080/https"),
        "got `{value}`",
    );
}

#[test]
fn struct_field_default_yields_to_explicit_named_arg() {
    // When the caller names the defaulted field, the default is
    // bypassed entirely — no shadowing, no merge surprises.
    let value = first_target_path(
        r#"struct Server { host: String, port: Int = 8080 }
               val s: Server = Server { host: "api", port: 443 }
               reconcile template(
                 source = "tmpl.tpl",
                 target = "${s.host}:${s.port}",
                 vars = {"body": ""},
               )
            "#,
    );
    assert!(value.ends_with("api:443"), "got `{value}`");
}

#[test]
fn imported_struct_field_default_uses_defining_module_scope() {
    let proj = TempProject::new("imported-struct-default");
    fs::write(
        proj.root.join("defs.keron"),
        r"val default_port: Int = 8080
               struct Server { host: String, port: Int = default_port }
            ",
    )
    .expect("write defs module");
    let src = r#"from "./defs.keron" use Server
                    val default_port: Int = 443
                    val s: Server = Server { host: "api" }
                    reconcile template(source = "tmpl.tpl", target = "${s.host}:${s.port}", vars = {"body": ""})
        "#;
    let entry = proj.entry(src);
    let canonical = fs::canonicalize(&entry).unwrap();
    let base_dir = canonical.parent().unwrap().to_path_buf();
    let graph = resolve(vec![EntrySource {
        text: src.to_string(),
        base_dir: base_dir.clone(),
        id: keron_modules::ModuleId(canonical),
    }])
    .unwrap_or_else(|errs| panic!("resolve failed: {errs:?}"));
    let states = eval_graph(&graph, &base_dir).unwrap_or_else(|e| panic!("eval failed: {e}"));
    let ResourceState::Template { path, .. } = &states[0] else {
        panic!("expected template, got {:?}", states[0]);
    };
    assert!(path.to_string_lossy().ends_with("api:8080"));
}
