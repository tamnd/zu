//! Bulk-loaded graph storage: node groups of 131,072 rows, per-group CSR
//! in both directions, and the group directory meta chain.
//!
//! This is the read-optimized COPY path of `docs/04-storage-zu1-format.md`
//! §2 and §4. Each group stores two segments per direction: slot offsets
//! (131,073 monotone values, so delta wins the cascade) and neighbor ids
//! as dense row ids, sorted per list, which is what hits the bits per
//! edge target. Fwd is keyed by source, Bwd by destination, so both
//! out-neighbors and in-neighbors answer without scanning. Slack gaps and
//! the spill chain arrive with the updatable CSR; bulk-built groups are
//! dense.
//!
//! Directory layout (version-prefixed, hand-rolled):
//! `version: u16`, `node_count: u64`, `edge_count: u64`,
//! `group_count: u32`, then per group `row_count: u32` followed by the
//! fwd offsets, fwd neighbors, bwd offsets, and bwd neighbors
//! `SegmentMeta`.
//!
//! Each rel table's directory is its own meta chain, reached through the
//! catalog and the table index of `crate::catalog`, so one file holds any
//! number of named graphs and a bulk load replaces only the table it
//! names.

use std::io::BufRead;
use std::path::Path;

use zu_common::{GROUP_ROWS, Result, ZuError};

use crate::catalog::{Catalog, TableIndex};
use crate::file::{BlockPtr, NULL_BLOCK, Zu1File};
use crate::meta;
use crate::segment::{SegmentMeta, read_range, read_segment, write_segment};

const DIRECTORY_VERSION: u16 = 3;

/// Traversal direction: Fwd follows edges source to destination, Bwd the
/// reverse.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Fwd,
    Bwd,
}

/// One direction of a group's CSR: `row_count + 1` offsets into the
/// neighbor segment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirectionMeta {
    pub offsets: SegmentMeta,
    pub neighbors: SegmentMeta,
}

/// One node group's CSR pair.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupMeta {
    pub row_count: u32,
    pub fwd: DirectionMeta,
    pub bwd: DirectionMeta,
}

impl GroupMeta {
    pub fn dir(&self, dir: Direction) -> &DirectionMeta {
        match dir {
            Direction::Fwd => &self.fwd,
            Direction::Bwd => &self.bwd,
        }
    }
}

/// The per-table group directory, stored as one meta chain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Directory {
    pub node_count: u64,
    pub edge_count: u64,
    pub groups: Vec<GroupMeta>,
}

impl Directory {
    fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&DIRECTORY_VERSION.to_le_bytes());
        out.extend_from_slice(&self.node_count.to_le_bytes());
        out.extend_from_slice(&self.edge_count.to_le_bytes());
        out.extend_from_slice(&(self.groups.len() as u32).to_le_bytes());
        for g in &self.groups {
            out.extend_from_slice(&g.row_count.to_le_bytes());
            g.fwd.offsets.encode(&mut out);
            g.fwd.neighbors.encode(&mut out);
            g.bwd.offsets.encode(&mut out);
            g.bwd.neighbors.encode(&mut out);
        }
        out
    }

    pub(crate) fn decode(bytes: &[u8]) -> Result<Self> {
        let corrupt = |detail: &str| ZuError::Corrupt {
            what: "group directory",
            detail: detail.to_string(),
        };
        let head = bytes.get(..22).ok_or_else(|| corrupt("truncated header"))?;
        let version = u16::from_le_bytes(head[..2].try_into().unwrap());
        if version != DIRECTORY_VERSION {
            return Err(ZuError::Unsupported {
                what: "group directory version",
                id: u32::from(version),
            });
        }
        let node_count = u64::from_le_bytes(head[2..10].try_into().unwrap());
        let edge_count = u64::from_le_bytes(head[10..18].try_into().unwrap());
        let group_count = u32::from_le_bytes(head[18..22].try_into().unwrap()) as usize;
        let mut pos = 22;
        let mut groups = Vec::with_capacity(group_count);
        for _ in 0..group_count {
            let rc = bytes
                .get(pos..pos + 4)
                .ok_or_else(|| corrupt("truncated group entry"))?;
            let row_count = u32::from_le_bytes(rc.try_into().unwrap());
            pos += 4;
            let mut metas = Vec::with_capacity(4);
            for _ in 0..4 {
                let (meta, next) = SegmentMeta::decode(bytes, pos)?;
                metas.push(meta);
                pos = next;
            }
            let mut it = metas.into_iter();
            groups.push(GroupMeta {
                row_count,
                fwd: DirectionMeta {
                    offsets: it.next().unwrap(),
                    neighbors: it.next().unwrap(),
                },
                bwd: DirectionMeta {
                    offsets: it.next().unwrap(),
                    neighbors: it.next().unwrap(),
                },
            });
        }
        if pos != bytes.len() {
            return Err(corrupt("trailing bytes"));
        }
        Ok(Self {
            node_count,
            edge_count,
            groups,
        })
    }
}

