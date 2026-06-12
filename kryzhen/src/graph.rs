//! Dependency graph: [`topo_sort`] orders migrations so each runs after its `requires`,
//! rejecting cycles ([`Error::Cycle`]) and dangling references
//! ([`Error::DanglingDependency`]). Among ready migrations the smallest name is emitted
//! first, giving a deterministic order.

use crate::types::{Migration, MigrationName};
use crate::{Error, Result};
use std::collections::{HashMap, HashSet};

/// Order migrations so each appears after all its `requires`.
/// Errors on cycles and on requires that reference an unknown migration.
pub fn topo_sort(migrations: Vec<Migration>) -> Result<Vec<Migration>> {
    let mut by_name: HashMap<MigrationName, Migration> = HashMap::new();
    for m in migrations {
        by_name.insert(m.name.clone(), m);
    }

    // Validate all requires resolve.
    for m in by_name.values() {
        for req in &m.requires {
            if !by_name.contains_key(req) {
                return Err(Error::DanglingDependency {
                    migration: m.name.clone(),
                    missing: req.clone(),
                });
            }
        }
    }

    // in-degree = number of unmet requires; dependents = reverse edges.
    let mut indegree: HashMap<MigrationName, usize> = HashMap::new();
    let mut dependents: HashMap<MigrationName, Vec<MigrationName>> = HashMap::new();
    for m in by_name.values() {
        indegree.entry(m.name.clone()).or_insert(0);
        for req in &m.requires {
            *indegree.entry(m.name.clone()).or_insert(0) += 1;
            dependents
                .entry(req.clone())
                .or_default()
                .push(m.name.clone());
        }
    }

    // ready = zero-indegree nodes. Kept sorted descending so `ready.pop()` (last
    // element) yields the lexicographically smallest ready node each step, giving a
    // deterministic, smallest-first order.
    let mut ready: Vec<MigrationName> = indegree
        .iter()
        .filter(|(_, &d)| d == 0)
        .map(|(n, _)| n.clone())
        .collect();
    ready.sort_by(|a, b| b.cmp(a));

    let mut order: Vec<Migration> = Vec::new();
    let mut emitted: HashSet<MigrationName> = HashSet::new();

    while let Some(name) = ready.pop() {
        if !emitted.insert(name.clone()) {
            continue;
        }
        order.push(by_name.get(&name).unwrap().clone());
        if let Some(deps) = dependents.get(&name) {
            for d in deps {
                let e = indegree.get_mut(d).unwrap();
                *e -= 1;
                if *e == 0 {
                    ready.push(d.clone());
                }
            }
        }
        ready.sort_by(|a, b| b.cmp(a));
    }

    if order.len() != by_name.len() {
        let remaining: Vec<MigrationName> = by_name
            .keys()
            .filter(|n| !emitted.contains(*n))
            .cloned()
            .collect();
        return Err(Error::Cycle(remaining));
    }

    Ok(order)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::checksum;

    fn mig(name: &str, requires: &[&str]) -> Migration {
        Migration {
            name: MigrationName(name.into()),
            description: String::new(),
            requires: requires
                .iter()
                .map(|s| MigrationName(s.to_string()))
                .collect(),
            script: String::new(),
            checksum: checksum(""),
        }
    }

    fn names(ms: &[Migration]) -> Vec<String> {
        ms.iter().map(|m| m.name.0.clone()).collect()
    }

    #[test]
    fn orders_dependencies_before_dependents() {
        let out = topo_sort(vec![mig("b", &["a"]), mig("a", &[])]).unwrap();
        assert_eq!(names(&out), vec!["a", "b"]);
    }

    #[test]
    fn independent_roots_emitted_smallest_first() {
        // `a` and `z` have no dependencies, so both are ready from the start; the
        // deterministic tie-break must emit the lexicographically smaller name first.
        let out = topo_sort(vec![mig("z", &[]), mig("a", &[])]).unwrap();
        assert_eq!(names(&out), vec!["a", "z"]);
    }

    #[test]
    fn diamond_orders_root_first_and_sink_last() {
        let out = topo_sort(vec![
            mig("d", &["b", "c"]),
            mig("b", &["a"]),
            mig("c", &["a"]),
            mig("a", &[]),
        ])
        .unwrap();
        let n = names(&out);
        let pos = |x: &str| n.iter().position(|y| y == x).unwrap();
        assert!(pos("a") < pos("b") && pos("a") < pos("c"));
        assert!(pos("b") < pos("d") && pos("c") < pos("d"));
    }

    #[test]
    fn detects_cycle() {
        let err = topo_sort(vec![mig("a", &["b"]), mig("b", &["a"])]).unwrap_err();
        assert!(matches!(err, Error::Cycle(_)));
    }

    #[test]
    fn dangling_dependency_errors() {
        let err = topo_sort(vec![mig("a", &["missing"])]).unwrap_err();
        assert!(matches!(err, Error::DanglingDependency { .. }));
    }
}
