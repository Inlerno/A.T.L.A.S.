//! Module dependency graph (issue #7).
//!
//! Validates the module catalog (duplicate ids, unknown dependencies,
//! self-dependencies, cycles) and provides a deterministic start order
//! (topological, ties broken by id) and the strictly reverse stop order.

use std::collections::{BTreeMap, BTreeSet};

use super::contract::{ModuleDescriptor, ModuleError, ModuleId};

/// `Default` is the empty graph, used when the catalog is invalid (#9).
#[derive(Debug, Clone, Default)]
pub struct ModuleGraph {
    modules: BTreeMap<ModuleId, ModuleDescriptor>,
    /// Direct dependencies of each module.
    dependencies: BTreeMap<ModuleId, BTreeSet<ModuleId>>,
    /// Direct dependents of each module (reverse edges).
    dependents: BTreeMap<ModuleId, BTreeSet<ModuleId>>,
    start_order: Vec<ModuleId>,
}

impl ModuleGraph {
    /// Builds and validates the graph.
    ///
    /// Checks, in this order: duplicate ids, then for each module (by id) a
    /// self-dependency or an unknown dependency, then cycles.
    pub fn build(
        descriptors: impl IntoIterator<Item = ModuleDescriptor>,
    ) -> Result<Self, ModuleError> {
        let mut modules = BTreeMap::new();
        for descriptor in descriptors {
            if modules.contains_key(&descriptor.id) {
                return Err(ModuleError::DuplicateId(descriptor.id));
            }
            modules.insert(descriptor.id.clone(), descriptor);
        }

        let mut dependencies: BTreeMap<ModuleId, BTreeSet<ModuleId>> = BTreeMap::new();
        let mut dependents: BTreeMap<ModuleId, BTreeSet<ModuleId>> = modules
            .keys()
            .map(|id| (id.clone(), BTreeSet::new()))
            .collect();
        for (id, descriptor) in &modules {
            let mut direct = BTreeSet::new();
            for dependency in &descriptor.dependencies {
                if dependency == id {
                    return Err(ModuleError::SelfDependency(id.clone()));
                }
                let Some(reverse) = dependents.get_mut(dependency) else {
                    return Err(ModuleError::MissingDependency {
                        module: id.clone(),
                        dependency: dependency.clone(),
                    });
                };
                reverse.insert(id.clone());
                direct.insert(dependency.clone());
            }
            dependencies.insert(id.clone(), direct);
        }

        if let Some(path) = find_cycle(&dependencies) {
            return Err(ModuleError::DependencyCycle { path });
        }
        let start_order = topological_order(&dependencies, &dependents);

        Ok(Self {
            modules,
            dependencies,
            dependents,
            start_order,
        })
    }

    pub fn contains(&self, id: &ModuleId) -> bool {
        self.modules.contains_key(id)
    }

    pub fn descriptor(&self, id: &ModuleId) -> Option<&ModuleDescriptor> {
        self.modules.get(id)
    }

    /// Direct dependencies of `id` (empty for unknown modules).
    pub fn dependencies(&self, id: &ModuleId) -> impl Iterator<Item = &ModuleId> {
        self.dependencies.get(id).into_iter().flatten()
    }

    /// All modules in start order: dependencies before dependents, ties by id.
    pub fn start_order(&self) -> &[ModuleId] {
        &self.start_order
    }

    /// All modules in stop order: the exact reverse of [`Self::start_order`].
    pub fn shutdown_order(&self) -> Vec<ModuleId> {
        self.start_order.iter().rev().cloned().collect()
    }

    /// Modules of `set` in start order.
    pub fn in_start_order(&self, set: &BTreeSet<ModuleId>) -> Vec<ModuleId> {
        self.start_order
            .iter()
            .filter(|id| set.contains(*id))
            .cloned()
            .collect()
    }

    /// Modules of `set` in stop order (reverse start order).
    pub fn in_shutdown_order(&self, set: &BTreeSet<ModuleId>) -> Vec<ModuleId> {
        let mut order = self.in_start_order(set);
        order.reverse();
        order
    }

    /// Everything `id` depends on, directly or indirectly (excluding `id`).
    pub fn transitive_dependencies(&self, id: &ModuleId) -> BTreeSet<ModuleId> {
        closure(&self.dependencies, id)
    }

