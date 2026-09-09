//! Deterministic symbol-level dependency graph for RELC.

use std::collections::{BTreeMap, BTreeSet};

use crate::source_registry::SourceId;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SymbolId {
    pub source: SourceId,
    pub name: String,
}

impl SymbolId {
    pub fn new(source: SourceId, name: impl Into<String>) -> Self {
        Self {
            source,
            name: name.into(),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct SymbolDependencyGraph {
    edges: BTreeMap<SymbolId, BTreeSet<SymbolId>>,
}

impl SymbolDependencyGraph {
    pub fn add_symbol(&mut self, symbol: SymbolId) {
        self.edges.entry(symbol).or_default();
    }

    pub fn add_edge(&mut self, from: SymbolId, to: SymbolId) {
        self.edges.entry(to.clone()).or_default();
        self.edges.entry(from).or_default().insert(to);
    }

    pub fn dependencies(&self, symbol: &SymbolId) -> impl Iterator<Item = &SymbolId> {
        self.edges.get(symbol).into_iter().flatten()
    }

    pub fn symbols(&self) -> impl Iterator<Item = &SymbolId> {
        self.edges.keys()
    }

    pub fn edge_count(&self) -> usize {
        self.edges.values().map(BTreeSet::len).sum()
    }

    /// Returns strongly-connected recursive groups. A one-node SCC is only
    /// recursive when the symbol has an explicit self edge.
    pub fn recursive_groups(&self) -> Vec<Vec<SymbolId>> {
        let mut index = 0usize;
        let mut indices = BTreeMap::<SymbolId, usize>::new();
        let mut lowlink = BTreeMap::<SymbolId, usize>::new();
        let mut stack = Vec::<SymbolId>::new();
        let mut on_stack = BTreeSet::<SymbolId>::new();
        let mut groups = Vec::new();

        for symbol in self.edges.keys() {
            if !indices.contains_key(symbol) {
                self.strong_connect(
                    symbol.clone(),
                    &mut index,
                    &mut indices,
                    &mut lowlink,
                    &mut stack,
                    &mut on_stack,
                    &mut groups,
                );
            }
        }

        groups.retain(|group| {
            group.len() > 1
                || group.first().is_some_and(|symbol| {
                    self.edges
                        .get(symbol)
                        .is_some_and(|edges| edges.contains(symbol))
                })
        });
        for group in &mut groups {
            group.sort();
        }
        groups.sort();
        groups
    }

    #[allow(clippy::too_many_arguments)]
    fn strong_connect(
        &self,
        symbol: SymbolId,
        index: &mut usize,
        indices: &mut BTreeMap<SymbolId, usize>,
        lowlink: &mut BTreeMap<SymbolId, usize>,
        stack: &mut Vec<SymbolId>,
        on_stack: &mut BTreeSet<SymbolId>,
        groups: &mut Vec<Vec<SymbolId>>,
    ) {
        let current_index = *index;
        *index += 1;
        indices.insert(symbol.clone(), current_index);
        lowlink.insert(symbol.clone(), current_index);
        stack.push(symbol.clone());
        on_stack.insert(symbol.clone());

        if let Some(dependencies) = self.edges.get(&symbol) {
            for dependency in dependencies {
                if !indices.contains_key(dependency) {
                    self.strong_connect(
                        dependency.clone(),
                        index,
                        indices,
                        lowlink,
                        stack,
                        on_stack,
                        groups,
                    );
                    let dependency_low = lowlink[dependency];
                    let symbol_low = lowlink[&symbol];
                    lowlink.insert(symbol.clone(), symbol_low.min(dependency_low));
                } else if on_stack.contains(dependency) {
                    let dependency_index = indices[dependency];
                    let symbol_low = lowlink[&symbol];
                    lowlink.insert(symbol.clone(), symbol_low.min(dependency_index));
                }
            }
        }

        if lowlink[&symbol] == indices[&symbol] {
            let mut group = Vec::new();
            while let Some(item) = stack.pop() {
                on_stack.remove(&item);
                group.push(item.clone());
                if item == symbol {
                    break;
                }
            }
            groups.push(group);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source_registry::RelSourceKind;

    fn symbol(source: &str, name: &str) -> SymbolId {
        SymbolId::new(
            SourceId::physical(RelSourceKind::Module, source).unwrap(),
            name,
        )
    }

    #[test]
    fn import_like_cycle_is_not_recursive_without_symbol_cycle() {
        let a = symbol("A", "FUNC_A");
        let b = symbol("B", "lookup");
        let c = symbol("C", "inspect");
        let a_other = symbol("A", "FUNC_B");
        let mut graph = SymbolDependencyGraph::default();
        graph.add_edge(a, b.clone());
        graph.add_edge(b, c.clone());
        graph.add_edge(c, a_other);
        assert!(graph.recursive_groups().is_empty());
    }

    #[test]
    fn mutually_recursive_symbols_form_one_group() {
        let a = symbol("A", "walk");
        let b = symbol("B", "walk");
        let mut graph = SymbolDependencyGraph::default();
        graph.add_edge(a.clone(), b.clone());
        graph.add_edge(b, a);
        assert_eq!(graph.recursive_groups().len(), 1);
        assert_eq!(graph.recursive_groups()[0].len(), 2);
    }
}