/// Reads a whitespace separated `src dst` edge list, the SNAP layout,
/// skipping empty lines and `#` comments.
pub fn read_edge_list(path: &Path) -> Result<Vec<(u32, u32)>> {
    let bad = |line_no: usize| {
        ZuError::InvalidArgument(format!("line {line_no}: expected 'src dst' integers"))
    };
    let file = std::fs::File::open(path)?;
    let mut reader = std::io::BufReader::with_capacity(1 << 20, file);
    let mut edges = Vec::new();
    let mut line = String::new();
    let mut line_no = 0usize;
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            return Ok(edges);
        }
        line_no += 1;
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let mut parts = trimmed.split_ascii_whitespace();
        let src = parts
            .next()
            .and_then(|t| t.parse::<u32>().ok())
            .ok_or_else(|| bad(line_no))?;
        let dst = parts
            .next()
            .and_then(|t| t.parse::<u32>().ok())
            .ok_or_else(|| bad(line_no))?;
        edges.push((src, dst));
    }
}

/// Builds one direction's CSR groups from edges sorted by `(key, other)`.
fn build_direction(
    db: &mut Zu1File,
    node_count: u64,
    edges: &[(u32, u32)],
) -> Result<Vec<DirectionMeta>> {
    #[cfg(debug_assertions)]
    for w in edges.windows(2) {
        debug_assert!(w[0] <= w[1], "edges must be sorted");
    }
    let group_rows = GROUP_ROWS as u64;
    let group_count = node_count.div_ceil(group_rows).max(1) as usize;
    let mut dirs = Vec::with_capacity(group_count);
    let mut edge_ix = 0usize;
    let mut offsets = Vec::new();
    let mut neighbors: Vec<u64> = Vec::new();
    for g in 0..group_count as u64 {
        let first_row = g * group_rows;
        let row_count = (node_count - first_row).min(group_rows) as u32;
        offsets.clear();
        neighbors.clear();
        offsets.push(0);
        for row in 0..u64::from(row_count) {
            let node = (first_row + row) as u32;
            while edge_ix < edges.len() && edges[edge_ix].0 == node {
                neighbors.push(u64::from(edges[edge_ix].1));
                edge_ix += 1;
            }
            offsets.push(neighbors.len() as u64);
        }
        dirs.push(DirectionMeta {
            offsets: write_segment(db, &offsets)?,
            neighbors: write_segment(db, &neighbors)?,
        });
    }
    if edge_ix != edges.len() {
        return Err(ZuError::InvalidArgument(format!(
            "{} edges reference nodes at or above node_count {node_count}",
            edges.len() - edge_ix
        )));
    }
    Ok(dirs)
}

/// Frees every block of the directory chain at `root` plus all four
/// segments per group it lists. The blocks recycle at the next
/// checkpoint per the shadow-publishing rules.
fn free_directory(db: &mut Zu1File, root: BlockPtr) -> Result<()> {
    let directory = Directory::decode(&meta::read_chain(db, root)?)?;
    for group in &directory.groups {
        for seg in [
            &group.fwd.offsets,
            &group.fwd.neighbors,
            &group.bwd.offsets,
            &group.bwd.neighbors,
        ] {
            for &ptr in &seg.blocks {
                db.free_block(ptr)?;
            }
        }
    }
    for ptr in meta::chain_blocks(db, root)? {
        db.free_block(ptr)?;
    }
    Ok(())
}

