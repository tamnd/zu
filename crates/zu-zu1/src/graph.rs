//! Bulk-loaded graph storage: node groups of 131,072 rows, per-group CSR
//! with sorted neighbor lists, and the group directory meta chain.
//!
//! This is the read-optimized COPY path of `docs/04-storage-zu1-format.md`
//! §2 and §4. Each group stores two segments per direction: slot offsets
//! (131,073 monotone values, so delta wins the cascade) and neighbor ids
//! as dense row ids, sorted per list, which is what hits the bits per
//! edge target. Slack gaps and the spill chain arrive with the updatable
//! CSR; bulk-built groups are dense.
//!
//! Directory layout (version-prefixed, hand-rolled):
//! `version: u16`, `node_count: u64`, `edge_count: u64`,
//! `group_count: u32`, then per group `row_count: u32` followed by the
//! offsets and neighbors `SegmentMeta`.

use std::io::BufRead;
use std::path::Path;

use zu_common::{GROUP_ROWS, Result, ZuError};

use crate::file::Zu1File;
use crate::meta;
use crate::segment::{SegmentMeta, read_range, read_segment, write_segment};

const DIRECTORY_VERSION: u16 = 1;

/// One node group's forward CSR: `row_count + 1` offsets into the
/// neighbor segment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupMeta {
    pub row_count: u32,
    pub offsets: SegmentMeta,
    pub neighbors: SegmentMeta,
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
            g.offsets.encode(&mut out);
            g.neighbors.encode(&mut out);
        }
        out
    }

    fn decode(bytes: &[u8]) -> Result<Self> {
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
            let (offsets, next) = SegmentMeta::decode(bytes, pos)?;
            let (neighbors, end) = SegmentMeta::decode(bytes, next)?;
            pos = end;
            groups.push(GroupMeta {
                row_count,
                offsets,
                neighbors,
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

/// Bulk-loads a forward CSR from an edge list into `db` and publishes it
/// with a checkpoint. `edges` must be sorted by `(src, dst)` and node ids
/// must be dense row ids below `node_count`. Returns the directory.
pub fn bulk_load(db: &mut Zu1File, node_count: u64, edges: &[(u32, u32)]) -> Result<Directory> {
    for w in edges.windows(2) {
        debug_assert!(w[0] <= w[1], "edges must be sorted");
    }
    let group_rows = GROUP_ROWS as u64;
    let group_count = node_count.div_ceil(group_rows).max(1) as usize;
    let mut groups = Vec::with_capacity(group_count);
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
        let offsets_meta = write_segment(db, &offsets)?;
        let neighbors_meta = write_segment(db, &neighbors)?;
        groups.push(GroupMeta {
            row_count,
            offsets: offsets_meta,
            neighbors: neighbors_meta,
        });
    }
    if edge_ix != edges.len() {
        return Err(ZuError::InvalidArgument(format!(
            "{} edges reference nodes at or above node_count {node_count}",
            edges.len() - edge_ix
        )));
    }
    let directory = Directory {
        node_count,
        edge_count: edges.len() as u64,
        groups,
    };
    let root = meta::write_chain(db, &directory.encode())?;
    db.db_header_mut().table_index_root = root;
    db.checkpoint()?;
    Ok(directory)
}

/// Read access to a bulk-loaded graph, caching the most recently decoded
/// group so sequential scans decode each group once.
#[derive(Debug)]
pub struct GraphReader {
    directory: Directory,
    cached_group: Option<(usize, Vec<u64>, Vec<u64>)>,
}

impl GraphReader {
    /// Loads the directory from the committed table index root.
    pub fn load(db: &mut Zu1File) -> Result<Self> {
        let root = db.db_header().table_index_root;
        let bytes = meta::read_chain(db, root)?;
        Ok(Self {
            directory: Directory::decode(&bytes)?,
            cached_group: None,
        })
    }

    pub fn directory(&self) -> &Directory {
        &self.directory
    }

    /// Returns the sorted neighbor list of `node`, decoding the node's
    /// group on a cache miss.
    pub fn neighbors(&mut self, db: &mut Zu1File, node: u64) -> Result<&[u64]> {
        if node >= self.directory.node_count {
            return Err(ZuError::InvalidArgument(format!(
                "node {node} out of range 0..{}",
                self.directory.node_count
            )));
        }
        let g = (node / GROUP_ROWS as u64) as usize;
        let row = (node % GROUP_ROWS as u64) as usize;
        if self.cached_group.as_ref().map(|(i, _, _)| *i) != Some(g) {
            let meta = &self.directory.groups[g];
            let mut offsets = Vec::with_capacity(meta.offsets.value_count as usize);
            let mut nbrs = Vec::with_capacity(meta.neighbors.value_count as usize);
            read_segment(db, &meta.offsets, &mut offsets)?;
            read_segment(db, &meta.neighbors, &mut nbrs)?;
            self.cached_group = Some((g, offsets, nbrs));
        }
        let (_, offsets, nbrs) = self.cached_group.as_ref().unwrap();
        let lo = offsets[row] as usize;
        let hi = offsets[row + 1] as usize;
        Ok(&nbrs[lo..hi])
    }

    /// Point access: appends `node`'s sorted neighbor list to `out`
    /// without decoding the group. Two offset values locate the list,
    /// then only the chunks covering it are read, so a 1-hop read
    /// touches at most `2 + ceil(degree / 1024) + 1` chunk decodes and
    /// bytes on that order rather than the group's megabytes.
    pub fn neighbors_into(&self, db: &mut Zu1File, node: u64, out: &mut Vec<u64>) -> Result<()> {
        if node >= self.directory.node_count {
            return Err(ZuError::InvalidArgument(format!(
                "node {node} out of range 0..{}",
                self.directory.node_count
            )));
        }
        let g = (node / GROUP_ROWS as u64) as usize;
        let row = node % GROUP_ROWS as u64;
        let meta = &self.directory.groups[g];
        let mut offs = Vec::with_capacity(2);
        read_range(db, &meta.offsets, row, row + 2, &mut offs)?;
        read_range(db, &meta.neighbors, offs[0], offs[1], out)
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
    fn roundtrip_small_graph() {
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
        let mut reference: Vec<Vec<u64>> = vec![Vec::new(); n as usize];
        for &(s, d) in edges.iter() {
            reference[s as usize].push(u64::from(d));
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
                reference[v as usize].as_slice(),
                "node {v}"
            );
            point.clear();
            reader.neighbors_into(&mut db, v, &mut point).unwrap();
            assert_eq!(point, reference[v as usize], "point read node {v}");
        }
        assert!(
            reader
                .neighbors_into(&mut db, u64::from(n), &mut point)
                .is_err()
        );
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
            let want = reader.neighbors(&mut db, node).unwrap().to_vec();
            let mut got = Vec::new();
            reader.neighbors_into(&mut db, node, &mut got).unwrap();
            assert_eq!(got, want, "node {node}");
        }
    }
}
