//! AST → canonical-source emitter.
//!
//! The emitter walks a `Program` and produces a fully formatted
//! string with 2-space indentation and a single trailing newline.
//! For wrap-eligible nodes (list/map/call literals, long binary
//! chains, multi-arg calls) it first formats the inline form into a
//! scratch string; if that fits in the remaining line budget the
//! inline form is committed, otherwise the block form is emitted
//! instead with one element per line and a trailing comma.
//!
//! Comments are emitted via the side-table built by
//! [`crate::trivia::extract_comments`]. The emitter maintains a
//! cursor into that table and flushes comments at three points:
//!
//! - **Leading**: before emitting an item whose start coincides with
//!   the comment's attached span.
//! - **Trailing**: after emitting an item whose end coincides.
//! - **`ModuleTrailing`**: after the last item.
//!
//! `BlockInterior` comments fall through to the trailing/leading
//! pass via their `block_span` heuristic — full nested-block placement
//! lands in a follow-up.

use crate::ast::{
    BinOp, Block, CallArg, CommentAttachment, CommentMap, Expr, FnDecl, ForPattern, Item, Literal,
    MapEntry, MatchArm, Param, Pattern, Program, ReconcileDecl, Span, Spanned, Stmt, StringPart,
    StructDecl, StructField, StructPatternField, TypeAliasDecl, UnaryOp, UseDecl, ValDecl,
};

use super::precedence::{Side, UNARY_PREC, binop_prec, child_needs_parens, is_right_assoc};
use super::string_lit::render_cooked_inner;

const INDENT_UNIT: &str = "  ";
const LINE_BUDGET: usize = 100;

pub fn format_program(program: &Program, comments: &CommentMap) -> String {
    let mut e = Emitter::new(comments);
    e.emit_program(program);
    e.finish()
}

struct Emitter<'src> {
    comments: &'src CommentMap,
    out: String,
    indent: usize,
    /// Index of the next comment to consider in `comments.comments`.
    next_comment: usize,
}

impl<'src> Emitter<'src> {
    const fn new(comments: &'src CommentMap) -> Self {
        Self {
            comments,
            out: String::new(),
            indent: 0,
            next_comment: 0,
        }
    }

    fn finish(mut self) -> String {
        // Strip excess trailing newlines and re-add exactly one.
        while self.out.ends_with("\n\n") {
            self.out.pop();
        }
        if !self.out.is_empty() && !self.out.ends_with('\n') {
            self.out.push('\n');
        }
        self.out
    }

    fn column(&self) -> usize {
        self.out
            .rfind('\n')
            .map_or(self.out.len(), |i| self.out.len() - i - 1)
    }

    fn write_indent(&mut self) {
        for _ in 0..self.indent {
            self.out.push_str(INDENT_UNIT);
        }
    }

    fn newline(&mut self) {
        self.out.push('\n');
    }

    fn write(&mut self, s: &str) {
        self.out.push_str(s);
    }

    // ============================================================
    // Comment handling
    // ============================================================

    fn emit_leading_comments_for(&mut self, target: &Span) {
        while self.next_comment < self.comments.comments.len() {
            let (c, attach) = &self.comments.comments[self.next_comment];
            let take = match attach {
                CommentAttachment::Leading(span) => span == target,
                _ => false,
            };
            if !take {
                break;
            }
            self.write_indent();
            self.write(&c.text);
            self.newline();
            self.next_comment += 1;
        }
    }

    fn emit_trailing_comment_for(&mut self, target: &Span) {
        if self.next_comment >= self.comments.comments.len() {
            return;
        }
        let (c, attach) = &self.comments.comments[self.next_comment];
        if matches!(attach, CommentAttachment::Trailing(span) if span == target) {
            self.write("  ");
            self.write(&c.text);
            self.next_comment += 1;
        }
    }

    fn emit_module_trailing_comments(&mut self) {
        while self.next_comment < self.comments.comments.len() {
            let (c, attach) = &self.comments.comments[self.next_comment];
            if !matches!(
                attach,
                CommentAttachment::ModuleTrailing | CommentAttachment::BlockInterior { .. }
            ) {
                self.next_comment += 1;
                continue;
            }
            self.newline();
            self.write_indent();
            self.write(&c.text);
            self.next_comment += 1;
        }
    }