/// Frees a committed meta chain so a rewritten copy replaces it.
fn free_chain(db: &mut Zu1File, root: BlockPtr) -> Result<()> {
    if root == NULL_BLOCK {
        return Ok(());
    }
    for ptr in meta::chain_blocks(db, root)? {
        db.free_block(ptr)?;
    }
    Ok(())
}

/// Bulk-loads both CSR directions from an edge list into `db` as the
/// default tables `node` and `edge`, publishing them with a checkpoint.
pub fn bulk_load(db: &mut Zu1File, node_count: u64, edges: &[(u32, u32)]) -> Result<Directory> {
    bulk_load_as(db, "node", "edge", node_count, edges)
}

/// Bulk-loads both CSR directions from an edge list into `db` as the rel
/// table `rel_table` over the node table `node_table`, then publishes
/// the catalog, table index, and directory with a checkpoint. `edges`
/// must be sorted by `(src, dst)` and node ids must be dense row ids
/// below `node_count`. The reverse direction is built from an internally
/// sorted `(dst, src)` copy, so peak memory holds the edge list twice.
/// A rel table with the same name is replaced and its blocks recycle one
/// checkpoint later; other tables in the file are untouched. The node
/// table's row domain only grows across loads. Returns the directory.
pub fn bulk_load_as(
    db: &mut Zu1File,
    node_table: &str,
    rel_table: &str,
    node_count: u64,
    edges: &[(u32, u32)],
) -> Result<Directory> {
    let mut catalog = Catalog::load(db)?;
    let mut index = TableIndex::load(db)?;
    if let Some(rel) = catalog.rel_by_name(rel_table) {
        let id = rel.id;
        if let Some(root) = index.get(id) {
            free_directory(db, root)?;
            index.remove(id);
        }
    }
    let fwd = build_direction(db, node_count, edges)?;
    let mut rev: Vec<(u32, u32)> = edges.iter().map(|&(s, d)| (d, s)).collect();
    rev.sort_unstable();
    let bwd = build_direction(db, node_count, &rev)?;
    drop(rev);
    let row_counts = |g: u64| {
        let first_row = g * GROUP_ROWS as u64;
        (node_count - first_row).min(GROUP_ROWS as u64) as u32
    };
    let groups = fwd
        .into_iter()
        .zip(bwd)
        .enumerate()
        .map(|(g, (fwd, bwd))| GroupMeta {
            row_count: row_counts(g as u64),
            fwd,
            bwd,
        })
        .collect();
    let directory = Directory {
        node_count,
        edge_count: edges.len() as u64,
        groups,
    };
    let root = meta::write_chain(db, &directory.encode())?;
    let from = catalog.upsert_node(node_table, node_count)?;
    let rel_id = catalog.upsert_rel(rel_table, from, from, edges.len() as u64)?;
    index.set(rel_id, root);
    // The catalog and index chains are rewritten whole, freeing the
    // committed copies first.
    free_chain(db, db.db_header().catalog_root)?;
    free_chain(db, db.db_header().table_index_root)?;
    let catalog_root = meta::write_chain(db, &catalog.encode())?;
    let index_root = meta::write_chain(db, &index.encode())?;
    db.db_header_mut().catalog_root = catalog_root;
    db.db_header_mut().table_index_root = index_root;
    db.checkpoint()?;
    Ok(directory)
}

/// Read access to a bulk-loaded graph, caching the most recently decoded
/// group and direction so sequential scans decode each group once.
#[derive(Debug)]
pub struct GraphReader {
    directory: Directory,
    cached_group: Option<(usize, Direction, Vec<u64>, Vec<u64>)>,
}

impl GraphReader {
    /// Opens the only rel table in the file, the common single-graph
    /// case. A file holding several rel tables needs [`Self::load_table`]
    /// with a name.
    pub fn load(db: &mut Zu1File) -> Result<Self> {
        let catalog = Catalog::load(db)?;
        match catalog.rel_tables() {
            [rel] => {
                let name = rel.name.clone();
                Self::load_table(db, &name)
            }
            [] => Err(ZuError::InvalidArgument(
                "file holds no rel tables".to_string(),
            )),
            many => Err(ZuError::InvalidArgument(format!(
                "file holds {} rel tables, name one",
                many.len()
            ))),
        }
    }

