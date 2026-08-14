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
//! `props: BlockPtr`, `group_count: u32`, `has_keys: u8`, then when
//! `has_keys` is 1 the key and row `SegmentMeta` of the primary-key
//! index, then per group `row_count: u32`, `edge_base: u64`, and the fwd
//! offsets, fwd neighbors, bwd offsets, and bwd neighbors `SegmentMeta`.
//!
//! Each rel table's directory is its own meta chain, reached through the
//! catalog and the table index of `crate::catalog`, so one file holds any
//! number of named graphs and a bulk load replaces only the table it
//! names.
//!
//! Edges carry properties the way nodes do, through a props directory of
//! `crate::props`, hung off `props` here rather than off the table index,
//! whose entry for a rel id is this directory. The row domain of those
//! columns is the edge ordinal: the position of an edge in the sorted
//! load order, which is also its position in the forward neighbor arrays
//! read group after group, so `edge_base` plus the slot a destination
//! sits in names the row without anything being stored per edge to say
//! so. Reading a property backward costs the search that finds the slot
//! (see [`GraphReader::edge_ordinal`]), and nothing in either direction
//! costs a permutation on disk.

use std::io::BufRead;
use std::path::Path;
use std::sync::Arc;

use zu_common::{GROUP_ROWS, Result, ZuError};

use crate::catalog::{Catalog, TableIndex};
use crate::file::{BlockPtr, NULL_BLOCK, Zu1File};
use crate::keys::{KeyIndex, KeyReader, write_key_index};
use crate::meta;
use crate::segment::{SegmentMeta, probe, read_range, read_segment_pooled, write_segment};

// Version 8 added the edge property root and the per-group edge base.
// Version 7 widened SegmentMeta with the structural layout byte for
// FullZip, so version 6 files must fail as unsupported here rather than
// misread downstream. Version 6 had added the has_keys byte and the
// primary-key index segments to the header, version 5 the SegmentMeta
// zone map, version 4 the per-chunk fence array.
const DIRECTORY_VERSION: u16 = 8;

/// Traversal direction: Fwd follows edges source to destination, Bwd the
/// reverse.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Fwd,
    Bwd,
}

/// A pooled pin of one direction of a group's CSR: the decoded offset
/// and neighbor arrays as shared handles.
pub type CsrArrays = (Arc<Vec<u64>>, Arc<Vec<u64>>);

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
    /// How many edges the groups before this one hold, which is the
    /// ordinal of this group's first forward edge. Stored rather than
    /// summed on load because summing means reading the last offset of
    /// every group's offsets segment, a chunk read per group, before a
    /// reader answers anything.
    pub edge_base: u64,
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
    /// Primary-key index over original node ids, present when the load
    /// relabeled rows.
    pub keys: Option<KeyIndex>,
    /// Root of the edge property chain, [`NULL_BLOCK`] when the table
    /// stores none. Its row domain is the edge ordinal.
    pub props: BlockPtr,
    pub groups: Vec<GroupMeta>,
}