    // ============================================================
    // Program & items
    // ============================================================

    fn emit_program(&mut self, program: &Program) {
        let mut first = true;
        for item in &program.items {
            let span = item.span();
            if !first {
                // Blank line separator between top-level items, placed
                // *before* any leading comments so the comment block
                // sits visually attached to the item it documents.
                self.newline();
            }
            first = false;
            self.emit_leading_comments_for(&span);
            self.write_indent();
            self.emit_item(item);
            self.emit_trailing_comment_for(&span);
            self.newline();
        }
        self.emit_module_trailing_comments();
    }

    fn emit_item(&mut self, item: &Item) {
        match item {
            Item::Use(u) => self.emit_use(u),
            Item::Val(v) => self.emit_val(v),
            Item::Fn(f) => self.emit_fn(f),
            Item::Struct(s) => self.emit_struct(s),
            Item::TypeAlias(t) => self.emit_type_alias(t),
            Item::Reconcile(r) => self.emit_reconcile(r),
            Item::ExprStmt(e) => self.emit_expr(e),
        }
    }

    fn emit_use(&mut self, u: &UseDecl) {
        self.write("from \"");
        self.write(&u.source.node);
        self.write("\" use ");
        let names: Vec<&str> = u.names.iter().map(|n| n.node.as_str()).collect();
        let inline = names.join(", ");
        if self.column() + inline.len() <= LINE_BUDGET {
            self.write(&inline);
        } else {
            // Block form.
            self.indent += 1;
            for (i, name) in names.iter().enumerate() {
                self.newline();
                self.write_indent();
                self.write(name);
                if i + 1 < names.len() {
                    self.write(",");
                }
            }
            self.indent -= 1;
        }
    }

    fn emit_val(&mut self, v: &ValDecl) {
        self.write("val ");
        self.write(&v.name.node);
        if let Some(ty) = &v.ty {
            self.write(": ");
            self.write(&ty.node.to_string());
        }
        self.write(" = ");
        self.emit_expr(&v.value);
    }

    fn emit_fn(&mut self, f: &FnDecl) {
        self.write("fn ");
        self.write(&f.name.node);
        self.write("(");
        self.emit_params(&f.params);
        self.write("): ");
        self.write(&f.return_type.node.to_string());
        self.write(" ");
        self.emit_block(&f.body);
    }

    fn emit_params(&mut self, params: &[Param]) {
        let inline = render_params_inline(params);
        if self.column() + inline.len() < LINE_BUDGET {
            self.write(&inline);
            return;
        }
        self.indent += 1;
        for (i, p) in params.iter().enumerate() {
            self.newline();
            self.write_indent();
            self.write(&p.name.node);
            self.write(": ");
            self.write(&p.ty.node.to_string());
            if let Some(default) = &p.default {
                self.write(" = ");
                self.emit_expr(default);
            }
            if i + 1 < params.len() {
                self.write(",");
            }
        }
        self.indent -= 1;
        self.newline();
        self.write_indent();
    }

    fn emit_struct(&mut self, s: &StructDecl) {
        self.write("struct ");
        self.write(&s.name.node);
        self.write(" {");
        if s.fields.is_empty() {
            self.write("}");
            return;
        }
        let inline = render_struct_fields_inline(&s.fields);
        if self.column() + 1 + inline.len() + 2 <= LINE_BUDGET {
            self.write(" ");
            self.write(&inline);
            self.write(" }");
            return;
        }
        self.indent += 1;
        for (i, field) in s.fields.iter().enumerate() {
            self.newline();
            self.write_indent();
            self.emit_struct_field(field);
            if i + 1 < s.fields.len() {
                self.write(",");
            }
        }
        self.indent -= 1;
        self.newline();
        self.write_indent();
        self.write("}");
    }