    /// Opens the rel table called `name` through the catalog and the
    /// table index.
    pub fn load_table(db: &mut Zu1File, name: &str) -> Result<Self> {
        let catalog = Catalog::load(db)?;
        let rel = catalog
            .rel_by_name(name)
            .ok_or_else(|| ZuError::InvalidArgument(format!("no rel table '{name}'")))?;
        let root = TableIndex::load(db)?
            .get(rel.id)
            .ok_or_else(|| ZuError::Corrupt {
                what: "table index",
                detail: format!("rel table '{name}' has no directory entry"),
            })?;
        let bytes = meta::read_chain(db, root)?;
        Ok(Self {
            directory: Directory::decode(&bytes)?,
            cached_group: None,
        })
    }

    pub fn directory(&self) -> &Directory {
        &self.directory
    }

    fn locate(&self, node: u64) -> Result<(usize, usize)> {
        if node >= self.directory.node_count {
            return Err(ZuError::InvalidArgument(format!(
                "node {node} out of range 0..{}",
                self.directory.node_count
            )));
        }
        Ok((
            (node / GROUP_ROWS as u64) as usize,
            (node % GROUP_ROWS as u64) as usize,
        ))
    }

    /// Returns `node`'s sorted list in `dir`, decoding the node's group
    /// on a cache miss.
    pub fn neighbors_dir(&mut self, db: &mut Zu1File, node: u64, dir: Direction) -> Result<&[u64]> {
        let (g, row) = self.locate(node)?;
        if self.cached_group.as_ref().map(|(i, d, _, _)| (*i, *d)) != Some((g, dir)) {
            let meta = self.directory.groups[g].dir(dir);
            let mut offsets = Vec::with_capacity(meta.offsets.value_count as usize);
            let mut nbrs = Vec::with_capacity(meta.neighbors.value_count as usize);
            read_segment(db, &meta.offsets, &mut offsets)?;
            read_segment(db, &meta.neighbors, &mut nbrs)?;
            self.cached_group = Some((g, dir, offsets, nbrs));
        }
        let (_, _, offsets, nbrs) = self.cached_group.as_ref().unwrap();
        let lo = offsets[row] as usize;
        let hi = offsets[row + 1] as usize;
        Ok(&nbrs[lo..hi])
    }

    /// Returns the sorted out-neighbor list of `node`.
    pub fn neighbors(&mut self, db: &mut Zu1File, node: u64) -> Result<&[u64]> {
        self.neighbors_dir(db, node, Direction::Fwd)
    }

    /// Point access: appends `node`'s sorted list in `dir` to `out`
    /// without decoding the group. Two offset values locate the list,
    /// then only the chunks covering it are read, so a 1-hop read
    /// touches at most `2 + ceil(degree / 1024) + 1` chunk decodes and
    /// bytes on that order rather than the group's megabytes.
    pub fn neighbors_dir_into(
        &self,
        db: &mut Zu1File,
        node: u64,
        dir: Direction,
        out: &mut Vec<u64>,
    ) -> Result<()> {
        let (g, row) = self.locate(node)?;
        let meta = self.directory.groups[g].dir(dir);
        let mut offs = Vec::with_capacity(2);
        read_range(db, &meta.offsets, row as u64, row as u64 + 2, &mut offs)?;
        read_range(db, &meta.neighbors, offs[0], offs[1], out)
    }

    /// Point access to the out-neighbor list.
    pub fn neighbors_into(&self, db: &mut Zu1File, node: u64, out: &mut Vec<u64>) -> Result<()> {
        self.neighbors_dir_into(db, node, Direction::Fwd, out)
    }

