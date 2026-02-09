mod apply;
mod discovery;
mod fs_util;
mod graph;
mod manifest_lua;
mod pipeline;
mod plan;
mod providers;
mod secrets;
mod template;

pub use apply::{ApplyOptions, apply_plan};
pub use discovery::discover_manifests;
pub use graph::build_execution_order;
pub use manifest_lua::{
    evaluate_manifest, evaluate_manifest_with_warnings, evaluate_many, evaluate_many_with_warnings,
};
pub use pipeline::{build_plan_for_folder, has_potentially_destructive_forced_changes};
pub use plan::build_plan;
pub use providers::ProviderRegistry;