    fn emit_struct_field(&mut self, field: &StructField) {
        self.write(&field.name.node);
        self.write(": ");
        self.write(&field.ty.node.to_string());
        if let Some(default) = &field.default {
            self.write(" = ");
            self.emit_expr(default);
        }
    }

    fn emit_type_alias(&mut self, t: &TypeAliasDecl) {
        self.write("type ");
        self.write(&t.name.node);
        self.write(" = ");
        let variants: Vec<String> = t
            .variants
            .iter()
            .map(|v| format!("\"{}\"", render_cooked_inner(&v.node)))
            .collect();
        let inline = variants.join(" | ");
        if self.column() + inline.len() <= LINE_BUDGET {
            self.write(&inline);
            return;
        }
        // Block form: first variant on the same line, subsequent
        // variants on their own lines aligned at the `|`.
        let column_marker = self.column();
        let (first, rest) = variants.split_first().expect("checker rejects empty union");
        self.write(first);
        for v in rest {
            self.newline();
            for _ in 0..column_marker {
                self.out.push(' ');
            }
            self.write("| ");
            self.write(v);
        }
    }

    fn emit_reconcile(&mut self, r: &ReconcileDecl) {
        self.write("reconcile");
        let multi_chain = r.chains.iter().any(|c| c.len() > 1);
        let multi_top = r.chains.len() > 1;
        if !multi_top && !multi_chain {
            // `reconcile expr`
            let expr = &r.chains[0][0];
            self.write(" ");
            self.emit_expr(expr);
            return;
        }
        if multi_top {
            // `reconcile { ... }`
            self.write(" {");
            self.indent += 1;
            for chain in &r.chains {
                self.newline();
                self.write_indent();
                self.emit_chain(chain);
                self.write(";");
            }
            self.indent -= 1;
            self.newline();
            self.write_indent();
            self.write("}");
            return;
        }
        // single chain with multiple steps: `reconcile a -> b -> c`
        self.write(" ");
        self.emit_chain(&r.chains[0]);
    }

    fn emit_chain(&mut self, chain: &[Spanned<Expr>]) {
        for (i, step) in chain.iter().enumerate() {
            if i > 0 {
                self.write(" -> ");
            }
            self.emit_expr(step);
        }
    }

    // ============================================================
    // Expressions
    // ============================================================

    fn emit_expr(&mut self, expr: &Spanned<Expr>) {
        match &expr.node {
            Expr::Literal(lit) => self.write(&render_literal(lit)),
            Expr::Var(name) => self.write(name),
            Expr::Binary { op, lhs, rhs } => self.emit_binary(*op, lhs, rhs),
            Expr::Unary { op, operand } => self.emit_unary(*op, operand),
            Expr::Field { receiver, field } => self.emit_field(receiver, &field.node),
            Expr::Call { callee, args } => self.emit_call(&callee.node, args),
            Expr::List(items) => self.emit_list(items),
            Expr::Map(entries) => self.emit_map(entries),
            Expr::If {
                cond,
                then_branch,
                else_branch,
            } => {
                self.emit_if(cond, then_branch, else_branch);
            }
            Expr::For {
                pattern,
                iter_expr,
                body,
            } => {
                self.emit_for(pattern, iter_expr, body);
            }
            Expr::Match { scrutinee, arms } => self.emit_match(scrutinee, arms),
            Expr::Interpolation(parts) => self.emit_interpolation(parts),
        }
    }

    fn emit_binary(&mut self, op: BinOp, lhs: &Spanned<Expr>, rhs: &Spanned<Expr>) {
        let parent_prec = binop_prec(op);
        let right_assoc = is_right_assoc(op);
        let lhs_parens = child_needs_parens(&lhs.node, parent_prec, right_assoc, Side::Left);
        let rhs_parens = child_needs_parens(&rhs.node, parent_prec, right_assoc, Side::Right);
        if lhs_parens {
            self.write("(");
        }
        self.emit_expr(lhs);
        if lhs_parens {
            self.write(")");
        }
        self.write(" ");
        self.write(op.symbol());
        self.write(" ");
        if rhs_parens {
            self.write("(");
        }
        self.emit_expr(rhs);
        if rhs_parens {
            self.write(")");
        }
    }