    /// Point access to the in-neighbor list.
    pub fn in_neighbors_into(&self, db: &mut Zu1File, node: u64, out: &mut Vec<u64>) -> Result<()> {
        self.neighbors_dir_into(db, node, Direction::Bwd, out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sorted_edges(edges: &mut Vec<(u32, u32)>) -> &[(u32, u32)] {
        edges.sort_unstable();
        edges.dedup();
        edges
    }

    #[test]
    fn roundtrip_small_graph_both_directions() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("g.zu1");
        let mut edges = vec![(0u32, 1u32), (0, 3), (1, 2), (3, 0), (3, 1), (4, 4)];
        {
            let mut db = Zu1File::create(&path).unwrap();
            let d = bulk_load(&mut db, 5, sorted_edges(&mut edges)).unwrap();
            assert_eq!(d.edge_count, 6);
            assert_eq!(d.groups.len(), 1);
        }
        let mut db = Zu1File::open(&path).unwrap();
        let mut reader = GraphReader::load(&mut db).unwrap();
        assert_eq!(reader.neighbors(&mut db, 0).unwrap(), &[1, 3]);
        assert_eq!(reader.neighbors(&mut db, 1).unwrap(), &[2]);
        assert_eq!(reader.neighbors(&mut db, 2).unwrap(), &[] as &[u64]);
        assert_eq!(reader.neighbors(&mut db, 3).unwrap(), &[0, 1]);
        assert_eq!(reader.neighbors(&mut db, 4).unwrap(), &[4]);
        assert!(reader.neighbors(&mut db, 5).is_err());
        // In-neighbors: who points at each node.
        let cases: &[(u64, &[u64])] = &[(0, &[3]), (1, &[0, 3]), (2, &[1]), (3, &[0]), (4, &[4])];
        for &(node, want) in cases {
            assert_eq!(
                reader.neighbors_dir(&mut db, node, Direction::Bwd).unwrap(),
                want,
                "in-neighbors of {node}"
            );
            let mut point = Vec::new();
            reader.in_neighbors_into(&mut db, node, &mut point).unwrap();
            assert_eq!(point, want, "point in-neighbors of {node}");
        }
    }

    #[test]
    fn multi_group_boundaries() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("g.zu1");
        let rows = GROUP_ROWS;
        let node_count = u64::from(rows) * 2 + 10;
        // Edges around the group boundary and in the short tail group.
        let mut edges = vec![
            (rows - 1, 0),
            (rows - 1, rows),
            (rows, rows - 1),
            (rows, rows + 1),
            (2 * rows + 9, 2),
        ];
        {
            let mut db = Zu1File::create(&path).unwrap();
            let d = bulk_load(&mut db, node_count, sorted_edges(&mut edges)).unwrap();
            assert_eq!(d.groups.len(), 3);
            assert_eq!(d.groups[2].row_count, 10);
        }
        let mut db = Zu1File::open(&path).unwrap();
        let mut reader = GraphReader::load(&mut db).unwrap();
        assert_eq!(
            reader.neighbors(&mut db, u64::from(rows) - 1).unwrap(),
            &[0, u64::from(rows)]
        );
        assert_eq!(
            reader.neighbors(&mut db, u64::from(rows)).unwrap(),
            &[u64::from(rows) - 1, u64::from(rows) + 1]
        );
        assert_eq!(reader.neighbors(&mut db, node_count - 1).unwrap(), &[2]);
        assert_eq!(reader.neighbors(&mut db, 5).unwrap(), &[] as &[u64]);
        // Cross-group in-neighbors: node 0 is pointed at by rows - 1, node
        // 2 by the last node.
        assert_eq!(
            reader.neighbors_dir(&mut db, 0, Direction::Bwd).unwrap(),
            &[u64::from(rows) - 1]
        );
        assert_eq!(
            reader.neighbors_dir(&mut db, 2, Direction::Bwd).unwrap(),
            &[node_count - 1]
        );
    }

