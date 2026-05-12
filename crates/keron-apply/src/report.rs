//! Render module-resolution diagnostics with line/column-aware
//! ariadne reports against each owning module's source text.
//!
//! When a diagnostic's owning module isn't in the source map (or the
//! source is empty), there's no `(line, col)` to point at — those
//! fall back to a plain `[<module-id>] <message>` line, kept inline
//! with the rendered file reports for consistency.

use std::io::Cursor;

use ariadne::{Color, Label, Report, ReportKind, sources};
use keron_lang::Diagnostic;
use keron_modules::{ModuleId, ResolveErrors};

pub fn render(bundle: &ResolveErrors, color: bool) -> String {
    let mut out: Vec<u8> = Vec::new();
    for err in &bundle.errors {
        for d in &err.diagnostics {
            render_one(&err.module, d, &bundle.sources, color, &mut out);
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn render_one(
    module: &ModuleId,
    d: &Diagnostic,
    src_by_id: &std::collections::HashMap<ModuleId, String>,
    color: bool,
    out: &mut Vec<u8>,
) {
    let header = module.display();
    let Some(src) = src_by_id.get(module).filter(|s| !s.is_empty()) else {
        // Ariadne needs a non-empty buffer to compute line/col; emit
        // a minimal one-liner when we don't have the source.
        let _ = std::io::Write::write_fmt(out, format_args!("[{header}] {}\n", d.message));
        return;
    };

    // Synthesized `0..0` spans (cycle reports) and out-of-range spans
    // would otherwise panic ariadne — clamp to source length.
    let end = d.span.end.min(src.len());
    let start = d.span.start.min(end);
    let span = (header.clone(), start..end);

    // Per-label colors set via `Label::with_color` bypass
    // `Config::with_color(false)`, so only paint when requested.
    let mut label = Label::new(span.clone()).with_message(&d.message);
    if color {
        label = label.with_color(Color::Red);
    }
    let report = Report::build(ReportKind::Error, span)
        .with_message(&d.message)
        .with_label(label)
        .with_config(ariadne::Config::new().with_color(color))
        .finish();

    let mut buf: Vec<u8> = Vec::new();
    let cache = sources(std::iter::once((header.clone(), src.as_str())));
    if report.write(cache, Cursor::new(&mut buf)).is_err() {
        // A renderer error must not lose info — fall back to the plain
        // one-liner rather than swallow the diagnostic.
        let _ = std::io::Write::write_fmt(out, format_args!("[{header}] {}\n", d.message));
        return;
    }
    out.extend_from_slice(&buf);
}

#[cfg(test)]
mod tests {
    use super::*;
    use keron_lang::Diagnostic;
    use keron_modules::{ResolveError, ResolveErrors};
    use std::collections::HashMap;

    fn bundle_with(module: ModuleId, source: &str, diagnostics: Vec<Diagnostic>) -> ResolveErrors {
        let mut sources = HashMap::new();
        sources.insert(module.clone(), source.to_string());
        ResolveErrors {
            errors: vec![ResolveError {
                module,
                diagnostics,
            }],
            sources,
        }
    }

    #[test]
    fn render_includes_message_and_line_marker_for_file_module() {
        let src = "val x = 1\nval y = 2\n";
        let bundle = bundle_with(
            ModuleId("/tmp/foo.keron".into()),
            src,
            vec![Diagnostic::new(14..15, "bad token")],
        );
        let out = render(&bundle, false);
        assert!(out.contains("bad token"), "missing message: {out}");
        assert!(out.contains("Error"), "missing kind header: {out}");
        assert!(out.contains("/tmp/foo.keron"), "missing path: {out}");
        // Byte 14 lives on line 2 ("val x = 1\n" is 10 chars).
        assert!(out.contains(":2"), "missing line marker: {out}");
    }

    #[test]
    fn render_falls_back_for_module_without_source() {
        let bundle = bundle_with(
            ModuleId("/synth.keron".into()),
            "",
            vec![Diagnostic::new(0..0, "synthesized error")],
        );
        let out = render(&bundle, false);
        assert!(out.contains("/synth.keron"), "missing module header: {out}");
        assert!(out.contains("synthesized error"), "missing message: {out}");
    }

    #[test]
    fn render_iterates_each_diagnostic() {
        let bundle = bundle_with(
            ModuleId("/x.keron".into()),
            "val x = 1\n",
            vec![
                Diagnostic::new(0..3, "first"),
                Diagnostic::new(4..5, "second"),
            ],
        );
        let out = render(&bundle, false);
        assert!(
            out.contains("first") && out.contains("second"),
            "got: {out}"
        );
    }

    #[test]
    fn render_clamps_span_past_source_end() {
        let bundle = bundle_with(
            ModuleId("/y.keron".into()),
            "val x = 1\n",
            vec![Diagnostic::new(999..1001, "out of range")],
        );
        let out = render(&bundle, false);
        assert!(out.contains("out of range"), "got: {out}");
    }
}