    fn emit_unary(&mut self, op: UnaryOp, operand: &Spanned<Expr>) {
        self.write(op.symbol());
        let parens = child_needs_parens(&operand.node, UNARY_PREC, false, Side::Unary);
        if parens {
            self.write("(");
        }
        self.emit_expr(operand);
        if parens {
            self.write(")");
        }
    }

    fn emit_field(&mut self, receiver: &Spanned<Expr>, field: &str) {
        self.emit_expr(receiver);
        self.write(".");
        self.write(field);
    }

    fn emit_call(&mut self, callee: &str, args: &[CallArg]) {
        self.write(callee);
        self.write("(");
        if args.is_empty() {
            self.write(")");
            return;
        }
        let inline = render_call_args_inline(args);
        if self.column() + inline.len() < LINE_BUDGET {
            self.write(&inline);
            self.write(")");
            return;
        }
        self.indent += 1;
        for arg in args {
            self.newline();
            self.write_indent();
            self.emit_call_arg(arg);
            // Trailing comma after every arg in block form, including
            // the last — matches rustfmt-style diff-friendliness.
            self.write(",");
        }
        self.indent -= 1;
        self.newline();
        self.write_indent();
        self.write(")");
    }

    fn emit_call_arg(&mut self, arg: &CallArg) {
        if let Some(name) = &arg.name {
            self.write(&name.node);
            self.write(" = ");
        }
        self.emit_expr(&arg.value);
    }

    fn emit_list(&mut self, items: &[Spanned<Expr>]) {
        if items.is_empty() {
            self.write("[]");
            return;
        }
        let inline = render_list_inline(items);
        if self.column() + inline.len() <= LINE_BUDGET {
            self.write(&inline);
            return;
        }
        self.write("[");
        self.indent += 1;
        for item in items {
            self.newline();
            self.write_indent();
            self.emit_expr(item);
            self.write(",");
        }
        self.indent -= 1;
        self.newline();
        self.write_indent();
        self.write("]");
    }

    fn emit_map(&mut self, entries: &[MapEntry]) {
        if entries.is_empty() {
            self.write("{}");
            return;
        }
        let inline = render_map_inline(entries);
        if self.column() + inline.len() <= LINE_BUDGET {
            self.write(&inline);
            return;
        }
        self.write("{");
        self.indent += 1;
        for entry in entries {
            self.newline();
            self.write_indent();
            self.emit_expr(&entry.key);
            self.write(": ");
            self.emit_expr(&entry.value);
            self.write(",");
        }
        self.indent -= 1;
        self.newline();
        self.write_indent();
        self.write("}");
    }

    fn emit_if(&mut self, cond: &Spanned<Expr>, then_b: &Block, else_b: &Block) {
        self.write("if ");
        self.emit_expr(cond);
        self.write(" ");
        self.emit_block(then_b);
        if !block_is_empty(else_b) {
            self.write(" else ");
            self.emit_block(else_b);
        }
    }

    fn emit_for(&mut self, pattern: &ForPattern, iter: &Spanned<Expr>, body: &Block) {
        self.write("for ");
        match pattern {
            ForPattern::Elem(name) => self.write(&name.node),
            ForPattern::Entry { key, value } => {
                self.write("(");
                self.write(&key.node);
                self.write(", ");
                self.write(&value.node);
                self.write(")");
            }
        }
        self.write(" in ");
        self.emit_expr(iter);
        self.write(" ");
        self.emit_block(body);
    }

    fn emit_match(&mut self, scrutinee: &Spanned<Expr>, arms: &[MatchArm]) {
        self.write("match ");
        self.emit_expr(scrutinee);
        self.write(" {");
        self.indent += 1;
        for arm in arms {
            self.newline();
            self.write_indent();
            self.emit_pattern(&arm.pattern.node);
            if let Some(g) = &arm.guard {
                self.write(" if ");
                self.emit_expr(g);
            }
            self.write(" => ");
            self.emit_expr(&arm.body);
            self.write(",");
        }
        self.indent -= 1;
        self.newline();
        self.write_indent();
        self.write("}");
    }