    /// Everything that depends on `id`, directly or indirectly (excluding `id`).
    pub fn transitive_dependents(&self, id: &ModuleId) -> BTreeSet<ModuleId> {
        closure(&self.dependents, id)
    }
}

fn closure(edges: &BTreeMap<ModuleId, BTreeSet<ModuleId>>, start: &ModuleId) -> BTreeSet<ModuleId> {
    let mut seen = BTreeSet::new();
    let mut stack: Vec<&ModuleId> = edges.get(start).into_iter().flatten().collect();
    while let Some(id) = stack.pop() {
        if seen.insert(id.clone()) {
            stack.extend(edges.get(id).into_iter().flatten());
        }
    }
    seen
}

/// Returns a cycle as a path that starts and ends with the same module, e.g.
/// `a -> b -> c -> a` (edges point from a module to its dependency).
fn find_cycle(dependencies: &BTreeMap<ModuleId, BTreeSet<ModuleId>>) -> Option<Vec<ModuleId>> {
    #[derive(Clone, Copy, PartialEq)]
    enum Mark {
        InProgress,
        Done,
    }

    fn visit(
        id: &ModuleId,
        dependencies: &BTreeMap<ModuleId, BTreeSet<ModuleId>>,
        marks: &mut BTreeMap<ModuleId, Mark>,
        path: &mut Vec<ModuleId>,
    ) -> Option<Vec<ModuleId>> {
        match marks.get(id) {
            Some(Mark::Done) => return None,
            Some(Mark::InProgress) => {
                let start = path.iter().position(|p| p == id).unwrap_or(0);
                let mut cycle = path[start..].to_vec();
                cycle.push(id.clone());
                return Some(cycle);
            }
            None => {}
        }
        marks.insert(id.clone(), Mark::InProgress);
        path.push(id.clone());
        for dependency in dependencies.get(id).into_iter().flatten() {
            if let Some(cycle) = visit(dependency, dependencies, marks, path) {
                return Some(cycle);
            }
        }
        path.pop();
        marks.insert(id.clone(), Mark::Done);
        None
    }

    let mut marks = BTreeMap::new();
    let mut path = Vec::new();
    dependencies
        .keys()
        .find_map(|id| visit(id, dependencies, &mut marks, &mut path))
}

