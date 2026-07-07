//! Shared plain-text rendering of declarations and signatures, used
//! by hover, completion details, and signature help. All output is
//! keron surface syntax (via `Display for Type`), never Rust debug
//! formatting.

use keron_lang::{FnDecl, FnSig, ParamSig, StructDecl, TypeAliasDecl, ValDecl};

/// `fn name(p: T, q: U = …): Ret` from a parsed declaration.
#[must_use]
pub fn fn_decl_signature(f: &FnDecl) -> String {
    let params: Vec<String> = f
        .params
        .iter()
        .map(|p| {
            let mut s = format!("{}: {}", p.name.node, p.ty.node);
            if p.default.is_some() {
                s.push_str(" = …");
            }
            s
        })
        .collect();
    format!(
        "fn {}({}): {}",
        f.name.node,
        params.join(", "),
        f.return_type.node
    )
}

/// Signature text for a checker-level [`FnSig`] (imports + builtins).
/// Struct constructors render in the brace-literal shape they are
/// actually written in.
#[must_use]
pub fn fn_sig_signature(name: &str, sig: &FnSig) -> String {
    if let Some(struct_name) = &sig.struct_name {
        let fields: Vec<String> = sig.params.iter().map(param_label).collect();
        return format!("struct {struct_name} {{ {} }}", fields.join(", "));
    }
    let params: Vec<String> = sig.params.iter().map(param_label).collect();
    format!("fn {name}({}): {}", params.join(", "), sig.return_type)
}

/// `name: Type` (with a `= …` marker for defaulted params) — the unit
/// signature help highlights.
#[must_use]
pub fn param_label(p: &ParamSig) -> String {
    let mut s = format!("{}: {}", p.name, p.ty);
    if p.has_default {
        s.push_str(" = …");
    }
    s
}

/// `name(p = $1, q = $2)$0` — a call snippet covering the required
/// (non-defaulted) parameters; struct constructors use brace-literal
/// shape instead.
#[must_use]
pub fn call_snippet(name: &str, sig: &FnSig) -> String {
    let required: Vec<&ParamSig> = sig.params.iter().filter(|p| !p.has_default).collect();
    if let Some(struct_name) = &sig.struct_name {
        let fields: Vec<String> = required
            .iter()
            .enumerate()
            .map(|(i, p)| format!("{}: ${{{}}}", p.name, i + 1))
            .collect();
        return format!("{struct_name} {{ {} }}$0", fields.join(", "));
    }
    let args: Vec<String> = required
        .iter()
        .enumerate()
        .map(|(i, p)| format!("{} = ${{{}}}", p.name, i + 1))
        .collect();
    format!("{name}({})$0", args.join(", "))
}

/// `struct Name { field: T, … }` from a parsed declaration.
#[must_use]
pub fn struct_decl_signature(s: &StructDecl) -> String {
    let fields: Vec<String> = s
        .fields
        .iter()
        .map(|f| {
            let mut t = format!("{}: {}", f.name.node, f.ty.node);
            if f.default.is_some() {
                t.push_str(" = …");
            }
            t
        })
        .collect();
    format!("struct {} {{ {} }}", s.name.node, fields.join(", "))
}

/// `type Name = "a" | "b"` from a parsed declaration.
#[must_use]
pub fn type_alias_signature(t: &TypeAliasDecl) -> String {
    let variants: Vec<String> = t
        .variants
        .iter()
        .map(|v| format!("\"{}\"", v.node))
        .collect();
    format!("type {} = {}", t.name.node, variants.join(" | "))
}

/// `val name: Type`, or just `val name` when unannotated (the LSP does
/// not re-run inference).
#[must_use]
pub fn val_decl_signature(v: &ValDecl) -> String {
    v.ty.as_ref().map_or_else(
        || format!("val {}", v.name.node),
        |ty| format!("val {}: {}", v.name.node, ty.node),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use keron_lang::{Item, parse};

    fn first_fn(src: &str) -> FnDecl {
        let program = parse(src).expect("fixture parses");
        program
            .items
            .into_iter()
            .find_map(|i| match i {
                Item::Fn(f) => Some(f),
                _ => None,
            })
            .expect("has fn")
    }

    #[test]
    fn renders_fn_with_default_marker() {
        let f = first_fn("fn greet(who: String, punct: String = \"!\"): String { who + punct }");
        assert_eq!(
            fn_decl_signature(&f),
            "fn greet(who: String, punct: String = …): String"
        );
    }

    #[test]
    fn renders_struct_and_alias() {
        let program = parse("struct P { x: Int, y: Int = 0 }\ntype C = \"red\" | \"blue\"\n")
            .expect("parses");
        let mut rendered = Vec::new();
        for item in &program.items {
            match item {
                Item::Struct(s) => rendered.push(struct_decl_signature(s)),
                Item::TypeAlias(t) => rendered.push(type_alias_signature(t)),
                _ => {}
            }
        }
        assert_eq!(
            rendered,
            vec![
                "struct P { x: Int, y: Int = … }".to_string(),
                "type C = \"red\" | \"blue\"".to_string(),
            ]
        );
    }

    #[test]
    fn renders_val_with_and_without_annotation() {
        let program = parse("val a: Int = 1\nval b = 2\n").expect("parses");
        let vals: Vec<String> = program
            .items
            .iter()
            .filter_map(|i| match i {
                Item::Val(v) => Some(val_decl_signature(v)),
                _ => None,
            })
            .collect();
        assert_eq!(vals, vec!["val a: Int".to_string(), "val b".to_string()]);
    }
}