    fn emit_pattern(&mut self, pattern: &Pattern) {
        match pattern {
            Pattern::Lit(lit) => self.write(&render_literal(lit)),
            Pattern::Wildcard => self.write("_"),
            Pattern::Bind(name) => self.write(name),
            Pattern::Struct { name, fields } => {
                self.write(&name.node);
                self.write(" {");
                if fields.is_empty() {
                    self.write("}");
                    return;
                }
                let inline = render_struct_pattern_fields_inline(fields);
                if self.column() + 1 + inline.len() + 2 <= LINE_BUDGET {
                    self.write(" ");
                    self.write(&inline);
                    self.write(" }");
                    return;
                }
                self.indent += 1;
                for (i, f) in fields.iter().enumerate() {
                    self.newline();
                    self.write_indent();
                    self.emit_struct_pattern_field(f);
                    if i + 1 < fields.len() {
                        self.write(",");
                    }
                }
                self.indent -= 1;
                self.newline();
                self.write_indent();
                self.write("}");
            }
        }
    }

    fn emit_struct_pattern_field(&mut self, f: &StructPatternField) {
        self.write(&f.name.node);
        if let Some(inner) = &f.pattern {
            self.write(": ");
            self.emit_pattern(&inner.node);
        }
    }

    fn emit_interpolation(&mut self, parts: &[StringPart]) {
        self.write("\"");
        for part in parts {
            match part {
                StringPart::Text(t) => self.write(&render_cooked_inner(t)),
                StringPart::Expr { expr, .. } => {
                    self.write("${");
                    self.emit_expr(expr);
                    self.write("}");
                }
            }
        }
        self.write("\"");
    }

    // ============================================================
    // Blocks & statements
    // ============================================================

    fn emit_block(&mut self, block: &Block) {
        if block_is_empty(block) {
            self.write("{}");
            return;
        }
        self.write("{");
        self.indent += 1;
        for stmt in &block.stmts {
            self.newline();
            self.write_indent();
            self.emit_stmt(stmt);
        }
        if let Some(trailing) = &block.trailing {
            self.newline();
            self.write_indent();
            self.emit_expr(trailing);
        }
        self.indent -= 1;
        self.newline();
        self.write_indent();
        self.write("}");
    }

    fn emit_stmt(&mut self, stmt: &Stmt) {
        match stmt {
            Stmt::Val(v) => self.emit_val(v),
            Stmt::Reconcile(r) => self.emit_reconcile(r),
        }
    }
}

const fn block_is_empty(block: &Block) -> bool {
    block.stmts.is_empty() && block.trailing.is_none()
}

// =====================================================================
// Inline-render helpers (pure — no Emitter state)
// =====================================================================

fn render_literal(lit: &Literal) -> String {
    match lit {
        Literal::String(s) => format!("\"{}\"", render_cooked_inner(s)),
        Literal::Int(n) => n.to_string(),
        Literal::Boolean(b) => b.to_string(),
        Literal::Double(d) => render_double(*d),
        Literal::Null => "null".to_string(),
    }
}

fn render_double(d: f64) -> String {
    // Canonical form: always include a decimal point so the value
    // round-trips as `Double` rather than re-parsing as `Int`.
    if d.is_nan() {
        return "NaN".to_string(); // parser rejects, but we still
        // produce a recognizable token.
    }
    let s = format!("{d}");
    if s.contains('.') || s.contains('e') || s.contains('E') {
        s
    } else {
        format!("{s}.0")
    }
}

