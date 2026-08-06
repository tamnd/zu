//! Compact in-memory CSR adjacency, the shared substrate for the graph
//! kernels in `kernels.rs` and, later, the `Expand`/`MultiwayIntersect`
//! operators of `docs/07-query-engine.md`.
//!
//! Neighbor lists are sorted and deduplicated, which is what gives the
//! WCOJ intersection its tries for free and keeps binary search valid
//! inside `Expand`. The builder buffers raw edges and performs a single
//! global sort at build time, never a per-vertex sort.

/// Compressed sparse row adjacency over `u32` node ids.
///
/// `offsets` has `node_count + 1` entries and
/// `neighbors[offsets[v]..offsets[v + 1]]` is the sorted, deduplicated
/// neighbor list of `v`.
#[derive(Debug, Clone)]
pub struct Csr {
    offsets: Vec<u64>,
    neighbors: Vec<u32>,
}

impl Csr {
    /// Builds an out-edge CSR from an edge list; parallel edges collapse.
    pub fn from_edges(node_count: u32, edges: Vec<(u32, u32)>) -> Self {
        let mut builder = CsrBuilder::new(node_count);
        for (src, dst) in edges {
            builder.push(src, dst);
        }
        builder.build()
    }

    /// Builds the reversed CSR of the same edge set, serving in-edges.
    pub fn reversed(&self) -> Self {
        let mut builder = CsrBuilder::new(self.node_count());
        for v in 0..self.node_count() {
            for &w in self.neighbors(v) {
                builder.push(w, v);
            }
        }
        builder.build()
    }

    /// Number of nodes, fixed at build time.
    pub fn node_count(&self) -> u32 {
        (self.offsets.len() - 1) as u32
    }

    /// Number of stored (deduplicated) edges.
    pub fn edge_count(&self) -> u64 {
        self.neighbors.len() as u64
    }

    /// Sorted neighbor list of `v`.
    pub fn neighbors(&self, v: u32) -> &[u32] {
        let lo = self.offsets[v as usize] as usize;
        let hi = self.offsets[v as usize + 1] as usize;
        &self.neighbors[lo..hi]
    }

    /// Out-degree of `v` (in-degree when the CSR is a reversed build).
    pub fn degree(&self, v: u32) -> u32 {
        (self.offsets[v as usize + 1] - self.offsets[v as usize]) as u32
    }
}

/// Streaming CSR builder: `push` only buffers, `build` sorts the whole
/// edge list once, deduplicates, and lays out offsets in one pass.
#[derive(Debug)]
pub struct CsrBuilder {
    node_count: u32,
    edges: Vec<(u32, u32)>,
}

impl CsrBuilder {
    /// Creates a builder for a graph with `node_count` nodes.
    pub fn new(node_count: u32) -> Self {
        Self {
            node_count,
            edges: Vec::new(),
        }
    }

    /// Buffers one directed edge; both endpoints must be in range.
    pub fn push(&mut self, src: u32, dst: u32) {
        assert!(
            src < self.node_count && dst < self.node_count,
            "edge ({src}, {dst}) out of range for {} nodes",
            self.node_count
        );
        self.edges.push((src, dst));
    }

    /// Sorts once, drops parallel edges, and freezes the CSR.
    pub fn build(mut self) -> Csr {
        self.edges.sort_unstable();
        self.edges.dedup();
        let mut offsets = vec![0u64; self.node_count as usize + 1];
        for &(src, _) in &self.edges {
            offsets[src as usize + 1] += 1;
        }
        let mut total = 0u64;
        for o in &mut offsets[1..] {
            total += *o;
            *o = total;
        }
        let neighbors = self.edges.into_iter().map(|(_, dst)| dst).collect();
        Csr { offsets, neighbors }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_sorts_and_dedups() {
        let edges = vec![(2, 1), (0, 3), (2, 1), (2, 0), (0, 1), (0, 3)];
        let csr = Csr::from_edges(4, edges);
        assert_eq!(csr.node_count(), 4);
        assert_eq!(csr.edge_count(), 4);
        assert_eq!(csr.neighbors(0), &[1, 3]);
        assert_eq!(csr.neighbors(1), &[] as &[u32]);
        assert_eq!(csr.neighbors(2), &[0, 1]);
        assert_eq!(csr.neighbors(3), &[] as &[u32]);
        assert_eq!(csr.degree(0), 2);
        assert_eq!(csr.degree(2), 2);
        assert_eq!(csr.degree(3), 0);
    }

    #[test]
    fn reversed_serves_in_edges() {
        let csr = Csr::from_edges(4, vec![(0, 1), (2, 1), (0, 3), (3, 0)]);
        let rev = csr.reversed();
        assert_eq!(rev.node_count(), 4);
        assert_eq!(rev.edge_count(), 4);
        assert_eq!(rev.neighbors(1), &[0, 2]);
        assert_eq!(rev.neighbors(0), &[3]);
        assert_eq!(rev.neighbors(3), &[0]);
        assert_eq!(rev.neighbors(2), &[] as &[u32]);
    }

    #[test]
    fn empty_and_isolated_nodes() {
        let empty = Csr::from_edges(0, Vec::new());
        assert_eq!(empty.node_count(), 0);
        assert_eq!(empty.edge_count(), 0);

        let isolated = Csr::from_edges(3, vec![(1, 1)]);
        assert_eq!(isolated.degree(0), 0);
        assert_eq!(isolated.degree(1), 1);
        assert_eq!(isolated.degree(2), 0);
        assert_eq!(isolated.neighbors(1), &[1]);
    }
}
