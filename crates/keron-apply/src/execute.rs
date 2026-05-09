//! Apply phase. Stubbed today: the executor lands alongside the
//! evaluator, since there is nothing to execute until `build_plan`
//! produces concrete `ResourceState` values.

use anyhow::{Result, bail};

use crate::plan::Plan;

#[derive(Debug, Clone, Copy, Default)]
pub struct ExecuteSummary {
    pub added: usize,
    pub changed: usize,
    pub destroyed: usize,
}

pub fn execute(_plan: &Plan) -> Result<ExecuteSummary> {
    bail!("executor not yet implemented")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn execute_returns_not_implemented_error() {
        let err = execute(&Plan::default()).unwrap_err();
        assert!(err.to_string().contains("not yet implemented"));
    }
}
