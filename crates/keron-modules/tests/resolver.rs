//! Integration tests for the module resolver.
//!
//! Each test sets up a small on-disk project under a per-test temp
//! directory, runs `resolve(...)`, and asserts on the resulting graph
//! or errors.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use keron_modules::{EntrySource, ModuleId, resolve};

static COUNTER: AtomicUsize = AtomicUsize::new(0);

struct TempProject {
    root: PathBuf,
}

impl TempProject {
    fn new(name: &str) -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let root = env::temp_dir().join(format!(
            "keron-modules-test-{name}-{}-{n}",
            std::process::id()
        ));
        if root.exists() {
            fs::remove_dir_all(&root).expect("clean temp dir");
        }
        fs::create_dir_all(&root).expect("create temp dir");
        Self { root }
    }

    fn write(&self, rel: &str, content: &str) -> PathBuf {
        let path = self.root.join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create parent dir");
        }
        fs::write(&path, content).expect("write file");
        path
    }

    fn entry_source(file: &Path, content: &str) -> Vec<EntrySource> {
        let canonical = fs::canonicalize(file).expect("canonicalize entry");
        let base_dir = canonical.parent().expect("entry has parent").to_path_buf();
        vec![EntrySource {
            text: content.to_string(),
            base_dir,
            id: ModuleId(canonical),
        }]
    }

    /// Build a multi-root [`EntrySource`] list from already-written
    /// files. Each `(file, content)` pair becomes one root; the
    /// loader's recursive discovery is bypassed so tests can pin the
    /// exact set of roots they care about.
    fn roots(files: &[(&Path, &str)]) -> Vec<EntrySource> {
        files
            .iter()
            .map(|(f, src)| {
                let canonical = fs::canonicalize(f).expect("canonicalize root");
                let base_dir = canonical.parent().expect("root has parent").to_path_buf();
                EntrySource {
                    text: (*src).to_string(),
                    base_dir,
                    id: ModuleId(canonical),
                }
            })
            .collect()
    }
}

