use std::collections::{HashMap, HashSet};

use petgraph::Graph;
use petgraph::graph::NodeIndex;

use super::ModuleId;

/// Reconstruct a directed cycle containing `start` for diagnostics.
// Parent-only visibility makes the graph helper unavailable to sibling modules.
#[allow(clippy::redundant_pub_crate)]
pub(super) fn reconstruct_cycle(graph: &Graph<ModuleId, ()>, start: NodeIndex) -> Vec<ModuleId> {
    let mut stack = vec![start];
    let mut visited: HashSet<NodeIndex> = HashSet::from([start]);
    let mut parents: HashMap<NodeIndex, NodeIndex> = HashMap::new();
    while let Some(node) = stack.pop() {
        for next in graph.neighbors(node) {
            if next == start {
                let mut path = vec![node];
                let mut cursor = node;
                while cursor != start {
                    let Some(&parent) = parents.get(&cursor) else {
                        return vec![graph[start].clone()];
                    };
                    cursor = parent;
                    path.push(cursor);
                }
                path.reverse();
                let mut cycle: Vec<ModuleId> =
                    path.into_iter().map(|index| graph[index].clone()).collect();
                cycle.push(graph[start].clone());
                return cycle;
            }
            if visited.insert(next) {
                parents.insert(next, node);
                stack.push(next);
            }
        }
    }

    vec![graph[start].clone()]
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    #[test]
    fn cycle_returns_singleton_when_no_self_path() {
        let mut graph: Graph<ModuleId, ()> = Graph::new();
        let a = ModuleId(PathBuf::from("/recon-a.keron"));
        let node = graph.add_node(a.clone());
        let cycle = reconstruct_cycle(&graph, node);
        assert_eq!(cycle, vec![a]);
    }

    #[test]
    fn cycle_returns_full_path_when_present() {
        let mut graph: Graph<ModuleId, ()> = Graph::new();
        let a = ModuleId(PathBuf::from("/recon-cycle-a.keron"));
        let b = ModuleId(PathBuf::from("/recon-cycle-b.keron"));
        let a_node = graph.add_node(a.clone());
        let b_node = graph.add_node(b.clone());
        graph.add_edge(a_node, b_node, ());
        graph.add_edge(b_node, a_node, ());
        let cycle = reconstruct_cycle(&graph, a_node);
        assert!(cycle.len() >= 2);
        assert_eq!(cycle.first().unwrap(), cycle.last().unwrap());
        assert_eq!(cycle.first().unwrap(), &a);
        assert!(cycle.contains(&b), "cycle should include b: {cycle:?}");
    }

    #[test]
    fn cycle_handles_deep_graph_without_recursion() {
        let mut graph: Graph<ModuleId, ()> = Graph::new();
        let nodes: Vec<NodeIndex> = (0..20_000)
            .map(|i| graph.add_node(ModuleId(PathBuf::from(format!("/{i}.keron")))))
            .collect();
        for pair in nodes.windows(2) {
            graph.add_edge(pair[0], pair[1], ());
        }
        graph.add_edge(*nodes.last().expect("nodes"), nodes[0], ());

        let cycle = reconstruct_cycle(&graph, nodes[0]);

        assert_eq!(cycle.len(), nodes.len() + 1);
        assert_eq!(cycle.first(), cycle.last());
    }
}