    #[test]
    fn out_of_range_edges_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = Zu1File::create(&dir.path().join("g.zu1")).unwrap();
        let edges = [(0u32, 1u32), (9, 0)];
        assert!(matches!(
            bulk_load(&mut db, 5, &edges),
            Err(ZuError::InvalidArgument(_))
        ));
        // A destination out of range must fail too, via the bwd build.
        let mut db2 = Zu1File::create(&dir.path().join("g2.zu1")).unwrap();
        let edges = [(0u32, 1u32), (1, 9)];
        assert!(matches!(
            bulk_load(&mut db2, 5, &edges),
            Err(ZuError::InvalidArgument(_))
        ));
    }

    #[test]
    fn random_graph_matches_reference() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("g.zu1");
        let n = 5000u32;
        let mut rng = 0x5EEDu64;
        let mut edges: Vec<(u32, u32)> = (0..60_000)
            .map(|_| {
                rng ^= rng << 13;
                rng ^= rng >> 7;
                rng ^= rng << 17;
                (
                    (rng % u64::from(n)) as u32,
                    ((rng >> 32) % u64::from(n)) as u32,
                )
            })
            .collect();
        let edges = sorted_edges(&mut edges);
        let mut out_ref: Vec<Vec<u64>> = vec![Vec::new(); n as usize];
        let mut in_ref: Vec<Vec<u64>> = vec![Vec::new(); n as usize];
        for &(s, d) in edges.iter() {
            out_ref[s as usize].push(u64::from(d));
            in_ref[d as usize].push(u64::from(s));
        }
        for l in &mut in_ref {
            l.sort_unstable();
        }
        {
            let mut db = Zu1File::create(&path).unwrap();
            bulk_load(&mut db, u64::from(n), edges).unwrap();
        }
        let mut db = Zu1File::open(&path).unwrap();
        let mut reader = GraphReader::load(&mut db).unwrap();
        let mut point = Vec::new();
        for v in 0..u64::from(n) {
            assert_eq!(
                reader.neighbors(&mut db, v).unwrap(),
                out_ref[v as usize].as_slice(),
                "node {v}"
            );
            point.clear();
            reader.neighbors_into(&mut db, v, &mut point).unwrap();
            assert_eq!(point, out_ref[v as usize], "point read node {v}");
            point.clear();
            reader.in_neighbors_into(&mut db, v, &mut point).unwrap();
            assert_eq!(point, in_ref[v as usize], "point in read node {v}");
        }
        // The full-decode bwd path against the same reference, exercising
        // the (group, direction) cache.
        for v in 0..u64::from(n) {
            assert_eq!(
                reader.neighbors_dir(&mut db, v, Direction::Bwd).unwrap(),
                in_ref[v as usize].as_slice(),
                "in node {v}"
            );
        }
        assert!(
            reader
                .neighbors_into(&mut db, u64::from(n), &mut point)
                .is_err()
        );
    }

    #[test]
    fn named_tables_share_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("g.zu1");
        let mut follows = vec![(0u32, 1u32), (1, 2), (2, 0)];
        let mut likes = vec![(0u32, 2u32), (3, 1)];
        {
            let mut db = Zu1File::create(&path).unwrap();
            bulk_load_as(&mut db, "person", "follows", 3, sorted_edges(&mut follows)).unwrap();
            bulk_load_as(&mut db, "person", "likes", 4, sorted_edges(&mut likes)).unwrap();
        }
        let mut db = Zu1File::open(&path).unwrap();
        // Two rel tables: loading without a name must fail, naming works.
        assert!(GraphReader::load(&mut db).is_err());
        assert!(GraphReader::load_table(&mut db, "nope").is_err());
        let mut r = GraphReader::load_table(&mut db, "follows").unwrap();
        assert_eq!(r.neighbors(&mut db, 0).unwrap(), &[1]);
        assert_eq!(r.neighbors_dir(&mut db, 0, Direction::Bwd).unwrap(), &[2]);
        let mut r = GraphReader::load_table(&mut db, "likes").unwrap();
        assert_eq!(r.neighbors(&mut db, 3).unwrap(), &[1]);
        assert_eq!(r.neighbors_dir(&mut db, 2, Direction::Bwd).unwrap(), &[0]);
        // The shared node table grew to the larger row domain.
        let catalog = crate::catalog::Catalog::load(&mut db).unwrap();
        assert_eq!(catalog.node_by_name("person").unwrap().node_count, 4);
        assert_eq!(catalog.rel_tables().len(), 2);
        // Replacing one rel table leaves the other untouched.
        let mut third = vec![(1u32, 0u32)];
        bulk_load_as(&mut db, "person", "likes", 4, sorted_edges(&mut third)).unwrap();
        let mut r = GraphReader::load_table(&mut db, "likes").unwrap();
        assert_eq!(r.directory().edge_count, 1);
        assert_eq!(r.neighbors(&mut db, 1).unwrap(), &[0]);
        let mut r = GraphReader::load_table(&mut db, "follows").unwrap();
        assert_eq!(r.neighbors(&mut db, 2).unwrap(), &[0]);
        drop(db);
        crate::verify(&path).unwrap();
        // A fresh file holds no rel tables at all.
        let mut empty = Zu1File::create(&dir.path().join("e.zu1")).unwrap();
        assert!(GraphReader::load(&mut empty).is_err());
    }

    #[test]
    fn rebuild_recycles_blocks() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("g.zu1");
        let make_edges = |salt: u32| {
            let mut edges: Vec<(u32, u32)> = (0..4000u32)
                .map(|i| (i.wrapping_mul(31).wrapping_add(salt) % 500, i % 500))
                .collect();
            edges.sort_unstable();
            edges.dedup();
            edges
        };
        let mut db = Zu1File::create(&path).unwrap();
        // Build 2 frees build 1 but cannot reuse its blocks: they are the
        // committed graph while build 2 is written. Build 3 reuses build
        // 1's blocks and the allocator reaches steady state, so from
        // build 4 on the file stops growing.
        for salt in 0..3 {
            bulk_load(&mut db, 500, &make_edges(salt)).unwrap();
        }
        let watermark = db.db_header().block_count;
        for salt in 3..7 {
            bulk_load(&mut db, 500, &make_edges(salt)).unwrap();
            assert_eq!(
                db.db_header().block_count,
                watermark,
                "build {salt} grew the file"
            );
        }
        // The surviving graph is the last one written, in both directions.
        drop(db);
        let mut db = Zu1File::open(&path).unwrap();
        let reader = GraphReader::load(&mut db).unwrap();
        let edges = make_edges(6);
        let mut out_ref: Vec<Vec<u64>> = vec![Vec::new(); 500];
        let mut in_ref: Vec<Vec<u64>> = vec![Vec::new(); 500];
        for &(s, d) in &edges {
            out_ref[s as usize].push(u64::from(d));
            in_ref[d as usize].push(u64::from(s));
        }
        for l in &mut in_ref {
            l.sort_unstable();
        }
        let mut point = Vec::new();
        for v in 0..500u64 {
            point.clear();
            reader.neighbors_into(&mut db, v, &mut point).unwrap();
            assert_eq!(point, out_ref[v as usize], "out node {v}");
            point.clear();
            reader.in_neighbors_into(&mut db, v, &mut point).unwrap();
            assert_eq!(point, in_ref[v as usize], "in node {v}");
        }
        assert_eq!(reader.directory().edge_count, edges.len() as u64);
    }

    #[test]
    fn point_reads_cross_chunk_and_group_boundaries() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("g.zu1");
        let rows = GROUP_ROWS;
        let node_count = u64::from(rows) + 5;
        // A hub whose list spans several 1024-value chunks, rows sitting
        // exactly on chunk boundaries of the offsets segment, and a node
        // in the tail group.
        let mut edges: Vec<(u32, u32)> = (0..3000).map(|d| (7u32, d * 2)).collect();
        edges.push((1023, 1));
        edges.push((1024, 2));
        edges.push((rows, 3));
        {
            let mut db = Zu1File::create(&path).unwrap();
            bulk_load(&mut db, node_count, sorted_edges(&mut edges)).unwrap();
        }
        let mut db = Zu1File::open(&path).unwrap();
        let mut reader = GraphReader::load(&mut db).unwrap();
        for node in [7u64, 1023, 1024, u64::from(rows), 0, node_count - 1] {
            for dir in [Direction::Fwd, Direction::Bwd] {
                let want = reader.neighbors_dir(&mut db, node, dir).unwrap().to_vec();
                let mut got = Vec::new();
                reader
                    .neighbors_dir_into(&mut db, node, dir, &mut got)
                    .unwrap();
                assert_eq!(got, want, "node {node} {dir:?}");
            }
        }
    }
}