impl Directory {
    pub(crate) fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&DIRECTORY_VERSION.to_le_bytes());
        out.extend_from_slice(&self.node_count.to_le_bytes());
        out.extend_from_slice(&self.edge_count.to_le_bytes());
        out.extend_from_slice(&self.props.to_le_bytes());
        out.extend_from_slice(&(self.groups.len() as u32).to_le_bytes());
        out.push(u8::from(self.keys.is_some()));
        if let Some(keys) = &self.keys {
            keys.keys.encode(&mut out);
            keys.rows.encode(&mut out);
        }
        for g in &self.groups {
            out.extend_from_slice(&g.row_count.to_le_bytes());
            out.extend_from_slice(&g.edge_base.to_le_bytes());
            g.fwd.offsets.encode(&mut out);
            g.fwd.neighbors.encode(&mut out);
            g.bwd.offsets.encode(&mut out);
            g.bwd.neighbors.encode(&mut out);
        }
        out
    }

    /// Decodes a directory chain payload. Public alongside the other
    /// container decoders so tooling and the fuzz targets reach it
    /// without a file around it.
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let corrupt = |detail: &str| ZuError::Corrupt {
            what: "group directory",
            detail: detail.to_string(),
        };
        let head = bytes.get(..31).ok_or_else(|| corrupt("truncated header"))?;
        let version = u16::from_le_bytes(head[..2].try_into().unwrap());
        if version != DIRECTORY_VERSION {
            return Err(ZuError::Unsupported {
                what: "group directory version",
                id: u32::from(version),
            });
        }
        let node_count = u64::from_le_bytes(head[2..10].try_into().unwrap());
        let edge_count = u64::from_le_bytes(head[10..18].try_into().unwrap());
        let props = u64::from_le_bytes(head[18..26].try_into().unwrap());
        let group_count = u32::from_le_bytes(head[26..30].try_into().unwrap()) as usize;
        let mut pos = 31usize;
        let keys = match head[30] {
            0 => None,
            1 => {
                let (keys, next) = SegmentMeta::decode(bytes, pos)?;
                let (rows, next) = SegmentMeta::decode(bytes, next)?;
                pos = next;
                Some(KeyIndex { keys, rows })
            }
            flag => return Err(corrupt(&format!("has_keys byte is {flag}"))),
        };
        // A group entry is at least 208 bytes (row count, edge base, and
        // four empty segment metas), so a count the payload cannot hold
        // is rejected before it sizes an allocation.
        if group_count > bytes.len().saturating_sub(pos) / 208 {
            return Err(corrupt("truncated group entry"));
        }
        let mut groups = Vec::with_capacity(group_count);
        for _ in 0..group_count {
            let rc = bytes
                .get(pos..pos + 12)
                .ok_or_else(|| corrupt("truncated group entry"))?;
            let row_count = u32::from_le_bytes(rc[..4].try_into().unwrap());
            let edge_base = u64::from_le_bytes(rc[4..].try_into().unwrap());
            pos += 12;
            let mut metas = Vec::with_capacity(4);
            for _ in 0..4 {
                let (meta, next) = SegmentMeta::decode(bytes, pos)?;
                metas.push(meta);
                pos = next;
            }
            let mut it = metas.into_iter();
            groups.push(GroupMeta {
                row_count,
                edge_base,
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
            keys,
            props,
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

/// Reads a comma separated `src,dst` edge list. The first line may be a
/// header and is skipped when its first two fields do not parse as
/// integers; a row that fails to parse anywhere else is an error naming
/// the line, same contract as the SNAP reader. Fields are trimmed, so
/// `1, 2` and CRLF endings both work, and columns past the second are
/// ignored the way the SNAP reader ignores trailing fields.
pub fn read_edge_csv(path: &Path) -> Result<Vec<(u32, u32)>> {
    let bad = |line_no: usize| {
        ZuError::InvalidArgument(format!("line {line_no}: expected 'src,dst' integers"))
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
        if trimmed.is_empty() {
            continue;
        }
        let mut parts = trimmed.split(',');
        let src = parts
            .next()
            .map(str::trim)
            .and_then(|t| t.parse::<u32>().ok());
        let dst = parts
            .next()
            .map(str::trim)
            .and_then(|t| t.parse::<u32>().ok());
        match (src, dst) {
            (Some(src), Some(dst)) => edges.push((src, dst)),
            _ if line_no == 1 => {}
            _ => return Err(bad(line_no)),
        }
    }
}

/// Reads a node key list, one u64 per line, skipping empty lines and
/// `#` comments. Keys are original source ids too wide for dense rows;
/// LDBC SNB ids are the motivating corpus.
pub fn read_key_list(path: &Path) -> Result<Vec<u64>> {
    let bad = |line_no: usize| ZuError::InvalidArgument(format!("line {line_no}: expected a key"));
    let file = std::fs::File::open(path)?;
    let mut reader = std::io::BufReader::with_capacity(1 << 20, file);
    let mut keys = Vec::new();
    let mut line = String::new();
    let mut line_no = 0usize;
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            return Ok(keys);
        }
        line_no += 1;
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        keys.push(trimmed.parse::<u64>().map_err(|_| bad(line_no))?);
    }
}

/// Reads a whitespace separated `src dst` edge list of u64 keys: the
/// SNAP layout widened to sources whose ids do not fit u32.
pub fn read_key_edge_list(path: &Path) -> Result<Vec<(u64, u64)>> {
    let bad = |line_no: usize| {
        ZuError::InvalidArgument(format!("line {line_no}: expected 'src dst' keys"))
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
            .and_then(|t| t.parse::<u64>().ok())
            .ok_or_else(|| bad(line_no))?;
        let dst = parts
            .next()
            .and_then(|t| t.parse::<u64>().ok())
            .ok_or_else(|| bad(line_no))?;
        edges.push((src, dst));
    }
}

/// A densified edge list: the edges over dense rows, plus the original
/// key of every row in the shape [`bulk_load_keyed`] takes.
pub type Densified = (Vec<(u32, u32)>, Vec<u64>);

/// Maps keyed edges onto dense rows: each key's rank in the sorted
/// deduplicated key list becomes its row id, and both endpoints of
/// every edge resolve through that ranking. Returns the mapped edges in
/// input order plus the key of every row, which is exactly the
/// `key_by_row` contract of [`bulk_load_keyed`]. An edge endpoint
/// absent from the key list is an error naming the key, because a
/// silently invented node would corrupt the row domain.
pub fn densify_keyed(keys: &[u64], edges: &[(u64, u64)]) -> Result<Densified> {
    let mut by_row = keys.to_vec();
    by_row.sort_unstable();
    by_row.dedup();
    if by_row.len() > u32::MAX as usize {
        return Err(ZuError::InvalidArgument(format!(
            "{} keys exceed the u32 row domain",
            by_row.len()
        )));
    }
    let row_of = |key: u64| {
        by_row.binary_search(&key).map(|r| r as u32).map_err(|_| {
            ZuError::InvalidArgument(format!(
                "edge references key {key} absent from the key list"
            ))
        })
    };
    let mut dense = Vec::with_capacity(edges.len());
    for &(src, dst) in edges {
        dense.push((row_of(src)?, row_of(dst)?));
    }
    Ok((dense, by_row))
}

/// Builds one direction's CSR groups from edges sorted by `(key, other)`.
pub(crate) fn build_direction(
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

/// The ordinal of each group's first edge: how many of `edges` name a
/// source in an earlier group. The edges must be sorted, which is the
/// contract of every caller that builds a direction out of them.
pub(crate) fn group_bases(node_count: u64, edges: &[(u32, u32)]) -> Vec<u64> {
    let group_rows = GROUP_ROWS as u64;
    let group_count = node_count.div_ceil(group_rows).max(1) as usize;
    let mut bases = Vec::with_capacity(group_count);
    let mut at = 0usize;
    for g in 0..group_count as u64 {
        bases.push(at as u64);
        let end = (g + 1) * group_rows;
        at += edges[at..].partition_point(|&(s, _)| u64::from(s) < end);
    }
    bases
}

/// Frees every block of the directory chain at `root` plus all four
/// segments per group it lists and the edge property columns it points
/// at. The blocks recycle at the next checkpoint per the
/// shadow-publishing rules.
pub(crate) fn free_directory(db: &mut Zu1File, root: BlockPtr) -> Result<()> {
    free_directory_parts(db, root, true)
}

/// [`free_directory`] leaving the edge property chain alone, for a
/// rebuild that hands the same columns to the directory it writes. The
/// caller owns the root from then on: nothing else names it once the old
/// directory is gone.
pub(crate) fn free_directory_keeping_props(db: &mut Zu1File, root: BlockPtr) -> Result<()> {
    free_directory_parts(db, root, false)
}

fn free_directory_parts(db: &mut Zu1File, root: BlockPtr, props: bool) -> Result<()> {
    let directory = Directory::decode(&meta::read_chain(db, root)?)?;
    if props && directory.props != NULL_BLOCK {
        crate::props::free_props(db, directory.props)?;
    }
    if let Some(keys) = &directory.keys {
        for seg in [&keys.keys, &keys.rows] {
            for &ptr in &seg.blocks {
                db.free_block(ptr)?;
            }
        }
    }
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
pub(crate) fn free_chain(db: &mut Zu1File, root: BlockPtr) -> Result<()> {
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
    bulk_load_keyed(db, "node", "edge", node_count, edges, None)
}

/// [`bulk_load_keyed`] without a key index.
pub fn bulk_load_as(
    db: &mut Zu1File,
    node_table: &str,
    rel_table: &str,
    node_count: u64,
    edges: &[(u32, u32)],
) -> Result<Directory> {
    bulk_load_keyed(db, node_table, rel_table, node_count, edges, None)
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
/// `key_by_row`, when given, is the original id of every row (the
/// pre-`REORDER` labels) and builds the primary-key index alongside the
/// CSRs; it must hold exactly `node_count` unique keys.
pub fn bulk_load_keyed(
    db: &mut Zu1File,
    node_table: &str,
    rel_table: &str,
    node_count: u64,
    edges: &[(u32, u32)],
    key_by_row: Option<&[u64]>,
) -> Result<Directory> {
    if let Some(keys) = key_by_row
        && keys.len() as u64 != node_count
    {
        return Err(ZuError::InvalidArgument(format!(
            "{} keys over {node_count} nodes",
            keys.len()
        )));
    }
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
    let out_hist = crate::stats::degree_histogram(edges);
    let mut rev: Vec<(u32, u32)> = edges.iter().map(|&(s, d)| (d, s)).collect();
    rev.sort_unstable();
    let bwd = build_direction(db, node_count, &rev)?;
    let in_hist = crate::stats::degree_histogram(&rev);
    let norms = crate::stats::DegreeStats {
        out: crate::stats::degree_norms(edges),
        inn: crate::stats::degree_norms(&rev),
        cross: crate::stats::degree_cross(edges, &rev),
    };
    drop(rev);
    let row_counts = |g: u64| {
        let first_row = g * GROUP_ROWS as u64;
        (node_count - first_row).min(GROUP_ROWS as u64) as u32
    };
    let bases = group_bases(node_count, edges);
    let groups = fwd
        .into_iter()
        .zip(bwd)
        .enumerate()
        .map(|(g, (fwd, bwd))| GroupMeta {
            row_count: row_counts(g as u64),
            edge_base: bases[g],
            fwd,
            bwd,
        })
        .collect();
    let directory = Directory {
        node_count,
        edge_count: edges.len() as u64,
        keys: key_by_row
            .map(|keys| write_key_index(db, keys))
            .transpose()?,
        props: NULL_BLOCK,
        groups,
    };
    let root = meta::write_chain(db, &directory.encode())?;
    let from = catalog.upsert_node(node_table, node_count)?;
    let rel_id = catalog.upsert_rel(rel_table, from, from, edges.len() as u64)?;
    index.set(rel_id, root);
    // The catalog, index, and stats chains are rewritten whole,
    // freeing the committed copies first.
    let mut stats = crate::stats::Stats::load(db)?;
    stats.rels.insert(
        rel_id,
        crate::stats::RelStats {
            out_hist,
            in_hist,
            norms,
            colors: None,
        },
    );
    free_chain(db, db.db_header().catalog_root)?;
    free_chain(db, db.db_header().table_index_root)?;
    free_chain(db, db.db_header().stats_root)?;
    let catalog_root = meta::write_chain(db, &catalog.encode())?;
    let index_root = meta::write_chain(db, &index.encode())?;
    db.db_header_mut().catalog_root = catalog_root;
    db.db_header_mut().table_index_root = index_root;
    stats.store(db)?;
    db.checkpoint()?;
    Ok(directory)
}

/// Read access to a bulk-loaded graph, caching the most recently decoded
/// group per direction so sequential scans decode each group once. The
/// two directions cache independently because a plan often walks both
/// on the same rel row by row, an expand backward feeding a count
/// forward, and a shared slot would decode a full group per row.
#[derive(Debug)]
pub struct GraphReader {
    directory: Directory,
    cached_groups: [Option<CachedGroup>; 2],
    /// Last pooled offset array per direction, for the degree reads
    /// that never touch neighbors. The executor asks for degrees one
    /// 1024-row chunk at a time, so without this slot every chunk
    /// takes the shared pool's mutex for an array the reader saw a
    /// chunk ago, and at eight workers that lock is the profile.
    cached_offsets: [Option<(usize, Arc<Vec<u64>>)>; 2],
    key_reader: Option<KeyReader>,
}

/// One decoded CSR group: its index, offsets, and neighbor values. The
/// arrays live in the file's decoded pools, so the slot here is just
/// the last-touched handle and siblings forked off the same file reuse
/// the decode.
type CachedGroup = (usize, Arc<Vec<u64>>, Arc<Vec<u64>>);

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
            cached_groups: [None, None],
            cached_offsets: [None, None],
            key_reader: None,
        })
    }

    /// Resolves an original id through the primary-key index, or errors
    /// when the file was loaded without one. The key segment's chunk
    /// directory loads on the first call and is reused after, so a
    /// lookup costs two chunk decodes.
    pub fn lookup_key(&mut self, db: &mut Zu1File, key: u64) -> Result<Option<u64>> {
        if self.key_reader.is_none() {
            let index = self.directory.keys.clone().ok_or_else(|| {
                ZuError::InvalidArgument(
                    "file has no primary-key index, load with REORDER to build one".to_string(),
                )
            })?;
            self.key_reader = Some(KeyReader::load(db, index)?);
        }
        self.key_reader.as_mut().unwrap().lookup(db, key)
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
        let idx = dir as usize;
        if self.cached_groups[idx].as_ref().map(|(i, _, _)| *i) != Some(g) {
            let (offsets, nbrs) = self.csr_group(db, g, dir)?;
            self.cached_groups[idx] = Some((g, offsets, nbrs));
        }
        let (_, offsets, nbrs) = self.cached_groups[idx].as_ref().unwrap();
        let lo = offsets[row] as usize;
        let hi = offsets[row + 1] as usize;
        Ok(&nbrs[lo..hi])
    }

    /// Returns the sorted out-neighbor list of `node`.
    pub fn neighbors(&mut self, db: &mut Zu1File, node: u64) -> Result<&[u64]> {
        self.neighbors_dir(db, node, Direction::Fwd)
    }

    /// Chunks the neighbor array of `group` in `dir` is stored in,
    /// directory only, no decode. This is what says whether pinning a
    /// group is worth it: the pin decodes every one of these chunks,
    /// and reading one node's list as a range decodes about one of
    /// them, so a caller wanting fewer lists than there are chunks is
    /// better off reading each one. It is the same rule
    /// [`Self::degrees_into`] uses on the offset array.
    pub fn list_chunks(&self, group: usize, dir: Direction) -> usize {
        match self.directory.groups.get(group) {
            Some(g) => g.dir(dir).neighbors.chunk_count(),
            None => 0,
        }
    }

    /// Pool-backed pins of one group's CSR in `dir`: the offset and
    /// neighbor arrays as shared handles. Warm calls are two pool map
    /// probes and two `Arc` clones, no decode and no copy, which is
    /// what the Snapshot csr surface lends out as borrowed slices.
    pub fn csr_group(&self, db: &mut Zu1File, group: usize, dir: Direction) -> Result<CsrArrays> {
        let meta = self
            .directory
            .groups
            .get(group)
            .ok_or_else(|| {
                ZuError::InvalidArgument(format!(
                    "group {group} out of 0..{}",
                    self.directory.groups.len()
                ))
            })?
            .dir(dir);
        let pools = db.pools();
        Ok((
            read_segment_pooled(db, &pools.csr_offsets, &meta.offsets)?,
            read_segment_pooled(db, &pools.adjacency, &meta.neighbors)?,
        ))
    }

    /// Degree of `node` in `dir` from the pooled offset array alone;
    /// the neighbor values never decode for a count.
    pub fn degree_of(&self, db: &mut Zu1File, node: u64, dir: Direction) -> Result<u64> {
        let (g, row) = self.locate(node)?;
        let meta = self.directory.groups[g].dir(dir);
        let pools = db.pools();
        let offs = read_segment_pooled(db, &pools.csr_offsets, &meta.offsets)?;
        Ok(offs[row + 1] - offs[row])
    }

    /// The pooled offset array of `group` in `dir` through the
    /// reader-local slot, so degree loops over consecutive chunks of
    /// the same group skip the pool entirely.
    fn offsets(
        &mut self,
        db: &mut Zu1File,
        group: usize,
        dir: Direction,
    ) -> Result<&Arc<Vec<u64>>> {
        let idx = dir as usize;
        if self.cached_offsets[idx].as_ref().map(|(g, _)| *g) != Some(group) {
            let meta = self.directory.groups[group].dir(dir);
            let pools = db.pools();
            let offs = read_segment_pooled(db, &pools.csr_offsets, &meta.offsets)?;
            self.cached_offsets[idx] = Some((group, offs));
        }
        Ok(&self.cached_offsets[idx].as_ref().unwrap().1)
    }

    /// Sum of degrees over `nodes` in `dir`. This is the counting
    /// expand's bulk read: it touches the 8% offsets pool and never the
    /// 20% adjacency pool, so a count over a hub's neighborhood costs
    /// offset diffs, not decoded neighbor megabytes.
    pub fn degree_batch(&mut self, db: &mut Zu1File, nodes: &[u64], dir: Direction) -> Result<u64> {
        let mut total = 0u64;
        self.degrees_run(db, nodes, dir, |_, d| total += d)?;
        Ok(total)
    }

    /// Adds each node's degree in `dir` onto `out`, position for
    /// position, from the pooled offset arrays alone. Same read shape
    /// as `degree_batch`, kept per row so a caller can multiply
    /// degrees across rels instead of summing one.
    pub fn degrees_into(
        &mut self,
        db: &mut Zu1File,
        nodes: &[u64],
        dir: Direction,
        out: &mut [u64],
    ) -> Result<()> {
        debug_assert_eq!(nodes.len(), out.len());
        self.degrees_run(db, nodes, dir, |at, d| out[at] += d)
    }

    /// Every node's degree in `dir`, handed to `sink` with the position
    /// it arrived in. Nodes of the same group that arrive together are
    /// one run, and the run picks how it reads: the whole group's
    /// offsets once it is long enough to pay for decoding them, and the
    /// two offsets a row needs when it is not. A scan hands over a
    /// group's rows in order and takes the first path, which is what
    /// the reader-local slot is there for. A batch of point reads hands
    /// over rows from all over the table and takes the second, which is
    /// the difference between reading a chunk per row and decoding a
    /// group per row.
    fn degrees_run(
        &mut self,
        db: &mut Zu1File,
        nodes: &[u64],
        dir: Direction,
        mut sink: impl FnMut(usize, u64),
    ) -> Result<()> {
        let mut at = 0;
        while at < nodes.len() {
            let (group, _) = self.locate(nodes[at])?;
            let mut end = at + 1;
            while end < nodes.len() && self.locate(nodes[end])?.0 == group {
                end += 1;
            }
            let chunks = self.directory.groups[group]
                .dir(dir)
                .offsets
                .chunk_count()
                .max(1);
            if end - at >= chunks {
                let offs = Arc::clone(self.offsets(db, group, dir)?);
                for (i, &node) in (at..end).zip(&nodes[at..end]) {
                    let (_, row) = self.locate(node)?;
                    sink(i, offs[row + 1] - offs[row]);
                }
            } else {
                let meta = &self.directory.groups[group].dir(dir).offsets;
                let mut pair = Vec::with_capacity(2);
                for (i, &node) in (at..end).zip(&nodes[at..end]) {
                    let (_, row) = self.locate(node)?;
                    pair.clear();
                    read_range(db, meta, row as u64, row as u64 + 2, &mut pair)?;
                    sink(i, pair[1] - pair[0]);
                }
            }
            at = end;
        }
        Ok(())
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

    /// Edge probe: does `node` list `other` in `dir`? Two offset values
    /// locate the list, then the fence array names the one chunk that
    /// could hold `other`, so a probe decodes at most one neighbor chunk
    /// however large the degree. This is the primitive behind
    /// `MATCH (a)-[]->(b)` on bound endpoints.
    pub fn has_edge_dir(
        &self,
        db: &mut Zu1File,
        node: u64,
        other: u64,
        dir: Direction,
    ) -> Result<bool> {
        let (g, row) = self.locate(node)?;
        let meta = self.directory.groups[g].dir(dir);
        let mut offs = Vec::with_capacity(2);
        read_range(db, &meta.offsets, row as u64, row as u64 + 2, &mut offs)?;
        probe(db, &meta.neighbors, offs[0], offs[1], other)
    }

    /// Edge probe on the forward direction: does `src` point at `dst`?
    pub fn has_edge(&self, db: &mut Zu1File, src: u64, dst: u64) -> Result<bool> {
        self.has_edge_dir(db, src, dst, Direction::Fwd)
    }

    /// The row of the edge property columns that `src -> dst` holds,
    /// and `None` when the edge is not there.
    ///
    /// The ordinal is the edge's place in the load order, which the
    /// forward CSR lays out group after group and list after list, so
    /// it is the group's base plus the slot the destination sits in.
    /// Finding that slot is the same search [`Self::has_edge`] runs, at
    /// the same cost: two offset values and the one neighbor chunk the
    /// fences admit, whatever the degree. An edge reached forward could
    /// have its ordinal counted out as the expand walks the list, and
    /// the vectorized read does exactly that; this is the answer for an
    /// edge that arrived any other way, a backward expand above all,
    /// where the slot in the backward array says nothing about the
    /// forward one.
    ///
    /// Edges are unique in a table that stores properties, which
    /// [`crate::props::store_rel_props`] is what enforces, so the pair
    /// names one edge and the ordinal is that edge's.
    pub fn edge_ordinal(&self, db: &mut Zu1File, src: u64, dst: u64) -> Result<Option<u64>> {
        let (g, row) = self.locate(src)?;
        let group = &self.directory.groups[g];
        let mut offs = Vec::with_capacity(2);
        read_range(
            db,
            &group.fwd.offsets,
            row as u64,
            row as u64 + 2,
            &mut offs,
        )?;
        let slot = crate::segment::locate(db, &group.fwd.neighbors, offs[0], offs[1], dst)?;
        Ok(slot.map(|slot| group.edge_base + slot))
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
    fn csv_reader_matches_the_snap_reader() {
        let dir = tempfile::tempdir().unwrap();
        let txt = dir.path().join("edges.txt");
        let csv = dir.path().join("edges.csv");
        std::fs::write(&txt, "# comment\n0 1\n0 3\n1 2\n\n3 0\n").unwrap();
        std::fs::write(&csv, "src,dst\r\n0,1\n0, 3\n1,2\n\n3,0\r\n").unwrap();
        assert_eq!(read_edge_csv(&csv).unwrap(), read_edge_list(&txt).unwrap());
    }

    #[test]
    fn csv_without_header_keeps_the_first_row() {
        let dir = tempfile::tempdir().unwrap();
        let csv = dir.path().join("edges.csv");
        std::fs::write(&csv, "5,6\n7,8\n").unwrap();
        assert_eq!(read_edge_csv(&csv).unwrap(), vec![(5, 6), (7, 8)]);
    }

    #[test]
    fn csv_bad_row_errors_by_line() {
        let dir = tempfile::tempdir().unwrap();
        let csv = dir.path().join("edges.csv");
        std::fs::write(&csv, "src,dst\n1,2\nnope,4\n").unwrap();
        let err = read_edge_csv(&csv).unwrap_err();
        assert!(
            err.to_string().contains("line 3"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn csv_extra_columns_are_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let csv = dir.path().join("edges.csv");
        std::fs::write(&csv, "src,dst,weight\n1,2,0.5\n").unwrap();
        assert_eq!(read_edge_csv(&csv).unwrap(), vec![(1, 2)]);
    }

    #[test]
    fn key_readers_take_ids_past_u32() {
        let dir = tempfile::tempdir().unwrap();
        let nodes = dir.path().join("keys.txt");
        let edges = dir.path().join("edges.txt");
        std::fs::write(&nodes, "# persons\n14\n4398046517420\n\n16\n").unwrap();
        std::fs::write(&edges, "# knows\n14 4398046517420\n4398046517420 16\n").unwrap();
        assert_eq!(read_key_list(&nodes).unwrap(), vec![14, 4398046517420, 16]);
        assert_eq!(
            read_key_edge_list(&edges).unwrap(),
            vec![(14, 4398046517420), (4398046517420, 16)]
        );
    }

    #[test]
    fn key_readers_error_by_line() {
        let dir = tempfile::tempdir().unwrap();
        let nodes = dir.path().join("keys.txt");
        std::fs::write(&nodes, "14\nnope\n").unwrap();
        let err = read_key_list(&nodes).unwrap_err();
        assert!(
            err.to_string().contains("line 2"),
            "unexpected error: {err}"
        );
        let edges = dir.path().join("edges.txt");
        std::fs::write(&edges, "14 16\n14\n").unwrap();
        let err = read_key_edge_list(&edges).unwrap_err();
        assert!(
            err.to_string().contains("line 2"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn densify_ranks_keys_and_rejects_strays() {
        let keys = vec![4398046517420u64, 14, 16, 14];
        let edges = vec![(14u64, 4398046517420u64), (4398046517420, 16)];
        let (dense, by_row) = densify_keyed(&keys, &edges).unwrap();
        assert_eq!(by_row, vec![14, 16, 4398046517420]);
        assert_eq!(dense, vec![(0, 2), (2, 1)]);
        let err = densify_keyed(&keys, &[(14, 99)]).unwrap_err();
        assert!(err.to_string().contains("99"), "unexpected error: {err}");
    }

    #[test]
    fn densified_load_serves_lookups_by_original_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("g.zu1");
        let keys = vec![14u64, 16, 4398046517420];
        let edges = vec![(14u64, 4398046517420u64), (4398046517420, 16)];
        let (mut dense, by_row) = densify_keyed(&keys, &edges).unwrap();
        {
            let mut db = Zu1File::create(&path).unwrap();
            bulk_load_keyed(
                &mut db,
                "person",
                "knows",
                by_row.len() as u64,
                sorted_edges(&mut dense),
                Some(&by_row),
            )
            .unwrap();
        }
        let mut db = Zu1File::open(&path).unwrap();
        let mut reader = GraphReader::load(&mut db).unwrap();
        assert_eq!(reader.lookup_key(&mut db, 4398046517420).unwrap(), Some(2));
        assert_eq!(reader.lookup_key(&mut db, 15).unwrap(), None);
        let row = reader.lookup_key(&mut db, 14).unwrap().unwrap();
        assert_eq!(reader.neighbors(&mut db, row).unwrap(), &[2]);
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

    #[test]
    fn edge_probe_matches_the_lists() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("g.zu1");
        // A hub with a multi-chunk list, ordinary nodes, and a node in
        // the tail group, so probes cross chunk and group boundaries.
        let mut edges: Vec<(u32, u32)> = (0..3000).map(|d| (7u32, d * 2)).collect();
        edges.extend([(9, 4), (9, 7000), (1023, 1), (1024, 2), (GROUP_ROWS, 3)]);
        let node_count = u64::from(GROUP_ROWS) + 5;
        {
            let mut db = Zu1File::create(&path).unwrap();
            bulk_load(&mut db, node_count, sorted_edges(&mut edges)).unwrap();
        }
        let mut db = Zu1File::open(&path).unwrap();
        let reader = GraphReader::load(&mut db).unwrap();
        for &(s, d) in edges.iter() {
            assert!(
                reader
                    .has_edge(&mut db, u64::from(s), u64::from(d))
                    .unwrap(),
                "present edge {s}->{d}"
            );
            assert!(
                reader
                    .has_edge_dir(&mut db, u64::from(d), u64::from(s), Direction::Bwd)
                    .unwrap(),
                "present edge {s}->{d} backward"
            );
        }
        for (s, d) in [(7u64, 1u64), (7, 5999), (7, 6000), (9, 5), (0, 0), (500, 7)] {
            assert!(!reader.has_edge(&mut db, s, d).unwrap(), "absent {s}->{d}");
        }
        assert!(reader.has_edge(&mut db, node_count, 0).is_err());
    }

    #[test]
    fn group_decodes_are_pooled_across_thrash_and_forks() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("g.zu1");
        let mut edges: Vec<(u32, u32)> = vec![(1, 2), (1, 3), (GROUP_ROWS, 4), (GROUP_ROWS + 1, 5)];
        let node_count = u64::from(GROUP_ROWS) + 6;
        {
            let mut db = Zu1File::create(&path).unwrap();
            bulk_load(&mut db, node_count, sorted_edges(&mut edges)).unwrap();
        }
        let mut db = Zu1File::open(&path).unwrap();
        let mut reader = GraphReader::load(&mut db).unwrap();
        let g1 = u64::from(GROUP_ROWS);
        // Alternate groups: the reader's one slot per direction
        // thrashes, but the pool serves every revisit without a decode.
        for _ in 0..5 {
            assert_eq!(reader.neighbors(&mut db, 1).unwrap(), &[2, 3]);
            assert_eq!(reader.neighbors(&mut db, g1).unwrap(), &[4]);
        }
        let pools = db.pools();
        let s = pools.adjacency.stats();
        assert_eq!(s.misses, 2, "each group decoded once");
        assert_eq!(s.hits, 8, "every revisit was a pool hit");
        // A forked handle shares the pools, so a fresh reader on it
        // reads a warm group without decoding anything.
        let mut fork = db.reopen().unwrap();
        let mut sibling = GraphReader::load(&mut fork).unwrap();
        assert_eq!(sibling.neighbors(&mut fork, g1 + 1).unwrap(), &[5]);
        assert_eq!(pools.adjacency.stats().misses, 2, "fork reused the decode");
    }

    #[test]
    fn degrees_come_from_offsets_alone() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("deg.zu1");
        let mut edges: Vec<(u32, u32)> = vec![(1, 2), (1, 3), (GROUP_ROWS, 4), (GROUP_ROWS + 1, 5)];
        let node_count = u64::from(GROUP_ROWS) + 6;
        {
            let mut db = Zu1File::create(&path).unwrap();
            bulk_load(&mut db, node_count, sorted_edges(&mut edges)).unwrap();
        }
        let mut db = Zu1File::open(&path).unwrap();
        let mut reader = GraphReader::load(&mut db).unwrap();
        let g1 = u64::from(GROUP_ROWS);
        assert_eq!(reader.degree_of(&mut db, 1, Direction::Fwd).unwrap(), 2);
        assert_eq!(reader.degree_of(&mut db, 0, Direction::Fwd).unwrap(), 0);
        assert_eq!(reader.degree_of(&mut db, g1, Direction::Fwd).unwrap(), 1);
        assert_eq!(reader.degree_of(&mut db, 2, Direction::Bwd).unwrap(), 1);
        assert!(
            reader
                .degree_of(&mut db, node_count, Direction::Fwd)
                .is_err()
        );
        // The batch spans both groups and agrees with the point reads.
        let nodes = [1u64, 2, g1, g1 + 1];
        assert_eq!(
            reader
                .degree_batch(&mut db, &nodes, Direction::Fwd)
                .unwrap(),
            4
        );
        assert_eq!(
            reader
                .degree_batch(&mut db, &nodes, Direction::Bwd)
                .unwrap(),
            1
        );
        // Counting never decoded a neighbor value: the adjacency pool
        // saw no traffic at all, only the offsets pool did.
        let pools = db.pools();
        let adj = pools.adjacency.stats();
        assert_eq!(adj.misses + adj.hits, 0, "degrees touched adjacency");
        assert!(pools.csr_offsets.stats().misses > 0);
    }

    #[test]
    fn an_edge_ordinal_is_its_place_in_the_load_order() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ord.zu1");
        // Edges over three groups, a hub whose list spans several
        // chunks, and a tail group, so the ordinal is asked across
        // every boundary it has.
        let rows = GROUP_ROWS;
        let node_count = u64::from(rows) * 2 + 10;
        let mut edges: Vec<(u32, u32)> = (0..3000).map(|d| (7u32, d * 2)).collect();
        edges.extend([
            (0, 1),
            (rows - 1, 0),
            (rows, rows + 1),
            (rows + 5, 3),
            (2 * rows + 9, 2),
        ]);
        {
            let mut db = Zu1File::create(&path).unwrap();
            bulk_load(&mut db, node_count, sorted_edges(&mut edges)).unwrap();
        }
        let mut db = Zu1File::open(&path).unwrap();
        let reader = GraphReader::load(&mut db).unwrap();
        // The load order is the sorted edge list, so an edge's ordinal
        // is its index in it, and every edge has to answer its own.
        for (want, &(s, d)) in edges.iter().enumerate() {
            assert_eq!(
                reader
                    .edge_ordinal(&mut db, u64::from(s), u64::from(d))
                    .unwrap(),
                Some(want as u64),
                "edge {s}->{d}"
            );
        }
        // An edge that is not there has no ordinal, whether or not its
        // source has a list at all.
        for (s, d) in [(7u64, 1u64), (7, 5999), (0, 2), (5, 0), (node_count - 1, 0)] {
            assert_eq!(
                reader.edge_ordinal(&mut db, s, d).unwrap(),
                None,
                "{s}->{d}"
            );
        }
        assert!(reader.edge_ordinal(&mut db, node_count, 0).is_err());
    }

    #[test]
    fn hostile_group_count_rejected() {
        // A header claiming u32::MAX groups must die on the size check,
        // not in the allocator.
        let mut bytes = DIRECTORY_VERSION.to_le_bytes().to_vec();
        bytes.extend_from_slice(&10u64.to_le_bytes());
        bytes.extend_from_slice(&20u64.to_le_bytes());
        bytes.extend_from_slice(&NULL_BLOCK.to_le_bytes());
        bytes.extend_from_slice(&u32::MAX.to_le_bytes());
        bytes.push(0);
        let err = Directory::decode(&bytes).unwrap_err();
        assert!(format!("{err}").contains("truncated group entry"));
        // A has_keys byte that is neither 0 nor 1 is corruption, not a
        // silent skip.
        let flag_at = bytes.len() - 1;
        bytes[flag_at] = 7;
        let err = Directory::decode(&bytes).unwrap_err();
        assert!(format!("{err}").contains("has_keys byte is 7"));
    }

    #[test]
    fn keyed_load_resolves_original_ids() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("g.zu1");
        // A small graph relabeled by BFS, keys are the original labels,
        // exactly what zu copy --reorder produces.
        let mut edges: Vec<(u32, u32)> = (0..4000u32)
            .map(|i| (i.wrapping_mul(37) % 700, i.wrapping_mul(11) % 700))
            .collect();
        let n = 700u64;
        let map = crate::reorder::bfs_order(n, &edges);
        crate::reorder::relabel(&mut edges, &map);
        let edges = sorted_edges(&mut edges);
        let mut key_by_row = vec![0u64; n as usize];
        for (old, &new) in map.iter().enumerate() {
            key_by_row[new as usize] = old as u64;
        }
        {
            let mut db = Zu1File::create(&path).unwrap();
            bulk_load_keyed(&mut db, "node", "edge", n, edges, Some(&key_by_row)).unwrap();
        }
        crate::verify(&path).unwrap();
        let mut db = Zu1File::open(&path).unwrap();
        let mut reader = GraphReader::load(&mut db).unwrap();
        for old in (0..n).step_by(13) {
            assert_eq!(
                reader.lookup_key(&mut db, old).unwrap(),
                Some(u64::from(map[old as usize])),
                "key {old}"
            );
        }
        assert_eq!(reader.lookup_key(&mut db, n).unwrap(), None);
        assert_eq!(reader.lookup_key(&mut db, u64::MAX).unwrap(), None);
        // A file loaded without keys refuses key lookups.
        let path2 = dir.path().join("g2.zu1");
        let mut db2 = Zu1File::create(&path2).unwrap();
        bulk_load(&mut db2, n, edges).unwrap();
        let mut reader2 = GraphReader::load(&mut db2).unwrap();
        assert!(reader2.lookup_key(&mut db2, 0).is_err());
        // A key count that disagrees with the node domain is rejected.
        let mut db3 = Zu1File::create(&dir.path().join("g3.zu1")).unwrap();
        let err = bulk_load_keyed(&mut db3, "node", "edge", n, edges, Some(&[1, 2])).unwrap_err();
        assert!(format!("{err}").contains("2 keys"));
    }
}
