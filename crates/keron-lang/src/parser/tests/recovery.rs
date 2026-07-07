//! Item-level error recovery: broken items yield one diagnostic each
//! plus a partial AST containing every item that still parses.

use crate::{
    ast::Item,
    parser::{parse, parse_recovering},
};

#[test]
fn two_broken_items_report_two_errors_and_keep_the_valid_one() {
    let src = "val = 1\nval ok = 2\nfn broken( { 3 }\n";
    let (program, diagnostics) = parse_recovering(src);
    assert_eq!(
        diagnostics.len(),
        2,
        "one diagnostic per broken item, got: {diagnostics:#?}"
    );
    assert_eq!(program.items.len(), 1, "the valid item survives");
    let Item::Val(v) = &program.items[0] else {
        panic!("expected the surviving item to be `val ok`");
    };
    assert_eq!(v.name.node, "ok");
}

#[test]
fn parse_reports_all_errors_for_multiple_broken_items() {
    let src = "val = 1\nval ok = 2\nfn broken( { 3 }\n";
    let diagnostics = parse(src).expect_err("broken source must not parse");
    assert_eq!(diagnostics.len(), 2, "got: {diagnostics:#?}");
}

#[test]
fn garbage_only_input_reports_an_error_without_panicking() {
    let (program, diagnostics) = parse_recovering("@@@ ~~~ ???");
    assert!(program.items.is_empty());
    assert!(!diagnostics.is_empty());
}

#[test]
fn valid_input_yields_the_same_ast_as_parse_and_no_diagnostics() {
    let src = "val a = 1\nfn f(x: Int): Int { x }\nreconcile f(a)\n";
    let (recovered, diagnostics) = parse_recovering(src);
    assert!(diagnostics.is_empty(), "got: {diagnostics:#?}");
    let strict = parse(src).expect("valid source parses");
    assert_eq!(recovered, strict);
}

#[test]
fn broken_trailing_item_still_yields_the_leading_items() {
    let src = "val a = 1\nval b =\n";
    let (program, diagnostics) = parse_recovering(src);
    assert_eq!(diagnostics.len(), 1, "got: {diagnostics:#?}");
    assert_eq!(program.items.len(), 1);
    let Item::Val(v) = &program.items[0] else {
        panic!("expected `val a` to survive");
    };
    assert_eq!(v.name.node, "a");
}

#[test]
fn recovery_does_not_resync_on_indented_keywords() {
    // The broken `fn` body contains an indented `val`; recovery must
    // skip past it to the next column-zero item instead of surfacing
    // bogus follow-on errors from inside the broken body.
    let src = "fn broken(x: Int { \n    val inner = 1\n}\nval after = 2\n";
    let (program, diagnostics) = parse_recovering(src);
    assert_eq!(diagnostics.len(), 1, "got: {diagnostics:#?}");
    assert_eq!(program.items.len(), 1);
    let Item::Val(v) = &program.items[0] else {
        panic!("expected `val after` to survive");
    };
    assert_eq!(v.name.node, "after");
}

#[test]
fn depth_limit_still_rejects_before_recovery_runs() {
    let src = format!("val x = {}1{}", "(".repeat(300), ")".repeat(300));
    let (program, diagnostics) = parse_recovering(&src);
    assert!(program.items.is_empty());
    assert_eq!(diagnostics.len(), 1);
}
