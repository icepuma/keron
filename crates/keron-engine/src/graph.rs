use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;

use keron_domain::ManifestSpec;

use crate::error::GraphError;

/// Build a dependency-respecting execution order using topological sorting.
///
/// # Errors
///
/// Returns an error when dependencies point to missing manifests or when a
/// dependency cycle is detected.
pub fn build_execution_order(
    manifests: &[ManifestSpec],
) -> std::result::Result<Vec<PathBuf>, GraphError> {
    if manifests.is_empty() {
        return Ok(Vec::new());
    }

    let mut indegree: HashMap<PathBuf, usize> = HashMap::new();
    let mut adjacency: HashMap<PathBuf, Vec<PathBuf>> = HashMap::new();

    for manifest in manifests {
        indegree.entry(manifest.id.path.to_path_buf()).or_insert(0);
        adjacency.entry(manifest.id.path.to_path_buf()).or_default();
    }

    let mut missing = Vec::new();
    for manifest in manifests {
        for dependency in &manifest.dependencies {
            if !indegree.contains_key(dependency.as_path()) {
                missing.push(format!(
                    "{} depends on missing manifest {}",
                    manifest.id.path.display(),
                    dependency.display()
                ));
                continue;
            }

            adjacency
                .entry(dependency.to_path_buf())
                .or_default()
                .push(manifest.id.path.to_path_buf());

            let Some(entry) = indegree.get_mut(manifest.id.path.as_path()) else {
                return Err(GraphError::Invariant {
                    message: format!(
                        "internal graph error: missing indegree for {}",
                        manifest.id.path.display()
                    ),
                });
            };
            *entry += 1;
        }
    }

    if !missing.is_empty() {
        let details = missing.join("\n  - ");
        return Err(GraphError::MissingNodes { details });
    }

    let mut ready = BTreeSet::new();
    for (path, count) in &indegree {
        if *count == 0 {
            ready.insert(path.clone());
        }
    }

    let mut order = Vec::with_capacity(manifests.len());
    while let Some(next) = ready.pop_first() {
        order.push(next.clone());

        if let Some(neighbors) = adjacency.get(&next) {
            for neighbor in neighbors {
                let Some(entry) = indegree.get_mut(neighbor) else {
                    return Err(GraphError::Invariant {
                        message: "internal graph error: missing neighbor indegree".to_string(),
                    });
                };

                if *entry == 0 {
                    continue;
                }

                *entry -= 1;
                if *entry == 0 {
                    ready.insert(neighbor.clone());
                }
            }
        }
    }

    if order.len() != manifests.len() {
        let mut leftovers: Vec<_> = indegree
            .iter()
            .filter_map(|(path, count)| if *count > 0 { Some(path.clone()) } else { None })
            .collect();
        leftovers.sort();
        let cycle = leftovers
            .into_iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>()
            .join(" -> ");
        return Err(GraphError::CycleDetected { cycle });
    }

    Ok(order)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use std::path::PathBuf;

    use keron_domain::ManifestSpec;

    use super::build_execution_order;

    #[test]
    fn orders_by_dependency_edges() {
        let a = PathBuf::from("/tmp/a.lua");
        let b = PathBuf::from("/tmp/b.lua");

        let one = ManifestSpec::new(a.clone());
        let mut two = ManifestSpec::new(b.clone());
        two.dependencies.push(a.into());

        let ordered = build_execution_order(&[two, one]).expect("order");
        assert_eq!(ordered, vec![PathBuf::from("/tmp/a.lua"), b]);
    }

    #[test]
    fn detects_cycle() {
        let mut one = ManifestSpec::new(PathBuf::from("/tmp/a.lua"));
        let mut two = ManifestSpec::new(PathBuf::from("/tmp/b.lua"));

        one.dependencies.push(PathBuf::from("/tmp/b.lua").into());
        two.dependencies.push(PathBuf::from("/tmp/a.lua").into());

        let err = build_execution_order(&[one, two]).expect_err("must fail");
        assert!(err.to_string().contains("cycle"));
    }
}
