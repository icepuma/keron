//! Corpus runner: discovers `.keron` fixtures and registers one nextest case
//! per file. Snapshots are sidecar `.snap` files next to each fixture.

// Test binary: `pub` items are never reachable from outside the binary, so
// `unreachable_pub` always fires. `redundant_pub_crate` rules out the
// `pub(crate)` workaround. Allow it crate-wide for this test target.
#![allow(unreachable_pub)]

mod harness;

use harness::{CORPUS_ROOT, collect_trials};
use libtest_mimic::Arguments;

fn main() {
    let args = Arguments::from_args();
    let trials = collect_trials(CORPUS_ROOT.as_path());
    libtest_mimic::run(&args, trials).exit();
}
