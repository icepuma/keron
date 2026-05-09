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

    fn entry_source(file: &Path, content: &str) -> EntrySource {
        let canonical = fs::canonicalize(file).expect("canonicalize entry");
        let base_dir = canonical.parent().expect("entry has parent").to_path_buf();
        EntrySource {
            text: content.to_string(),
            base_dir,
            id: ModuleId::File(canonical),
        }
    }
}

impl Drop for TempProject {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn resolves_std_fs_import() {
    let proj = TempProject::new("std-fs");
    let src = "from \"std:fs\" use symlink\n\
               val s: Symlink = symlink(from = \"a\", to = \"b\")\n\
               reconcile s\n";
    let entry = proj.write("entry.keron", src);
    let graph = resolve(TempProject::entry_source(&entry, src)).expect("resolve ok");
    let entry_id = ModuleId::File(fs::canonicalize(&entry).unwrap());
    assert_eq!(graph.entry, entry_id);
    assert!(graph.modules.contains_key(&ModuleId::Std("fs".into())));
}

#[test]
fn missing_std_module_errors() {
    let proj = TempProject::new("missing-std");
    let src = "from \"std:zzz\" use foo\n\
               val n: Int = foo()\n";
    let entry = proj.write("entry.keron", src);
    let errors = resolve(TempProject::entry_source(&entry, src)).expect_err("should fail");
    assert!(
        errors
            .iter()
            .flat_map(|e| &e.diagnostics)
            .any(|d| d.message.contains("unknown stdlib module"))
    );
}

#[test]
fn invalid_path_prefix_errors() {
    let proj = TempProject::new("bad-prefix");
    let src = "from \"helpers.keron\" use foo\n";
    let entry = proj.write("entry.keron", src);
    let errors = resolve(TempProject::entry_source(&entry, src)).expect_err("should fail");
    assert!(
        errors
            .iter()
            .flat_map(|e| &e.diagnostics)
            .any(|d| d.message.contains("must start with"))
    );
}

#[test]
fn cross_file_import_resolves() {
    let proj = TempProject::new("cross-file");
    let helpers_src = "from \"std:fs\" use symlink\n\
                       fn link(name: String): Symlink {\n\
                       \tsymlink(from = name, to = name)\n\
                       }\n";
    proj.write("helpers.keron", helpers_src);
    let entry_src = "from \"./helpers.keron\" use link\n\
                     val s: Symlink = link(\"x\")\n\
                     reconcile s\n";
    let entry = proj.write("entry.keron", entry_src);
    let graph = resolve(TempProject::entry_source(&entry, entry_src)).expect("resolve ok");
    assert_eq!(graph.modules.len(), 3); // entry + helpers + std:fs
}

#[test]
fn missing_export_errors() {
    let proj = TempProject::new("missing-export");
    let helpers_src = "fn other(): Int { 1 }\n";
    proj.write("helpers.keron", helpers_src);
    let entry_src = "from \"./helpers.keron\" use missing\n\
                     val n: Int = missing()\n";
    let entry = proj.write("entry.keron", entry_src);
    let errors = resolve(TempProject::entry_source(&entry, entry_src)).expect_err("should fail");
    assert!(
        errors
            .iter()
            .flat_map(|e| &e.diagnostics)
            .any(|d| d.message.contains("does not export `missing`"))
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
    let errors = resolve(TempProject::entry_source(&entry, entry_src)).expect_err("should fail");
    assert!(
        errors
            .iter()
            .flat_map(|e| &e.diagnostics)
            .any(|d| d.message.contains("module cycle"))
    );
}

#[test]
fn duplicate_local_collides_with_import() {
    let proj = TempProject::new("dup");
    let src = "from \"std:fs\" use symlink\n\
               fn symlink(from: String, to: String): Symlink {\n\
               \tsymlink(from = from, to = to)\n\
               }\n";
    let entry = proj.write("entry.keron", src);
    let errors = resolve(TempProject::entry_source(&entry, src)).expect_err("should fail");
    assert!(
        errors
            .iter()
            .flat_map(|e| &e.diagnostics)
            .any(|d| d.message.contains("`symlink` is already defined"))
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
    let entry_id = ModuleId::File(fs::canonicalize(&entry).unwrap());
    let entry_mod = graph.modules.get(&entry_id).expect("entry module present");
    let (origin, original) = entry_mod
        .imports
        .get("greeting")
        .expect("greeting in imports");
    assert_eq!(original, "greeting");
    let helpers_id = ModuleId::File(fs::canonicalize(proj.root.join("helpers.keron")).unwrap());
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
    let errors = resolve(TempProject::entry_source(&entry, entry_src)).expect_err("should fail");
    let cycle_msg = errors
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
