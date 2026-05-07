//! Type checker unit tests, grouped by topic.

mod arithmetic;
mod lists;
mod literals;
mod strings;
mod vars;

use crate::{check::check, diagnostic::Diagnostic, parser::parse};

pub(super) fn check_src(src: &str) -> Result<(), Vec<Diagnostic>> {
    let prog = parse(src).expect("parse should succeed");
    check(&prog)
}
