//! Corpus runner: discovers `.keron` fixtures and registers one nextest case
//! per file. Snapshots are sidecar `.snap` files next to each fixture.

mod harness;

use harness::{CORPUS_ROOT, collect_trials};
use libtest_mimic::Arguments;

fn main() {
    let args = Arguments::from_args();
    let trials = collect_trials(CORPUS_ROOT.as_path());
    libtest_mimic::run(&args, trials).exit();
}
