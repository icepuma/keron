//! Apply phase. Stubbed today: the executor lands alongside the
//! evaluator, since there is nothing to execute until `build_plan`
//! produces concrete `ResourceState` values.

#![allow(clippy::redundant_pub_crate)]

use anyhow::{Result, bail};

use crate::plan::Plan;

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct ExecuteSummary {
    pub(crate) added: usize,
    pub(crate) changed: usize,
    pub(crate) destroyed: usize,
}

pub(crate) fn execute(_plan: &Plan) -> Result<ExecuteSummary> {
    bail!("executor not yet implemented")
}