fn render_params_inline(params: &[Param]) -> String {
    params
        .iter()
        .map(|p| {
            let mut s = format!("{}: {}", p.name.node, p.ty.node);
            if let Some(default) = &p.default {
                s.push_str(" = ");
                s.push_str(&render_expr_inline(&default.node));
            }
            s
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn render_struct_fields_inline(fields: &[StructField]) -> String {
    fields
        .iter()
        .map(|f| {
            let mut s = format!("{}: {}", f.name.node, f.ty.node);
            if let Some(default) = &f.default {
                s.push_str(" = ");
                s.push_str(&render_expr_inline(&default.node));
            }
            s
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn render_struct_pattern_fields_inline(fields: &[StructPatternField]) -> String {
    fields
        .iter()
        .map(|f| {
            f.pattern.as_ref().map_or_else(
                || f.name.node.clone(),
                |p| format!("{}: {}", f.name.node, render_pattern_inline(&p.node)),
            )
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn render_call_args_inline(args: &[CallArg]) -> String {
    args.iter()
        .map(|a| {
            a.name.as_ref().map_or_else(
                || render_expr_inline(&a.value.node),
                |name| format!("{} = {}", name.node, render_expr_inline(&a.value.node)),
            )
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn render_list_inline(items: &[Spanned<Expr>]) -> String {
    let parts: Vec<String> = items.iter().map(|i| render_expr_inline(&i.node)).collect();
    format!("[{}]", parts.join(", "))
}

fn render_map_inline(entries: &[MapEntry]) -> String {
    let parts: Vec<String> = entries
        .iter()
        .map(|e| {
            format!(
                "{}: {}",
                render_expr_inline(&e.key.node),
                render_expr_inline(&e.value.node),
            )
        })
        .collect();
    format!("{{{}}}", parts.join(", "))
}

fn render_pattern_inline(p: &Pattern) -> String {
    match p {
        Pattern::Lit(l) => render_literal(l),
        Pattern::Wildcard => "_".to_string(),
        Pattern::Bind(name) => name.clone(),
        Pattern::Struct { name, fields } => {
            if fields.is_empty() {
                format!("{} {{}}", name.node)
            } else {
                format!(
                    "{} {{ {} }}",
                    name.node,
                    render_struct_pattern_fields_inline(fields)
                )
            }
        }
    }
}

fn render_expr_inline(expr: &Expr) -> String {
    match expr {
        Expr::Literal(l) => render_literal(l),
        Expr::Var(name) => name.clone(),
        Expr::Binary { op, lhs, rhs } => render_binary_inline(*op, lhs, rhs),
        Expr::Unary { op, operand } => render_unary_inline(*op, operand),
        Expr::Field { receiver, field } => {
            format!("{}.{}", render_expr_inline(&receiver.node), field.node)
        }
        Expr::Call { callee, args } => {
            format!("{}({})", callee.node, render_call_args_inline(args))
        }
        Expr::List(items) => render_list_inline(items),
        Expr::Map(entries) => render_map_inline(entries),
        Expr::If {
            cond,
            then_branch,
            else_branch,
        } => render_if_inline(cond, then_branch, else_branch),
        Expr::For {
            pattern,
            iter_expr,
            body,
        } => render_for_inline(pattern, iter_expr, body),
        Expr::Match { scrutinee, arms } => render_match_inline(scrutinee, arms),
        Expr::Interpolation(parts) => render_interpolation_inline(parts),
    }
}

fn render_binary_inline(op: BinOp, lhs: &Spanned<Expr>, rhs: &Spanned<Expr>) -> String {
    let parent_prec = binop_prec(op);
    let right_assoc = is_right_assoc(op);
    let lhs_str = render_expr_inline(&lhs.node);
    let rhs_str = render_expr_inline(&rhs.node);
    let lhs_p = child_needs_parens(&lhs.node, parent_prec, right_assoc, Side::Left);
    let rhs_p = child_needs_parens(&rhs.node, parent_prec, right_assoc, Side::Right);
    format!(
        "{}{}{} {} {}{}{}",
        if lhs_p { "(" } else { "" },
        lhs_str,
        if lhs_p { ")" } else { "" },
        op.symbol(),
        if rhs_p { "(" } else { "" },
        rhs_str,
        if rhs_p { ")" } else { "" },
    )
}

fn render_unary_inline(op: UnaryOp, operand: &Spanned<Expr>) -> String {
    let needs = child_needs_parens(&operand.node, UNARY_PREC, false, Side::Unary);
    let inner = render_expr_inline(&operand.node);
    if needs {
        format!("{}({inner})", op.symbol())
    } else {
        format!("{}{inner}", op.symbol())
    }
}

fn render_if_inline(cond: &Spanned<Expr>, then_branch: &Block, else_branch: &Block) -> String {
    let then_s = render_block_inline(then_branch);
    if block_is_empty(else_branch) {
        format!("if {} {}", render_expr_inline(&cond.node), then_s)
    } else {
        format!(
            "if {} {} else {}",
            render_expr_inline(&cond.node),
            then_s,
            render_block_inline(else_branch)
        )
    }
}

fn render_for_inline(pattern: &ForPattern, iter_expr: &Spanned<Expr>, body: &Block) -> String {
    let pat = match pattern {
        ForPattern::Elem(n) => n.node.clone(),
        ForPattern::Entry { key, value } => format!("({}, {})", key.node, value.node),
    };
    format!(
        "for {pat} in {} {}",
        render_expr_inline(&iter_expr.node),
        render_block_inline(body)
    )
}

fn render_match_inline(scrutinee: &Spanned<Expr>, arms: &[MatchArm]) -> String {
    // `match` never truly inlines (it always block-shapes in
    // `emit_match`). This helper exists so callers that build an
    // inline scratch string can still produce *something* for budget
    // measurement — the wider scratch will exceed the budget and the
    // caller switches to block form.
    let arms_s: Vec<String> = arms
        .iter()
        .map(|a| {
            let guard = a.guard.as_ref().map_or(String::new(), |g| {
                format!(" if {}", render_expr_inline(&g.node))
            });
            format!(
                "{}{} => {}",
                render_pattern_inline(&a.pattern.node),
                guard,
                render_expr_inline(&a.body.node)
            )
        })
        .collect();
    format!(
        "match {} {{ {} }}",
        render_expr_inline(&scrutinee.node),
        arms_s.join(", ")
    )
}

fn render_interpolation_inline(parts: &[StringPart]) -> String {
    let mut s = String::from("\"");
    for part in parts {
        match part {
            StringPart::Text(t) => s.push_str(&render_cooked_inner(t)),
            StringPart::Expr { expr, .. } => {
                s.push_str("${");
                s.push_str(&render_expr_inline(&expr.node));
                s.push('}');
            }
        }
    }
    s.push('"');
    s
}

fn render_block_inline(block: &Block) -> String {
    if block_is_empty(block) {
        return "{}".to_string();
    }
    let mut parts: Vec<String> = block.stmts.iter().map(render_stmt_inline).collect();
    if let Some(t) = &block.trailing {
        parts.push(render_expr_inline(&t.node));
    }
    format!("{{ {} }}", parts.join("; "))
}

fn render_stmt_inline(stmt: &Stmt) -> String {
    match stmt {
        Stmt::Val(v) => {
            let ty =
                v.ty.as_ref()
                    .map_or(String::new(), |t| format!(": {}", t.node));
            format!(
                "val {}{} = {}",
                v.name.node,
                ty,
                render_expr_inline(&v.value.node)
            )
        }
        Stmt::Reconcile(r) => render_reconcile_inline(r),
    }
}

fn render_reconcile_inline(r: &ReconcileDecl) -> String {
    let multi_chain = r.chains.iter().any(|c| c.len() > 1);
    let multi_top = r.chains.len() > 1;
    if !multi_top && !multi_chain {
        return format!("reconcile {}", render_expr_inline(&r.chains[0][0].node));
    }
    if multi_top {
        let parts: Vec<String> = r
            .chains
            .iter()
            .map(|c| {
                c.iter()
                    .map(|s| render_expr_inline(&s.node))
                    .collect::<Vec<_>>()
                    .join(" -> ")
            })
            .collect();
        return format!("reconcile {{ {} }}", parts.join("; "));
    }
    let parts: Vec<String> = r.chains[0]
        .iter()
        .map(|s| render_expr_inline(&s.node))
        .collect();
    format!("reconcile {}", parts.join(" -> "))
}