/// Kahn's algorithm; among ready modules the smallest id goes first.
/// Must only be called on an acyclic graph.
fn topological_order(
    dependencies: &BTreeMap<ModuleId, BTreeSet<ModuleId>>,
    dependents: &BTreeMap<ModuleId, BTreeSet<ModuleId>>,
) -> Vec<ModuleId> {
    let mut remaining: BTreeMap<&ModuleId, usize> = dependencies
        .iter()
        .map(|(id, deps)| (id, deps.len()))
        .collect();
    let mut ready: BTreeSet<&ModuleId> = remaining
        .iter()
        .filter(|(_, count)| **count == 0)
        .map(|(id, _)| *id)
        .collect();
    let mut order = Vec::with_capacity(dependencies.len());
    while let Some(id) = ready.pop_first() {
        order.push(id.clone());
        for dependent in dependents.get(id).into_iter().flatten() {
            if let Some(count) = remaining.get_mut(dependent) {
                *count -= 1;
                if *count == 0 {
                    ready.insert(dependent);
                }
            }
        }
    }
    order
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(s: &str) -> ModuleId {
        ModuleId::new(s).expect("valid id")
    }

    fn module(name: &str, deps: &[&str]) -> ModuleDescriptor {
        ModuleDescriptor {
            id: id(name),
            display_name: name.to_owned(),
            description: String::new(),
            version: "0.1.0".to_owned(),
            dependencies: deps.iter().map(|d| id(d)).collect(),
            default_enabled: false,
        }
    }

    fn ids(list: &[&str]) -> Vec<ModuleId> {
        list.iter().map(|s| id(s)).collect()
    }

    /// Diamond + tail:  t.a <- t.b, t.a <- t.c, {t.b, t.c} <- t.d, t.d <- t.e;
    /// plus an independent t.z. Declared in scrambled order on purpose.
    fn diamond() -> Vec<ModuleDescriptor> {
        vec![
            module("t.e", &["t.d"]),
            module("t.c", &["t.a"]),
            module("t.z", &[]),
            module("t.d", &["t.c", "t.b"]),
            module("t.a", &[]),
            module("t.b", &["t.a"]),
        ]
    }

    #[test]
    fn rejects_duplicate_ids() {
        let err = ModuleGraph::build(vec![module("t.a", &[]), module("t.a", &[])]).unwrap_err();
        assert_eq!(err, ModuleError::DuplicateId(id("t.a")));
    }

    #[test]
    fn rejects_unknown_dependency() {
        let err = ModuleGraph::build(vec![module("t.a", &["t.missing"])]).unwrap_err();
        assert_eq!(
            err,
            ModuleError::MissingDependency {
                module: id("t.a"),
                dependency: id("t.missing"),
            }
        );
    }

    #[test]
    fn rejects_self_dependency() {
        let err = ModuleGraph::build(vec![module("t.a", &["t.a"])]).unwrap_err();
        assert_eq!(err, ModuleError::SelfDependency(id("t.a")));
    }

    #[test]
    fn rejects_cycle_with_path() {
        let err = ModuleGraph::build(vec![
            module("t.a", &["t.b"]),
            module("t.b", &["t.c"]),
            module("t.c", &["t.a"]),
            module("t.x", &[]),
        ])
        .unwrap_err();
        assert_eq!(
            err,
            ModuleError::DependencyCycle {
                path: ids(&["t.a", "t.b", "t.c", "t.a"]),
            }
        );
    }

    #[test]
    fn rejects_cycle_not_reachable_from_first_module() {
        let err = ModuleGraph::build(vec![
            module("t.a", &[]),
            module("t.m", &["t.n"]),
            module("t.n", &["t.m"]),
        ])
        .unwrap_err();
        assert_eq!(
            err,
            ModuleError::DependencyCycle {
                path: ids(&["t.m", "t.n", "t.m"]),
            }
        );
    }

    #[test]
    fn start_order_is_topological_and_deterministic() {
        let graph = ModuleGraph::build(diamond()).expect("valid graph");
        assert_eq!(
            graph.start_order(),
            ids(&["t.a", "t.b", "t.c", "t.d", "t.e", "t.z"]).as_slice()
        );
        // Every dependency precedes its dependent.
        let position = |m: &ModuleId| graph.start_order().iter().position(|x| x == m).unwrap();
        for m in graph.start_order() {
            for dep in graph.dependencies(m) {
                assert!(position(dep) < position(m), "{dep} must start before {m}");
            }
        }
        // Same result regardless of declaration order.
        let mut reversed = diamond();
        reversed.reverse();
        let again = ModuleGraph::build(reversed).expect("valid graph");
        assert_eq!(again.start_order(), graph.start_order());
    }

    #[test]
    fn shutdown_order_is_exact_reverse() {
        let graph = ModuleGraph::build(diamond()).expect("valid graph");
        let mut expected = graph.start_order().to_vec();
        expected.reverse();
        assert_eq!(graph.shutdown_order(), expected);
    }

    #[test]
    fn transitive_closures() {
        let graph = ModuleGraph::build(diamond()).expect("valid graph");
        assert_eq!(
            graph.transitive_dependencies(&id("t.e")),
            ids(&["t.a", "t.b", "t.c", "t.d"]).into_iter().collect()
        );
        assert_eq!(
            graph.transitive_dependents(&id("t.a")),
            ids(&["t.b", "t.c", "t.d", "t.e"]).into_iter().collect()
        );
        assert!(graph.transitive_dependencies(&id("t.a")).is_empty());
        assert!(graph.transitive_dependents(&id("t.z")).is_empty());
    }

    #[test]
    fn descriptor_lookup() {
        let graph = ModuleGraph::build(diamond()).expect("valid graph");
        let descriptor = graph.descriptor(&id("t.d")).expect("known module");
        assert_eq!(descriptor.id, id("t.d"));
        assert_eq!(descriptor.dependencies, ids(&["t.c", "t.b"]));
        assert!(graph.contains(&id("t.d")));
        assert!(graph.descriptor(&id("t.unknown")).is_none());
        assert!(!graph.contains(&id("t.unknown")));
    }

    #[test]
    fn subset_orders() {
        let graph = ModuleGraph::build(diamond()).expect("valid graph");
        let set: BTreeSet<ModuleId> = ids(&["t.e", "t.a", "t.c"]).into_iter().collect();
        assert_eq!(graph.in_start_order(&set), ids(&["t.a", "t.c", "t.e"]));
        assert_eq!(graph.in_shutdown_order(&set), ids(&["t.e", "t.c", "t.a"]));
    }
}