impl Drop for TempProject {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn builtins_are_implicitly_in_scope() {
    // No `from "std:..."` line — `symlink` and `Symlink` resolve via
    // the builtin registry. Single-module graph: just the entry.
    let proj = TempProject::new("builtins-implicit");
    let src = "val s: Symlink = symlink(source = \"b\", target = \"a\")\n\
               reconcile s\n";
    let entry = proj.write("entry.keron", src);
    let graph = resolve(TempProject::entry_source(&entry, src)).expect("resolve ok");
    let entry_id = ModuleId(fs::canonicalize(&entry).unwrap());
    assert_eq!(graph.entries, vec![entry_id]);
    assert_eq!(graph.modules.len(), 1, "stdlib does not enter the graph");
}

#[test]
fn ssh_key_and_gpg_key_resolve_as_implicit_builtins() {
    // Mirrors `builtins_are_implicitly_in_scope` for the keys module:
    // a manifest using `ssh_key(...)` / `gpg_key(...)` and the
    // `SshKey` / `GpgKey` named types resolves without any explicit
    // `from "std:..."` line.
    let proj = TempProject::new("keys-implicit");
    let src = "val k: SshKey = ssh_key(\n\
               \tprivate_path = \"/p\",\n\
               \tpublic_path = \"/p.pub\",\n\
               \tprivate = secret(\"op://k/test\"),\n\
               \tpublic = \"ssh-ed25519 AAAA u@h\",\n\
               )\n\
               val g: GpgKey = gpg_key(fingerprint = \"ABCD\", key = secret(\"op://k/gpg\"))\n\
               reconcile k\n\
               reconcile g\n";
    let entry = proj.write("entry.keron", src);
    let graph = resolve(TempProject::entry_source(&entry, src)).expect("resolve ok");
    let entry_id = ModuleId(fs::canonicalize(&entry).unwrap());
    assert_eq!(graph.entries, vec![entry_id]);
    assert_eq!(graph.modules.len(), 1, "stdlib does not enter the graph");
}

#[test]
fn invalid_path_prefix_errors() {
    let proj = TempProject::new("bad-prefix");
    let src = "from \"helpers.keron\" use foo\n";
    let entry = proj.write("entry.keron", src);
    let bundle = resolve(TempProject::entry_source(&entry, src)).expect_err("should fail");
    assert!(
        bundle
            .errors
            .iter()
            .flat_map(|e| &e.diagnostics)
            .any(|d| d.message.contains("must start with"))
    );
}

#[test]
fn cross_file_import_resolves() {
    let proj = TempProject::new("cross-file");
    let helpers_src = "fn link(name: String): Symlink {\n\
                       \tsymlink(source = name, target = name)\n\
                       }\n";
    proj.write("helpers.keron", helpers_src);
    let entry_src = "from \"./helpers.keron\" use link\n\
                     val s: Symlink = link(\"x\")\n\
                     reconcile s\n";
    let entry = proj.write("entry.keron", entry_src);
    let graph = resolve(TempProject::entry_source(&entry, entry_src)).expect("resolve ok");
    // entry + helpers — stdlib does not enter the graph.
    assert_eq!(graph.modules.len(), 2);
}

#[test]
fn missing_export_errors() {
    let proj = TempProject::new("missing-export");
    let helpers_src = "fn other(): Int { 1 }\n";
    proj.write("helpers.keron", helpers_src);
    let entry_src = "from \"./helpers.keron\" use missing\n\
                     val n: Int = missing()\n";
    let entry = proj.write("entry.keron", entry_src);
    let bundle = resolve(TempProject::entry_source(&entry, entry_src)).expect_err("should fail");
    assert!(
        bundle
            .errors
            .iter()
            .flat_map(|e| &e.diagnostics)
            .any(|d| d.message.contains("does not export `missing`"))
    );
}

#[test]
fn importing_unannotated_val_errors_clearly() {
    let proj = TempProject::new("unannotated-val-export");
    let helpers_src = "val answer = 42\n";
    proj.write("helpers.keron", helpers_src);
    let entry_src = "from \"./helpers.keron\" use answer\n\
                     val n: Int = answer\n";
    let entry = proj.write("entry.keron", entry_src);
    let bundle = resolve(TempProject::entry_source(&entry, entry_src)).expect_err("should fail");
    let messages: Vec<_> = bundle
        .errors
        .iter()
        .flat_map(|e| &e.diagnostics)
        .map(|d| d.message.as_str())
        .collect();
    assert!(
        messages
            .iter()
            .any(|m| m.contains("explicit type annotation")),
        "got: {messages:?}",
    );
}

#[test]
fn cycle_errors() {
    let proj = TempProject::new("cycle");
    proj.write(
        "a.keron",
        "from \"./b.keron\" use bar\nfn foo(): Int { bar() }\n",
    );
    proj.write(
        "b.keron",
        "from \"./a.keron\" use foo\nfn bar(): Int { foo() }\n",
    );
    let entry_src = "from \"./a.keron\" use foo\nval n: Int = foo()\n";
    let entry = proj.write("entry.keron", entry_src);
    let bundle = resolve(TempProject::entry_source(&entry, entry_src)).expect_err("should fail");
    assert!(
        bundle
            .errors
            .iter()
            .flat_map(|e| &e.diagnostics)
            .any(|d| d.message.contains("module cycle"))
    );
}

#[test]
fn user_fn_collides_with_builtin() {
    // A user `fn symlink(...)` shadowing a builtin should report the
    // dedicated "builtin and cannot be redefined" diagnostic, not the
    // generic "already defined" message used for user-vs-user collisions.
    let proj = TempProject::new("dup-builtin");
    let src = "fn symlink(source: String, target: String): Symlink {\n\
               \tsymlink(source = source, target = target)\n\
               }\n";
    let entry = proj.write("entry.keron", src);
    let bundle = resolve(TempProject::entry_source(&entry, src)).expect_err("should fail");
    assert!(
        bundle.errors.iter().flat_map(|e| &e.diagnostics).any(|d| d
            .message
            .contains("`symlink` is a builtin and cannot be redefined")),
        "got: {:?}",
        bundle
            .errors
            .iter()
            .flat_map(|e| &e.diagnostics)
            .map(|d| &d.message)
            .collect::<Vec<_>>(),
    );
}

#[test]
fn user_fn_collides_with_user_import_uses_generic_message() {
    // Cross-check: the "is already defined" generic message survives
    // for user-vs-user collisions (so we know the new builtin message
    // didn't subsume that path).
    let proj = TempProject::new("dup-user");
    let helpers_src = "fn helper(): Int { 1 }\n";
    proj.write("helpers.keron", helpers_src);
    let entry_src = "from \"./helpers.keron\" use helper\n\
                     fn helper(): Int { 2 }\n";
    let entry = proj.write("entry.keron", entry_src);
    let bundle = resolve(TempProject::entry_source(&entry, entry_src)).expect_err("should fail");
    assert!(
        bundle
            .errors
            .iter()
            .flat_map(|e| &e.diagnostics)
            .any(|d| d.message.contains("`helper` is already defined")),
        "got: {:?}",
        bundle
            .errors
            .iter()
            .flat_map(|e| &e.diagnostics)
            .map(|d| &d.message)
            .collect::<Vec<_>>(),
    );
}

#[test]
fn imported_val_with_annotation_resolves() {
    let proj = TempProject::new("import-val");
    let helpers_src = "val greeting: String = \"hi\"\n";
    proj.write("helpers.keron", helpers_src);
    let entry_src = "from \"./helpers.keron\" use greeting\n\
                     val msg: String = greeting\n";
    let entry = proj.write("entry.keron", entry_src);
    let graph = resolve(TempProject::entry_source(&entry, entry_src)).expect("resolve ok");
    let entry_id = ModuleId(fs::canonicalize(&entry).unwrap());
    let entry_mod = graph.modules.get(&entry_id).expect("entry module present");
    let (origin, original) = entry_mod
        .imports
        .get("greeting")
        .expect("greeting in imports");
    assert_eq!(original, "greeting");
    let helpers_id = ModuleId(fs::canonicalize(proj.root.join("helpers.keron")).unwrap());
    assert_eq!(origin, &helpers_id);
}

#[test]
fn cycle_path_starts_and_ends_at_same_module() {
    let proj = TempProject::new("cycle-path");
    proj.write(
        "a.keron",
        "from \"./b.keron\" use bar\nfn foo(): Int { bar() }\n",
    );
    proj.write(
        "b.keron",
        "from \"./a.keron\" use foo\nfn bar(): Int { foo() }\n",
    );
    let entry_src = "from \"./a.keron\" use foo\nval n: Int = foo()\n";
    let entry = proj.write("entry.keron", entry_src);
    let bundle = resolve(TempProject::entry_source(&entry, entry_src)).expect_err("should fail");
    let cycle_msg = bundle
        .errors
        .iter()
        .flat_map(|e| &e.diagnostics)
        .find(|d| d.message.contains("module cycle"))
        .expect("cycle diagnostic");
    // The rendered path lists each step; the first step must reappear
    // as the last one — that's what makes it a cycle. If the start
    // index in `topo_sort` were wrong, this invariant would break.
    let after = cycle_msg
        .message
        .strip_prefix("module cycle: ")
        .expect("expected prefix");
    let parts: Vec<&str> = after.split(" -> ").collect();
    assert!(parts.len() >= 3, "expected cycle path, got: {after}");
    assert_eq!(parts.first(), parts.last());
}

#[test]
fn imports_override_alphanumeric_order() {
    // The core property of the loader contract: when `a.keron` imports
    // from `z.keron`, `z` runs before `a` even though `a < z`
    // alphanumerically. Without this, every `use` edge could be
    // silently violated whenever the importer's name sorts before its
    // dependency's.
    let proj = TempProject::new("imports-override-alpha");
    let z_path = proj.write("z.keron", "val foo: Int = 7\n");
    let a_path = proj.write(
        "a.keron",
        "from \"./z.keron\" use foo\nval bar: Int = foo + 1\n",
    );
    let a_src = fs::read_to_string(&a_path).unwrap();
    let z_src = fs::read_to_string(&z_path).unwrap();
    let graph =
        resolve(TempProject::roots(&[(&a_path, &a_src), (&z_path, &z_src)])).expect("resolve ok");
    let a_id = ModuleId(fs::canonicalize(&a_path).unwrap());
    let z_id = ModuleId(fs::canonicalize(&z_path).unwrap());
    let pos_a = graph
        .topo_order
        .iter()
        .position(|m| m == &a_id)
        .expect("a in topo");
    let pos_z = graph
        .topo_order
        .iter()
        .position(|m| m == &z_id)
        .expect("z in topo");
    assert!(
        pos_z < pos_a,
        "import edge z -> a must serialize z first; got: {:?}",
        graph.topo_order,
    );
}

#[test]
fn alphanumeric_tie_break_when_no_imports() {
    // With no `use` edges between three modules, the topological
    // order falls back to alphanumeric `ModuleId` order. The previous
    // implementation drew this from `HashMap::keys()` which is
    // non-deterministic; with petgraph the input ordering matters and
    // we feed it sorted, so the result is stable across runs.
    let proj = TempProject::new("alpha-tiebreak");
    let c_path = proj.write("c.keron", "val cv: Int = 3\n");
    let b_path = proj.write("b.keron", "val bv: Int = 2\n");
    let a_path = proj.write("a.keron", "val av: Int = 1\n");
    let a_src = fs::read_to_string(&a_path).unwrap();
    let b_src = fs::read_to_string(&b_path).unwrap();
    let c_src = fs::read_to_string(&c_path).unwrap();
    // Pass the roots in a deliberately-shuffled order so a missing
    // sort step would surface as a topo_order matching the input.
    let graph = resolve(TempProject::roots(&[
        (&c_path, &c_src),
        (&a_path, &a_src),
        (&b_path, &b_src),
    ]))
    .expect("resolve ok");
    let a_id = ModuleId(fs::canonicalize(&a_path).unwrap());
    let b_id = ModuleId(fs::canonicalize(&b_path).unwrap());
    let c_id = ModuleId(fs::canonicalize(&c_path).unwrap());
    assert_eq!(
        graph.topo_order,
        vec![a_id, b_id, c_id],
        "expected alphanumeric order regardless of input order",
    );
}

#[test]
fn per_file_scope_isolates_vals_without_explicit_use() {
    // Under the old directory-concatenation model, every `val` in any
    // file in the same dir was visible everywhere. Under per-file
    // scope, referencing another module's val without an explicit
    // `use` must fail with the type checker's unknown-identifier
    // diagnostic.
    let proj = TempProject::new("per-file-scope");
    let a_path = proj.write("a.keron", "val x: Int = 1\n");
    let b_path = proj.write("b.keron", "val n: Int = x\n");
    let a_src = fs::read_to_string(&a_path).unwrap();
    let b_src = fs::read_to_string(&b_path).unwrap();
    let bundle = resolve(TempProject::roots(&[(&a_path, &a_src), (&b_path, &b_src)]))
        .expect_err("b references x without `use` -> should fail");
    let messages: Vec<&String> = bundle
        .errors
        .iter()
        .flat_map(|e| &e.diagnostics)
        .map(|d| &d.message)
        .collect();
    assert!(
        messages.iter().any(|m| m.contains('x')),
        "expected an error mentioning `x`, got: {messages:?}",
    );
}

#[test]
fn multi_root_loads_every_root_into_the_graph() {
    // Two roots with no `use` edges between them must both end up in
    // `graph.modules` and `graph.entries`. Previously only a single
    // entry was supported, so passing both required directory
    // concatenation; now they are independent first-class modules.
    let proj = TempProject::new("multi-root");
    let a_path = proj.write(
        "a.keron",
        "val s: Symlink = symlink(source = \"a-to\", target = \"a-from\")\n\
         reconcile s\n",
    );
    let b_path = proj.write(
        "b.keron",
        "val s: Symlink = symlink(source = \"b-to\", target = \"b-from\")\n\
         reconcile s\n",
    );
    let a_src = fs::read_to_string(&a_path).unwrap();
    let b_src = fs::read_to_string(&b_path).unwrap();
    let graph =
        resolve(TempProject::roots(&[(&a_path, &a_src), (&b_path, &b_src)])).expect("resolve ok");
    let a_id = ModuleId(fs::canonicalize(&a_path).unwrap());
    let b_id = ModuleId(fs::canonicalize(&b_path).unwrap());
    assert!(graph.modules.contains_key(&a_id), "a missing from modules");
    assert!(graph.modules.contains_key(&b_id), "b missing from modules");
    assert_eq!(graph.entries.len(), 2);
    assert!(graph.entries.contains(&a_id) && graph.entries.contains(&b_id));
}
