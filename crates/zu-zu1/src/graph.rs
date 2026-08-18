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
//! `version: u16`, `from_count: u64`, `to_count: u64`,
//! `edge_count: u64`, `props: BlockPtr`, `group_count: u32`,
//! `has_keys: u8`, then when `has_keys` is 1 the key and row
//! `SegmentMeta` of the primary-key index, then per group
//! `row_count: u32`, `edge_base: u64`, and the fwd offsets, fwd
//! neighbors, bwd offsets, and bwd neighbors `SegmentMeta`.
//!
//! A rel table runs between two node tables, which need not be the same
//! one and need not be the same size: `from_count` is the row domain
//! the forward direction is keyed by and `to_count` the one the
//! backward direction is. The two share a single group array, as long
//! as the longer of them needs, because every reader reaches a group by
//! row and a direction at once and the shorter end simply has nothing
//! past its own domain. A group beyond an end's rows holds that end's
//! empty CSR, the same one a table of no rows loads, which costs a
//! segment of one offset and no neighbors.
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

use crate::catalog::{Catalog, ElementKind, TableIndex};
use crate::file::{BlockPtr, NULL_BLOCK, Zu1File};
use crate::keys::{KeyIndex, KeyReader, write_key_index};
use crate::meta;
use crate::segment::{SegmentMeta, probe, read_range, read_segment_pooled, write_segment};

// Version 9 split the one node count into the from and to row domains,
// so a rel table can run between two node tables.
// Version 8 added the edge property root and the per-group edge base.
// Version 7 widened SegmentMeta with the structural layout byte for
// FullZip, so version 6 files must fail as unsupported here rather than
// misread downstream. Version 6 had added the has_keys byte and the
// primary-key index segments to the header, version 5 the SegmentMeta
// zone map, version 4 the per-chunk fence array.
const DIRECTORY_VERSION: u16 = 9;

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
    /// Rows of the FROM table this group covers, which is what the
    /// forward direction is keyed by. The backward direction is keyed
    /// by the TO table and carries its own row count in the length of
    /// its offsets array, so a group past one end's domain holds an
    /// empty CSR for that end and a full one for the other.
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
    /// Rows of the node table the edges leave, the domain a source id
    /// and the forward CSR are numbered in.
    pub from_count: u64,
    /// Rows of the node table the edges arrive at, the domain a
    /// destination id and the backward CSR are numbered in. Equal to
    /// `from_count` when both ends are the same table, which is what a
    /// load naming one node table produces.
    pub to_count: u64,
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
    /// The row domain `dir` is keyed by: sources for Fwd, destinations
    /// for Bwd. This is the range a node id must be in to have a list
    /// in that direction at all.
    pub fn rows(&self, dir: Direction) -> u64 {
        match dir {
            Direction::Fwd => self.from_count,
            Direction::Bwd => self.to_count,
        }
    }

    /// The one row domain of a rel table that runs inside a single node
    /// table, for the algorithms whose answer is a value per node and
    /// which therefore only mean anything on such a table. A table
    /// between two ends of different sizes has no such domain and is
    /// refused here; the binder refuses the cross table case ahead of
    /// this, and this is what keeps a caller reaching past it honest.
    pub fn one_domain(&self) -> Result<u64> {
        if self.from_count != self.to_count {
            return Err(ZuError::InvalidArgument(format!(
                "this needs a rel table over one node table, and its ends hold {} and {} rows",
                self.from_count, self.to_count
            )));
        }
        Ok(self.from_count)
    }

    pub(crate) fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&DIRECTORY_VERSION.to_le_bytes());
        out.extend_from_slice(&self.from_count.to_le_bytes());
        out.extend_from_slice(&self.to_count.to_le_bytes());
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
        // The version comes off first, on its own two bytes. Every
        // other field's offset is a claim this version makes, and the
        // header of an older one is a different length, so a length
        // check ahead of the gate would report corruption where the
        // truth is an older writer.
        let tag = bytes.get(..2).ok_or_else(|| corrupt("truncated header"))?;
        let version = u16::from_le_bytes(tag.try_into().unwrap());
        if version != DIRECTORY_VERSION {
            return Err(ZuError::Unsupported {
                what: "group directory version",
                id: u32::from(version),
            });
        }
        let head = bytes.get(..39).ok_or_else(|| corrupt("truncated header"))?;
        let from_count = u64::from_le_bytes(head[2..10].try_into().unwrap());
        let to_count = u64::from_le_bytes(head[10..18].try_into().unwrap());
        let edge_count = u64::from_le_bytes(head[18..26].try_into().unwrap());
        let props = u64::from_le_bytes(head[26..34].try_into().unwrap());
        let group_count = u32::from_le_bytes(head[34..38].try_into().unwrap()) as usize;
        let mut pos = 39usize;
        let keys = match head[38] {
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
            from_count,
            to_count,
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
    Ok(read_edge_csv_with_props(path, false)?.0)
}

/// What one header cell says its column is. `Skip` is a column a loader
/// reads nothing from, which is what the structural cells of a rel file
/// are once the endpoints are known.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CsvRole {
    Src,
    Dst,
    /// The id a node file gives its row, which a rel file names its
    /// endpoints by. Only a node file has one.
    Key,
    Prop(usize),
    Skip,
}

/// [`read_edge_csv`] with the columns that are neither endpoint read as
/// edge property columns, which is what the header is for.
///
/// The header is the schema. Every cell is `name:TYPE`, the form LDBC
/// writes and every bulk loader in this space parses: `:START_ID` and
/// `:END_ID` are the two endpoints, `:TYPE`, `:LABEL` and `:ID` carry
/// load-time structure and are read from no further, and a named cell
/// of `INT64`, `FLOAT64`, `BOOL` or `STRING` is a property column of
/// that name. A cell with no type token is a string column, except that
/// `src` and `dst` name the endpoints, which is how a hand written file
/// with no LDBC in it says the same thing. A file whose header names
/// neither endpoint uses its first two columns, the way the parquet
/// reader falls back to the first two fields.
///
/// A column is dense: every row owes it a value, and an empty field is
/// an error rather than a null, because an edge column is addressed by
/// the ordinal and a row that skipped one would shift every value after
/// it. A row with a different field count than the header is an error
/// for the same reason, which is also what a comma inside an unquoted
/// value reads as, since fields are split on commas and nothing else.
///
/// With `props` false, or on a file with no header, this is the reader
/// above and no column is built.
pub fn read_edge_csv_with_props(path: &Path, props: bool) -> Result<crate::props::EdgesWithProps> {
    use crate::props::OwnedColumn;

    let bad = |line_no: usize| {
        ZuError::InvalidArgument(format!("line {line_no}: expected 'src,dst' integers"))
    };
    let file = std::fs::File::open(path)?;
    let mut reader = std::io::BufReader::with_capacity(1 << 20, file);
    let mut edges = Vec::new();
    let mut columns: Vec<OwnedColumn> = Vec::new();
    let mut roles: Vec<CsvRole> = Vec::new();
    let mut line = String::new();
    let mut line_no = 0usize;
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            return Ok((edges, columns));
        }
        line_no += 1;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if roles.is_empty() {
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
                _ if line_no == 1 => {
                    if props {
                        let cells: Vec<&str> = trimmed.split(',').map(str::trim).collect();
                        (roles, columns) = parse_edge_header(&cells)?;
                    }
                }
                _ => return Err(bad(line_no)),
            }
            continue;
        }
        let wrong_width = |got: usize| {
            ZuError::InvalidArgument(format!(
                "line {line_no}: {got} fields where the header names {}",
                roles.len()
            ))
        };
        let (mut src, mut dst) = (None, None);
        let mut fields = 0usize;
        for (i, cell) in trimmed.split(',').map(str::trim).enumerate() {
            let role = roles.get(i).ok_or_else(|| wrong_width(i + 1))?;
            fields += 1;
            match role {
                CsvRole::Src => src = cell.parse::<u32>().ok(),
                CsvRole::Dst => dst = cell.parse::<u32>().ok(),
                // A rel header never names one, since `:ID` there is
                // the edge's own name and nothing reads it.
                CsvRole::Key | CsvRole::Skip => {}
                CsvRole::Prop(col) => {
                    let column = &mut columns[*col];
                    push_csv_value(&mut column.values, cell).ok_or_else(|| {
                        ZuError::InvalidArgument(format!(
                            "line {line_no}, column '{}': '{cell}' is not a {}",
                            column.name,
                            csv_type_name(&column.values)
                        ))
                    })?;
                }
            }
        }
        if fields != roles.len() {
            return Err(wrong_width(fields));
        }
        match (src, dst) {
            (Some(src), Some(dst)) => edges.push((src, dst)),
            _ => return Err(bad(line_no)),
        }
    }
}

/// The non-empty lines of a file, trimmed, with the number of the line
/// each was read from, which is what the two dataset readers below walk.
///
/// The edge reader above keeps its own loop on purpose. That one is the
/// ingest hot path and reads two integers a line with nothing else in
/// the way; these two read a header, a key space and typed columns, and
/// sharing the line handling is the only thing worth sharing.
struct CsvLines {
    reader: std::io::BufReader<std::fs::File>,
    line: String,
    line_no: usize,
}

impl CsvLines {
    fn open(path: &Path) -> Result<Self> {
        Ok(CsvLines {
            reader: std::io::BufReader::with_capacity(1 << 20, std::fs::File::open(path)?),
            line: String::new(),
            line_no: 0,
        })
    }

    /// The next line that holds anything, `None` at the end of the file.
    fn next(&mut self) -> Result<Option<(usize, &str)>> {
        loop {
            self.line.clear();
            if self.reader.read_line(&mut self.line)? == 0 {
                return Ok(None);
            }
            self.line_no += 1;
            if !self.line.trim().is_empty() {
                return Ok(Some((self.line_no, self.line.trim())));
            }
        }
    }
}

/// Reads a node file: one row a line, the row's own id in the column the
/// header marked `:ID`, and every named column beside it a property of
/// that row.
///
/// The first line is the header, always, which is the one place this
/// differs from [`read_edge_csv_with_props`]. An edge file can be two
/// integers a line and still be a graph; a node file with no header
/// names no property and no id, so there is nothing to read it as.
///
/// The row an id gets is the line it was written on, counting from zero
/// past the header. A rel file names its endpoints by id and the loader
/// maps them through that, so the order of a node file is the order of
/// the table it loads as, and nothing else decides it.
pub fn read_node_csv(path: &Path) -> Result<crate::props::NodesWithProps> {
    let mut lines = CsvLines::open(path)?;
    let mut keys: Vec<u64> = Vec::new();
    let mut columns = Vec::new();
    let mut roles: Vec<CsvRole> = Vec::new();
    let mut header = false;
    while let Some((line_no, line)) = lines.next()? {
        let cells: Vec<&str> = line.split(',').map(str::trim).collect();
        if !header {
            header = true;
            (roles, columns) = parse_header(&cells, true)?;
            continue;
        }
        if cells.len() != roles.len() {
            return Err(wrong_csv_width(line_no, cells.len(), roles.len()));
        }
        let mut key = None;
        for (cell, role) in cells.iter().zip(&roles) {
            match role {
                CsvRole::Key => key = cell.parse::<u64>().ok(),
                CsvRole::Prop(col) => push_csv_prop(&mut columns[*col], cell, line_no)?,
                _ => {}
            }
        }
        match key {
            Some(key) => keys.push(key),
            None => {
                return Err(ZuError::InvalidArgument(format!(
                    "line {line_no}: the id column holds no node id"
                )));
            }
        }
    }
    if !header {
        return Err(ZuError::InvalidArgument(
            "the file is empty, so it heads no node table".into(),
        ));
    }
    Ok((keys, columns))
}

/// Reads a rel file whose endpoints are the ids a node file gave its
/// rows rather than row offsets, together with the edge property columns
/// the header names.
///
/// The ids stay as they were written. Which row of which node table each
/// one is takes both node files to answer, and this reads one file, so
/// the translation belongs to the loader that holds all of them.
pub fn read_rel_csv_keyed(path: &Path) -> Result<crate::props::KeyedEdgesWithProps> {
    let mut lines = CsvLines::open(path)?;
    let mut edges: Vec<(u64, u64)> = Vec::new();
    let mut columns = Vec::new();
    let mut roles: Vec<CsvRole> = Vec::new();
    let mut header = false;
    while let Some((line_no, line)) = lines.next()? {
        let cells: Vec<&str> = line.split(',').map(str::trim).collect();
        if !header {
            header = true;
            (roles, columns) = parse_header(&cells, false)?;
            continue;
        }
        if cells.len() != roles.len() {
            return Err(wrong_csv_width(line_no, cells.len(), roles.len()));
        }
        let (mut src, mut dst) = (None, None);
        for (cell, role) in cells.iter().zip(&roles) {
            match role {
                CsvRole::Src => src = cell.parse::<u64>().ok(),
                CsvRole::Dst => dst = cell.parse::<u64>().ok(),
                CsvRole::Prop(col) => push_csv_prop(&mut columns[*col], cell, line_no)?,
                _ => {}
            }
        }
        match (src, dst) {
            (Some(src), Some(dst)) => edges.push((src, dst)),
            _ => {
                return Err(ZuError::InvalidArgument(format!(
                    "line {line_no}: an endpoint column holds no node id"
                )));
            }
        }
    }
    if !header {
        return Err(ZuError::InvalidArgument(
            "the file is empty, so it heads no rel table".into(),
        ));
    }
    Ok((edges, columns))
}

/// A row with a different field count than the header, which is refused
/// rather than padded: a column is addressed by its ordinal and a short
/// row would shift every value after it.
fn wrong_csv_width(line_no: usize, got: usize, want: usize) -> ZuError {
    ZuError::InvalidArgument(format!(
        "line {line_no}: {got} fields where the header names {want}"
    ))
}

/// Appends one field to the column it belongs to, or says the field is
/// not a value of that column's type.
fn push_csv_prop(column: &mut crate::props::OwnedColumn, cell: &str, line_no: usize) -> Result<()> {
    push_csv_value(&mut column.values, cell).ok_or_else(|| {
        ZuError::InvalidArgument(format!(
            "line {line_no}, column '{}': '{cell}' is not a {}",
            column.name,
            csv_type_name(&column.values)
        ))
    })
}

/// [`parse_header`] for a rel file, where `:ID` is structure to skip
/// rather than the row's own id.
fn parse_edge_header(cells: &[&str]) -> Result<(Vec<CsvRole>, Vec<crate::props::OwnedColumn>)> {
    parse_header(cells, false)
}

/// Reads the header cells into a role per column and an empty column
/// per property. See [`read_edge_csv_with_props`] for the form.
///
/// `node` says which kind of file this heads. A node file's `:ID` cell
/// is the key its rows are named by elsewhere, and it has no endpoints;
/// a rel file's endpoints are `:START_ID` and `:END_ID`, and an `:ID`
/// there is the edge's own name, which nothing reads.
fn parse_header(
    cells: &[&str],
    node: bool,
) -> Result<(Vec<CsvRole>, Vec<crate::props::OwnedColumn>)> {
    use crate::props::{OwnedColumn, OwnedValues};

    let mut roles = vec![CsvRole::Skip; cells.len()];
    let mut columns: Vec<OwnedColumn> = Vec::new();
    let mut named_ends = false;
    for (i, cell) in cells.iter().enumerate() {
        let (name, ty) = match cell.split_once(':') {
            Some((name, ty)) => (name.trim(), ty.trim().to_ascii_uppercase()),
            None => (*cell, String::new()),
        };
        let values = match ty.as_str() {
            "START_ID" => {
                roles[i] = CsvRole::Src;
                named_ends = true;
                continue;
            }
            "END_ID" => {
                roles[i] = CsvRole::Dst;
                named_ends = true;
                continue;
            }
            "ID" if node => {
                roles[i] = CsvRole::Key;
                continue;
            }
            "TYPE" | "LABEL" | "ID" => continue,
            "" if name.eq_ignore_ascii_case("src") => {
                roles[i] = CsvRole::Src;
                named_ends = true;
                continue;
            }
            "" if name.eq_ignore_ascii_case("dst") => {
                roles[i] = CsvRole::Dst;
                named_ends = true;
                continue;
            }
            "INT64" => OwnedValues::Int(Vec::new()),
            "FLOAT64" => OwnedValues::Float(Vec::new()),
            "BOOL" => OwnedValues::Bool(Vec::new()),
            "STRING" | "" => OwnedValues::Str(Vec::new()),
            other => {
                return Err(ZuError::InvalidArgument(format!(
                    "header column '{cell}': no edge property column is a {other}"
                )));
            }
        };
        if name.is_empty() {
            return Err(ZuError::InvalidArgument(format!(
                "header column {i}: a property column needs a name, '{cell}' has none"
            )));
        }
        if columns.iter().any(|c| c.name == name) {
            return Err(ZuError::InvalidArgument(format!(
                "header names the column '{name}' twice"
            )));
        }
        roles[i] = CsvRole::Prop(columns.len());
        columns.push(OwnedColumn {
            name: name.to_string(),
            values,
        });
    }
    if node {
        // A node file names one column with `:ID`, and if it named none
        // the first column is it, the same fallback the endpoints take.
        if !roles.contains(&CsvRole::Key) {
            if cells.is_empty() {
                return Err(ZuError::InvalidArgument(
                    "header names no columns, so it names no node".into(),
                ));
            }
            drop_prop(&mut roles, &mut columns, 0);
            roles[0] = CsvRole::Key;
        }
        return Ok((roles, columns));
    }
    if !named_ends {
        // Nothing said which columns the endpoints are, so they are the
        // first two, and whatever the header called them is not a
        // property of the edge they describe.
        if cells.len() < 2 {
            return Err(ZuError::InvalidArgument(
                "header names fewer than two columns, so it names no edge".into(),
            ));
        }
        for i in [0, 1] {
            drop_prop(&mut roles, &mut columns, i);
        }
        roles[0] = CsvRole::Src;
        roles[1] = CsvRole::Dst;
    }
    Ok((roles, columns))
}

/// Takes column `i` out of the property set, for a cell the header
/// typed as a property and the shape of the file says is structure.
/// Every later column's index moves down one to follow it.
fn drop_prop(roles: &mut [CsvRole], columns: &mut Vec<crate::props::OwnedColumn>, i: usize) {
    let CsvRole::Prop(col) = roles[i] else {
        return;
    };
    columns.remove(col);
    for role in roles.iter_mut() {
        if let CsvRole::Prop(other) = role
            && *other > col
        {
            *other -= 1;
        }
    }
}

/// Parses one field into the column's own type and appends it, or says
/// the field is not of that type. An empty field is not a value of any
/// of them: an edge column is dense.
fn push_csv_value(values: &mut crate::props::OwnedValues, cell: &str) -> Option<()> {
    use crate::props::OwnedValues;

    if cell.is_empty() && !matches!(values, OwnedValues::Str(_)) {
        return None;
    }
    match values {
        OwnedValues::Int(v) => v.push(cell.parse::<i64>().ok()? as u64),
        OwnedValues::Float(v) => v.push(cell.parse::<f64>().ok()?),
        OwnedValues::Bool(v) => v.push(match cell {
            _ if cell.eq_ignore_ascii_case("true") => true,
            _ if cell.eq_ignore_ascii_case("false") => false,
            _ => return None,
        }),
        OwnedValues::Str(v) => v.push(cell.as_bytes().to_vec()),
    }
    Some(())
}

/// The header token a column of this type is written with, for the
/// error that says a field is not one.
fn csv_type_name(values: &crate::props::OwnedValues) -> &'static str {
    use crate::props::OwnedValues;

    match values {
        OwnedValues::Int(_) => "INT64",
        OwnedValues::Float(_) => "FLOAT64",
        OwnedValues::Bool(_) => "BOOL",
        OwnedValues::Str(_) => "STRING",
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

/// How many rows of a table of `count` rows group `g` covers, which is
/// a full group everywhere but the last one and nothing at all past the
/// end of the table.
pub(crate) fn group_rows(count: u64, g: u64) -> u32 {
    let first_row = g * GROUP_ROWS as u64;
    count.saturating_sub(first_row).min(GROUP_ROWS as u64) as u32
}

/// Extends `dirs` with empty CSR groups until it has `want` of them, for
/// the end of a rel table whose node table is the shorter of the two.
/// An empty group is what a direction over no rows builds anyway, one
/// offset and no neighbors, so nothing downstream has a second shape to
/// read.
pub(crate) fn pad_direction(
    db: &mut Zu1File,
    dirs: &mut Vec<DirectionMeta>,
    want: usize,
) -> Result<()> {
    while dirs.len() < want {
        dirs.extend(build_direction(db, "row", 0, &[])?);
    }
    Ok(())
}

/// Builds one direction's CSR groups from edges sorted by `(key, other)`.
/// `end` names the endpoint the edges are keyed by, so an id outside
/// the row domain is refused in the words of the end it came from.
pub(crate) fn build_direction(
    db: &mut Zu1File,
    end: &str,
    node_count: u64,
    edges: &[(u32, u32)],
) -> Result<Vec<DirectionMeta>> {
    #[cfg(debug_assertions)]
    for w in edges.windows(2) {
        debug_assert!(w[0] <= w[1], "edges must be sorted");
    }
    let group_count = node_count.div_ceil(GROUP_ROWS as u64).max(1) as usize;
    let mut dirs = Vec::with_capacity(group_count);
    let mut edge_ix = 0usize;
    let mut offsets = Vec::new();
    let mut neighbors: Vec<u64> = Vec::new();
    for g in 0..group_count as u64 {
        let first_row = g * GROUP_ROWS as u64;
        let row_count = group_rows(node_count, g);
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
            "{} edges name a {end} at or above the {node_count} rows of its node table",
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

/// [`bulk_load_as`] for edges with no direction (GH02).
///
/// The edge list is stored the way it is written and nothing is
/// mirrored: the reverse CSR every load builds is what answers the
/// other way round, and the rel table says the two ways are one edge.
/// A pattern that asks for a direction is what tells them apart, so an
/// undirected table costs a directed one nothing on disk.
pub fn bulk_load_undirected_as(
    db: &mut Zu1File,
    node_table: &str,
    rel_table: &str,
    node_count: u64,
    edges: &[(u32, u32)],
) -> Result<Directory> {
    bulk_load_inner(
        db,
        Ends::within(node_table, node_count),
        rel_table,
        edges,
        None,
        true,
    )
}

/// The two ends of a rel table: the node table each is, and how many
/// rows that table holds. [`Ends::within`] is the common case where
/// both ends are one table, which is what an edge list over a single
/// node table loads as.
#[derive(Debug, Clone, Copy)]
pub struct Ends<'a> {
    pub from: (&'a str, u64),
    pub to: (&'a str, u64),
}

impl<'a> Ends<'a> {
    /// Both ends in one node table of `count` rows.
    pub fn within(table: &'a str, count: u64) -> Self {
        Ends {
            from: (table, count),
            to: (table, count),
        }
    }

    /// Ends in two node tables, which may still be the same one.
    pub fn between(from: (&'a str, u64), to: (&'a str, u64)) -> Self {
        Ends { from, to }
    }
}

/// [`bulk_load_as`] for a rel table whose ends are different node
/// tables, so a source id is a row of `ends.from` and a destination id
/// a row of `ends.to`.
///
/// This is what a labelled graph needs: `(:Person)-[:LIVES_IN]->(:City)`
/// is two node tables of unrelated sizes with one rel table between
/// them, and reading it backward from a city has to land in the person
/// table rather than in a row of its own.
pub fn bulk_load_between(
    db: &mut Zu1File,
    ends: Ends<'_>,
    rel_table: &str,
    edges: &[(u32, u32)],
    undirected: bool,
) -> Result<Directory> {
    bulk_load_inner(db, ends, rel_table, edges, None, undirected)
}

/// [`bulk_load_between`] carrying the primary-key index of its FROM
/// table, which is what a dataset of many node files needs.
///
/// The index is sized to `ends.from`, so `key_by_row` is the original id
/// of every row of that table and nothing else. Only one rel table has
/// to carry it: a lookup finds the index through whichever table leaves
/// the node table, so the second one would be the same map written
/// twice.
pub fn bulk_load_between_keyed(
    db: &mut Zu1File,
    ends: Ends<'_>,
    rel_table: &str,
    edges: &[(u32, u32)],
    key_by_row: Option<&[u64]>,
    undirected: bool,
) -> Result<Directory> {
    bulk_load_inner(db, ends, rel_table, edges, key_by_row, undirected)
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
    bulk_load_inner(
        db,
        Ends::within(node_table, node_count),
        rel_table,
        edges,
        key_by_row,
        false,
    )
}

/// Creates a rel table that holds no edges yet, between two node
/// tables the caller has already got the ids of, and returns its id.
///
/// This is what an `INSERT` needs when it writes an edge of a type no
/// table is named by. A bulk load makes a rel table out of an edge list
/// and would make the node tables with it, which is the wrong shape
/// here twice over: the ends are already there, and there are no edges
/// yet because the statement that wants the table is the one about to
/// write the first one.
///
/// The directory is the one a table of no edges has: both row domains
/// as the catalog says they are, and a group per group of the longer
/// end holding an empty CSR each way. The catalog is the caller's to
/// store, which is what keeps the table and the rows of the statement
/// that wanted it in one checkpoint.
pub fn create_empty_rel(
    db: &mut Zu1File,
    catalog: &mut Catalog,
    name: &str,
    from: u32,
    to: u32,
    undirected: bool,
) -> Result<u32> {
    let count_of = |id: u32| -> Result<u64> {
        catalog
            .node_by_id(id)
            .map(|t| t.node_count)
            .ok_or_else(|| ZuError::InvalidArgument(format!("no node table with id {id}")))
    };
    let (from_count, to_count) = (count_of(from)?, count_of(to)?);
    let none: [(u32, u32); 0] = [];
    let mut fwd = build_direction(db, "source", from_count, &none)?;
    let mut bwd = build_direction(db, "destination", to_count, &none)?;
    let group_count = fwd.len().max(bwd.len());
    pad_direction(db, &mut fwd, group_count)?;
    pad_direction(db, &mut bwd, group_count)?;
    let groups = fwd
        .into_iter()
        .zip(bwd)
        .enumerate()
        .map(|(g, (fwd, bwd))| GroupMeta {
            row_count: group_rows(from_count, g as u64),
            edge_base: 0,
            fwd,
            bwd,
        })
        .collect();
    let directory = Directory {
        from_count,
        to_count,
        edge_count: 0,
        keys: None,
        props: NULL_BLOCK,
        groups,
    };
    let root = meta::write_chain(db, &directory.encode())?;
    let id = catalog.upsert_rel_as(name, from, to, 0, undirected)?;
    let mut index = TableIndex::load(db)?;
    index.set(id, root);
    free_chain(db, db.db_header().table_index_root)?;
    let index_root = meta::write_chain(db, &index.encode())?;
    db.db_header_mut().table_index_root = index_root;
    // A table of no edges has the degree statistics of no edges, which
    // is not the same as having none: a planner that finds no entry has
    // to guess, and there is nothing to guess about here.
    let mut stats = crate::stats::Stats::load(db)?;
    stats.rels.insert(
        id,
        crate::stats::RelStats {
            out_hist: crate::stats::degree_histogram(&none),
            in_hist: crate::stats::degree_histogram(&none),
            norms: crate::stats::DegreeStats {
                out: crate::stats::degree_norms(&none),
                inn: crate::stats::degree_norms(&none),
                cross: crate::stats::degree_cross(&none, &none),
            },
            colors: None,
        },
    );
    free_chain(db, db.db_header().stats_root)?;
    stats.store(db)?;
    Ok(id)
}

fn bulk_load_inner(
    db: &mut Zu1File,
    ends: Ends<'_>,
    rel_table: &str,
    edges: &[(u32, u32)],
    key_by_row: Option<&[u64]>,
    undirected: bool,
) -> Result<Directory> {
    let ((from_table, from_count), (to_table, to_count)) = (ends.from, ends.to);
    if let Some(keys) = key_by_row
        && keys.len() as u64 != from_count
    {
        return Err(ZuError::InvalidArgument(format!(
            "{} keys over {from_count} nodes",
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
    let mut fwd = build_direction(db, "source", from_count, edges)?;
    let out_hist = crate::stats::degree_histogram(edges);
    let mut rev: Vec<(u32, u32)> = edges.iter().map(|&(s, d)| (d, s)).collect();
    rev.sort_unstable();
    let mut bwd = build_direction(db, "destination", to_count, &rev)?;
    let in_hist = crate::stats::degree_histogram(&rev);
    let norms = crate::stats::DegreeStats {
        out: crate::stats::degree_norms(edges),
        inn: crate::stats::degree_norms(&rev),
        cross: crate::stats::degree_cross(edges, &rev),
    };
    drop(rev);
    let group_count = fwd.len().max(bwd.len());
    pad_direction(db, &mut fwd, group_count)?;
    pad_direction(db, &mut bwd, group_count)?;
    let bases = group_bases(from_count, edges);
    let groups = fwd
        .into_iter()
        .zip(bwd)
        .enumerate()
        .map(|(g, (fwd, bwd))| GroupMeta {
            row_count: group_rows(from_count, g as u64),
            edge_base: bases.get(g).copied().unwrap_or(edges.len() as u64),
            fwd,
            bwd,
        })
        .collect();
    let directory = Directory {
        from_count,
        to_count,
        edge_count: edges.len() as u64,
        keys: key_by_row
            .map(|keys| write_key_index(db, keys))
            .transpose()?,
        props: NULL_BLOCK,
        groups,
    };
    let root = meta::write_chain(db, &directory.encode())?;
    let from = catalog.upsert_node(from_table, from_count)?;
    let to = if to_table == from_table {
        from
    } else {
        catalog.upsert_node(to_table, to_count)?
    };
    let rel_id = catalog.upsert_rel_as(rel_table, from, to, edges.len() as u64, undirected)?;
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

/// Frees everything the tables of one graph hold, which is the storage
/// half of `DROP GRAPH` (GC04).
///
/// A dropped graph hands its blocks back rather than merely losing its
/// name: the free list grows by what its tables held and the next load
/// writes into those blocks instead of past the end of the file. That
/// is the whole point of the statement on a file of any size, and it is
/// why this walks the tables while the catalog still says which ones
/// were its.
///
/// Nothing is published here. The table index and the statistics are
/// staged, the caller takes the tables out of the catalog it holds, and
/// the checkpoint that stores that catalog makes all three visible at
/// once, so a crash between them leaves the graph exactly as it was.
pub fn free_graph_storage(db: &mut Zu1File, catalog: &Catalog, graph: u32) -> Result<()> {
    let tables = catalog.graph_tables(graph);
    let mut index = TableIndex::load(db)?;
    let mut stats = crate::stats::Stats::load(db)?;
    for (id, kind) in tables {
        if let Some(root) = index.get(id) {
            match kind {
                ElementKind::Node => crate::props::free_props(db, root)?,
                ElementKind::Edge => free_directory(db, root)?,
            }
            index.remove(id);
        }
        // A node table's deleted rows live under a reserved key of the
        // same index, and nothing else names that chain.
        let tombstones = id | crate::fold::TOMBSTONE_KEY;
        if kind == ElementKind::Node
            && let Some(root) = index.get(tombstones)
        {
            free_chain(db, root)?;
            index.remove(tombstones);
        }
        stats.rels.remove(&id);
        stats.cols.remove(&id);
    }
    free_chain(db, db.db_header().table_index_root)?;
    free_chain(db, db.db_header().stats_root)?;
    let index_root = meta::write_chain(db, &index.encode())?;
    db.db_header_mut().table_index_root = index_root;
    stats.store(db)
}

/// Copies everything the tables of one graph hold into blocks of their
/// own, which is the storage half of `CREATE GRAPH ... AS COPY OF`
/// (GC04).
///
/// `tables` pairs each source table id with the id the catalog gave its
/// copy, which the caller has already put in the new graph. The copy is
/// block for block: every segment block of the source is read whole and
/// written into a freshly allocated one, and the directories are
/// re-encoded only because a directory names the blocks its segments
/// live in and those are now different blocks. The bytes of a column,
/// a CSR array and a key index are the source's byte for byte, so the
/// copy costs a read and a write per block and no decode, and it does
/// not matter to it what the columns hold.
///
/// Nothing is shared with the source. A copy that pointed at the same
/// segments would be a second name for one graph, and the first write
/// to either would show up in both; `COPY OF` is a graph that starts
/// out equal and goes its own way.
///
/// Nothing is published here either, as in [`free_graph_storage`]: the
/// table index and the statistics are staged and the checkpoint that
/// stores the catalog makes all three visible at once.
pub fn copy_graph_storage(db: &mut Zu1File, tables: &[(u32, u32, ElementKind)]) -> Result<()> {
    let mut index = TableIndex::load(db)?;
    let mut stats = crate::stats::Stats::load(db)?;
    for &(source, copy, kind) in tables {
        if let Some(root) = index.get(source) {
            let copied = match kind {
                ElementKind::Node => crate::props::copy_props(db, root)?,
                ElementKind::Edge => copy_directory(db, root)?,
            };
            index.set(copy, copied);
        }
        // A node table's deleted rows are part of what it holds: a copy
        // that left them behind would answer a scan with rows the
        // source no longer has.
        if kind == ElementKind::Node
            && let Some(root) = index.get(source | crate::fold::TOMBSTONE_KEY)
        {
            let copied = copy_chain(db, root)?;
            index.set(copy | crate::fold::TOMBSTONE_KEY, copied);
        }
        // The copy holds the same rows, so it plans the same way.
        // Gathering the statistics again would read every column of it
        // for numbers the file already has.
        if let Some(rels) = stats.rels.get(&source).cloned() {
            stats.rels.insert(copy, rels);
        }
        if let Some(cols) = stats.cols.get(&source).cloned() {
            stats.cols.insert(copy, cols);
        }
    }
    free_chain(db, db.db_header().table_index_root)?;
    free_chain(db, db.db_header().stats_root)?;
    let index_root = meta::write_chain(db, &index.encode())?;
    db.db_header_mut().table_index_root = index_root;
    stats.store(db)
}

/// Copies a group directory and everything it points at, answering the
/// root of the copy. The mirror of [`free_directory`], and it walks the
/// same pointers: a block either free walks past or copy walks past is
/// a block the other one leaks.
fn copy_directory(db: &mut Zu1File, root: BlockPtr) -> Result<BlockPtr> {
    let mut directory = Directory::decode(&meta::read_chain(db, root)?)?;
    if directory.props != NULL_BLOCK {
        directory.props = crate::props::copy_props(db, directory.props)?;
    }
    if let Some(keys) = &mut directory.keys {
        for seg in [&mut keys.keys, &mut keys.rows] {
            seg.blocks = copy_blocks(db, &seg.blocks)?;
        }
    }
    for group in &mut directory.groups {
        for seg in [
            &mut group.fwd.offsets,
            &mut group.fwd.neighbors,
            &mut group.bwd.offsets,
            &mut group.bwd.neighbors,
        ] {
            seg.blocks = copy_blocks(db, &seg.blocks)?;
        }
    }
    meta::write_chain(db, &directory.encode())
}

/// Copies a meta chain, answering the root of the copy. The payload is
/// what carries over rather than the blocks, because a chain block
/// holds the pointer to the next one and those are the caller's to
/// hand out.
fn copy_chain(db: &mut Zu1File, root: BlockPtr) -> Result<BlockPtr> {
    let payload = meta::read_chain(db, root)?;
    meta::write_chain(db, &payload)
}

/// Copies a segment's blocks into fresh ones, answering where they
/// landed. A block is read and written whole: what a segment block
/// holds is the encoder's business and none of this function's.
pub(crate) fn copy_blocks(db: &mut Zu1File, blocks: &[BlockPtr]) -> Result<Vec<BlockPtr>> {
    let mut out = Vec::with_capacity(blocks.len());
    for &ptr in blocks {
        let data = db.read_block(ptr)?;
        let copy = db.allocate_block();
        db.write_block(copy, &data)?;
        out.push(copy);
    }
    Ok(out)
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

    /// The group and row a node sits at in the domain `dir` is keyed
    /// by. A node id is a row of the FROM table when it is being read
    /// forward and of the TO table when backward, and the two tables
    /// need not be the same size, so which end is asking decides what
    /// the id is allowed to be.
    fn locate(&self, node: u64, dir: Direction) -> Result<(usize, usize)> {
        let rows = self.directory.rows(dir);
        if node >= rows {
            return Err(ZuError::InvalidArgument(format!(
                "node {node} out of range 0..{rows}"
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
        let (g, row) = self.locate(node, dir)?;
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

    /// `node`'s forward list together with the ordinal of the first
    /// edge in it, so the `i`th neighbor is edge `base + i` of the load
    /// order and an edge property is a slice index rather than a
    /// search.
    ///
    /// [`Self::edge_ordinal`] answers the same question for one pair
    /// and costs a binary search to do it, which is the right trade for
    /// an edge that arrived from somewhere else. A kernel walking the
    /// forward lists already knows where it is, and on a graph where a
    /// pair can repeat the count is the only thing that tells the
    /// copies apart.
    pub fn out_neighbors_from(&mut self, db: &mut Zu1File, node: u64) -> Result<(&[u64], u64)> {
        let (g, row) = self.locate(node, Direction::Fwd)?;
        let base = self.directory.groups[g].edge_base;
        let idx = Direction::Fwd as usize;
        if self.cached_groups[idx].as_ref().map(|(i, _, _)| *i) != Some(g) {
            let (offsets, nbrs) = self.csr_group(db, g, Direction::Fwd)?;
            self.cached_groups[idx] = Some((g, offsets, nbrs));
        }
        let (_, offsets, nbrs) = self.cached_groups[idx].as_ref().expect("just cached");
        let lo = offsets[row] as usize;
        let hi = offsets[row + 1] as usize;
        Ok((&nbrs[lo..hi], base + lo as u64))
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
        let (g, row) = self.locate(node, dir)?;
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
            let (group, _) = self.locate(nodes[at], dir)?;
            let mut end = at + 1;
            while end < nodes.len() && self.locate(nodes[end], dir)?.0 == group {
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
                    let (_, row) = self.locate(node, dir)?;
                    sink(i, offs[row + 1] - offs[row]);
                }
            } else {
                let meta = &self.directory.groups[group].dir(dir).offsets;
                let mut pair = Vec::with_capacity(2);
                for (i, &node) in (at..end).zip(&nodes[at..end]) {
                    let (_, row) = self.locate(node, dir)?;
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
        let (g, row) = self.locate(node, dir)?;
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
        let (g, row) = self.locate(node, dir)?;
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
    /// An edge reached forward could have its ordinal counted out as
    /// the expand walks the list, and the vectorized read does exactly
    /// that; this is the answer for an edge that arrived any other way,
    /// a backward expand above all, where the slot in the backward
    /// array says nothing about the forward one.
    ///
    /// This takes the cached-group path [`Self::neighbors_dir`] takes,
    /// not the point path [`Self::has_edge`] takes, because an edge
    /// property is read once per edge a pattern matched and those edges
    /// arrive together: the point path decodes an offset chunk and a
    /// neighbor chunk per call, which measured at about 5us per
    /// property read, against a binary search over a slice the group
    /// cache already holds. A caller reading one ordinal on its own
    /// pays a group decode for it, which is the same bet the neighbor
    /// list makes.
    ///
    /// A pair that runs twice names two edges with two ordinals, and a
    /// lookup given nothing but the pair cannot pick between them: this
    /// answers with the first, the earliest of the run in load order.
    /// A caller that wants every copy takes [`Self::edge_run`], which
    /// is this plus the length of the run.
    pub fn edge_ordinal(&mut self, db: &mut Zu1File, src: u64, dst: u64) -> Result<Option<u64>> {
        Ok(self.edge_run(db, src, dst)?.map(|(base, _)| base))
    }

    /// The whole run of `src -> dst`: the ordinal of its first edge and
    /// how many edges the pair holds, so the copies are `base`,
    /// `base + 1`, up to `base + count - 1`.
    ///
    /// A pattern that binds both endpoints matches once per edge, not
    /// once per pair, and this is what lets it. The forward list keeps
    /// a pair's copies next to each other, so the run is one
    /// `partition_point` for its start and a scan for its end, and the
    /// scan is over equal values a decoded group already holds.
    pub fn edge_run(&mut self, db: &mut Zu1File, src: u64, dst: u64) -> Result<Option<(u64, u64)>> {
        let (g, row) = self.locate(src, Direction::Fwd)?;
        let base = self.directory.groups[g].edge_base;
        let idx = Direction::Fwd as usize;
        if self.cached_groups[idx].as_ref().map(|(i, _, _)| *i) != Some(g) {
            let (offsets, nbrs) = self.csr_group(db, g, Direction::Fwd)?;
            self.cached_groups[idx] = Some((g, offsets, nbrs));
        }
        let (_, offsets, nbrs) = self.cached_groups[idx].as_ref().expect("just cached");
        let lo = offsets[row] as usize;
        let hi = offsets[row + 1] as usize;
        // partition_point rather than binary_search: a run of equal
        // destinations has the search landing anywhere inside it, and
        // the first slot of the run is the one answer that does not
        // depend on how the halving went.
        let slot = lo + nbrs[lo..hi].partition_point(|&n| n < dst);
        if slot >= hi || nbrs[slot] != dst {
            return Ok(None);
        }
        let end = slot + nbrs[slot..hi].partition_point(|&n| n == dst);
        Ok(Some((base + slot as u64, (end - slot) as u64)))
    }

    /// Point access to the in-neighbor list.
    pub fn in_neighbors_into(&self, db: &mut Zu1File, node: u64, out: &mut Vec<u64>) -> Result<()> {
        self.neighbors_dir_into(db, node, Direction::Bwd, out)
    }

    /// Replaces `out` with the load-order ordinal of every edge in
    /// `node`'s list in `dir`, in list order, so `out[i]` is the
    /// property row of the `i`th neighbor [`Self::neighbors_dir`]
    /// reports for the same arguments.
    ///
    /// This is what a pattern that reads an edge property wants and
    /// [`Self::edge_ordinal`] cannot give it. A pair that runs twice is
    /// two edges with two rows, and a lookup holding nothing but the
    /// pair answers with the first of the run for both, so a walk over
    /// a graph where pairs repeat reads one copy's value for every
    /// copy. Counting the list out instead is exact, and on the
    /// generated finance graphs a pair repeating is the common case
    /// rather than the corner one: an account there sends three hundred
    /// transfers to a hundred and fifty counterparties.
    ///
    /// Forward the answer is a range, the group's base plus the list's
    /// place in it, which is [`Self::out_neighbors_from`]. Backward the
    /// slot in the reverse array says nothing about the forward one, so
    /// each run of one source is looked up once and its copies counted
    /// off from there. The two directions agree on which copy is which,
    /// which is what makes an edge read forward and the same edge read
    /// backward carry the same value: a run of k copies fills k slots
    /// in each direction and the `i`th backward slot takes the `i`th
    /// forward ordinal.
    pub fn neighbor_ordinals_into(
        &mut self,
        db: &mut Zu1File,
        node: u64,
        dir: Direction,
        out: &mut Vec<u64>,
    ) -> Result<()> {
        out.clear();
        match dir {
            Direction::Fwd => {
                let (list, base) = self.out_neighbors_from(db, node)?;
                let len = list.len() as u64;
                out.extend(base..base + len);
            }
            Direction::Bwd => {
                // The sources land in `out` first and are rewritten in
                // place, because the forward lookup below wants the
                // reader and the backward list borrows it.
                out.extend_from_slice(self.neighbors_dir(db, node, Direction::Bwd)?);
                let mut i = 0;
                while i < out.len() {
                    let src = out[i];
                    let mut j = i + 1;
                    while j < out.len() && out[j] == src {
                        j += 1;
                    }
                    // The backward list holds this edge, so the forward
                    // one does too, and a missing ordinal means the two
                    // arrays disagree about the graph.
                    let base = self.edge_ordinal(db, src, node)?.ok_or_else(|| {
                        ZuError::Corrupt {
                            what: "adjacency",
                            detail: format!(
                                "the in-list of node {node} holds {src}, and no forward edge does"
                            ),
                        }
                    })?;
                    for (k, slot) in out[i..j].iter_mut().enumerate() {
                        *slot = base + k as u64;
                    }
                    i = j;
                }
            }
        }
        Ok(())
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

    /// The LDBC header a rel file is written with: the endpoints are
    /// named, the type column is structure rather than a value, and the
    /// three that are left are the edge's properties in their declared
    /// types.
    #[test]
    fn a_typed_csv_header_loads_the_edge_property_columns() {
        use crate::props::OwnedValues;

        let dir = tempfile::tempdir().unwrap();
        let csv = dir.path().join("link.csv");
        std::fs::write(
            &csv,
            ":START_ID,:END_ID,:TYPE,ltype:INT64,weight:FLOAT64,payload:STRING\n\
             0,1,LINK,7,0.5,ada\n\
             1,2,LINK,-8,1.5,kay\n",
        )
        .unwrap();
        let (edges, columns) = read_edge_csv_with_props(&csv, true).unwrap();
        assert_eq!(edges, vec![(0, 1), (1, 2)]);
        assert_eq!(columns.len(), 3);
        assert_eq!(columns[0].name, "ltype");
        assert_eq!(
            columns[0].values,
            OwnedValues::Int(vec![7, (-8i64) as u64]),
            "a signed value keeps its bits"
        );
        assert_eq!(columns[1].values, OwnedValues::Float(vec![0.5, 1.5]));
        assert_eq!(
            columns[2].values,
            OwnedValues::Str(vec![b"ada".to_vec(), b"kay".to_vec()])
        );
        // The same file read the way every caller before this one read
        // it: two columns and nothing else.
        assert_eq!(read_edge_csv(&csv).unwrap(), edges);
    }

    /// A header that names no endpoint still describes an edge list, so
    /// the first two columns are the endpoints and neither is a
    /// property of the edge they name.
    #[test]
    fn an_untyped_csv_header_takes_its_endpoints_by_position() {
        use crate::props::OwnedValues;

        let dir = tempfile::tempdir().unwrap();
        let csv = dir.path().join("edges.csv");
        std::fs::write(&csv, "from,to,since:INT64\n1,2,10\n2,3,20\n").unwrap();
        let (edges, columns) = read_edge_csv_with_props(&csv, true).unwrap();
        assert_eq!(edges, vec![(1, 2), (2, 3)]);
        assert_eq!(columns.len(), 1);
        assert_eq!(columns[0].name, "since");
        assert_eq!(columns[0].values, OwnedValues::Int(vec![10, 20]));
    }

    /// A file with no header names no column, so there is nothing to
    /// load a property out of and the reader says so by loading none.
    #[test]
    fn a_headerless_csv_carries_no_columns() {
        let dir = tempfile::tempdir().unwrap();
        let csv = dir.path().join("edges.csv");
        std::fs::write(&csv, "5,6\n7,8\n").unwrap();
        let (edges, columns) = read_edge_csv_with_props(&csv, true).unwrap();
        assert_eq!(edges, vec![(5, 6), (7, 8)]);
        assert!(columns.is_empty());
    }

    /// Every row owes every column a value. A row that is short one,
    /// which is also what a comma inside a value reads as, is an error
    /// naming the line rather than a column that shifted under the
    /// ordinals of every edge after it.
    #[test]
    fn a_row_that_does_not_match_the_header_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let csv = dir.path().join("edges.csv");
        std::fs::write(&csv, "src,dst,since:INT64\n1,2,10\n2,3\n").unwrap();
        let err = read_edge_csv_with_props(&csv, true).unwrap_err();
        assert!(
            err.to_string().contains("line 3") && err.to_string().contains("2 fields"),
            "unexpected error: {err}"
        );
        std::fs::write(&csv, "src,dst,since:INT64\n1,2,\n").unwrap();
        let err = read_edge_csv_with_props(&csv, true).unwrap_err();
        assert!(
            err.to_string().contains("line 2") && err.to_string().contains("'since'"),
            "unexpected error: {err}"
        );
    }

    /// A type the reader has no column for is refused at the header,
    /// before a value of it has been read as something else.
    #[test]
    fn a_header_type_with_no_column_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let csv = dir.path().join("edges.csv");
        std::fs::write(&csv, "src,dst,born:DATETIME\n1,2,2026-01-01\n").unwrap();
        let err = read_edge_csv_with_props(&csv, true).unwrap_err();
        assert!(
            err.to_string().contains("DATETIME"),
            "unexpected error: {err}"
        );
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
    fn a_forward_list_comes_with_the_ordinal_of_its_first_edge() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("run.zu1");
        // 0 -> 1 three times over, which is what makes counting the
        // only way to tell the copies apart, plus a second source so
        // the base is not always zero.
        let edges = [(0u32, 1u32), (0, 1), (0, 1), (0, 2), (3, 0), (3, 1)];
        {
            let mut db = Zu1File::create(&path).unwrap();
            bulk_load(&mut db, 4, &edges).unwrap();
        }
        let mut db = Zu1File::open(&path).unwrap();
        let mut reader = GraphReader::load(&mut db).unwrap();
        let (nbrs, base) = reader.out_neighbors_from(&mut db, 0).unwrap();
        assert_eq!(nbrs, [1, 1, 1, 2]);
        assert_eq!(base, 0);
        let (nbrs, base) = reader.out_neighbors_from(&mut db, 3).unwrap();
        assert_eq!(nbrs, [0, 1]);
        assert_eq!(base, 4);
        // A node with no out-edges still has a base, the ordinal its
        // first edge would take, so an empty list is an empty range
        // rather than a special case.
        let (nbrs, base) = reader.out_neighbors_from(&mut db, 1).unwrap();
        assert!(nbrs.is_empty());
        assert_eq!(base, 4);
        // The pair alone names the first of the run, and nothing else:
        // that is the answer that does not depend on how the search
        // halved the list.
        assert_eq!(reader.edge_ordinal(&mut db, 0, 1).unwrap(), Some(0));
        assert_eq!(reader.edge_ordinal(&mut db, 0, 2).unwrap(), Some(3));
        assert_eq!(reader.edge_ordinal(&mut db, 1, 0).unwrap(), None);
    }

    #[test]
    fn a_list_read_either_way_names_the_same_edge_rows() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("both.zu1");
        // Two pairs that run more than once, one of them from a source
        // that also sends elsewhere, so a run is neither the whole list
        // nor the start of it in either direction.
        let edges = [(0u32, 1u32), (0, 1), (0, 1), (0, 2), (3, 1), (3, 1), (4, 1)];
        {
            let mut db = Zu1File::create(&path).unwrap();
            bulk_load(&mut db, 5, &edges).unwrap();
        }
        let mut db = Zu1File::open(&path).unwrap();
        let mut reader = GraphReader::load(&mut db).unwrap();

        let mut ords = Vec::new();
        reader
            .neighbor_ordinals_into(&mut db, 0, Direction::Fwd, &mut ords)
            .unwrap();
        assert_eq!(ords, [0, 1, 2, 3]);
        reader
            .neighbor_ordinals_into(&mut db, 3, Direction::Fwd, &mut ords)
            .unwrap();
        assert_eq!(ords, [4, 5]);

        // Node 1's in-list is 0,0,0,3,3,4 and the rows it names are the
        // forward rows of those same edges, so every copy gets its own
        // and none is named twice.
        reader
            .neighbor_ordinals_into(&mut db, 1, Direction::Bwd, &mut ords)
            .unwrap();
        assert_eq!(ords, [0, 1, 2, 4, 5, 6]);
        reader
            .neighbor_ordinals_into(&mut db, 2, Direction::Bwd, &mut ords)
            .unwrap();
        assert_eq!(ords, [3]);

        // Every edge of the graph is named once from each side, which
        // is the property the property read depends on.
        let mut forward = Vec::new();
        for node in 0..5 {
            reader
                .neighbor_ordinals_into(&mut db, node, Direction::Fwd, &mut ords)
                .unwrap();
            forward.extend_from_slice(&ords);
        }
        let mut backward = Vec::new();
        for node in 0..5 {
            reader
                .neighbor_ordinals_into(&mut db, node, Direction::Bwd, &mut ords)
                .unwrap();
            backward.extend_from_slice(&ords);
        }
        forward.sort_unstable();
        backward.sort_unstable();
        assert_eq!(forward, (0..edges.len() as u64).collect::<Vec<_>>());
        assert_eq!(forward, backward);
    }

    #[test]
    fn a_run_reports_its_first_row_and_its_length() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("runs.zu1");
        // The same graph the list walk above reads, so the run a pair
        // names here is the run of rows that walk counts out there.
        let edges = [(0u32, 1u32), (0, 1), (0, 1), (0, 2), (3, 1), (3, 1), (4, 1)];
        {
            let mut db = Zu1File::create(&path).unwrap();
            bulk_load(&mut db, 5, &edges).unwrap();
        }
        let mut db = Zu1File::open(&path).unwrap();
        let mut reader = GraphReader::load(&mut db).unwrap();

        assert_eq!(reader.edge_run(&mut db, 0, 1).unwrap(), Some((0, 3)));
        // A run that ends the list, and one that is the whole list.
        assert_eq!(reader.edge_run(&mut db, 0, 2).unwrap(), Some((3, 1)));
        assert_eq!(reader.edge_run(&mut db, 3, 1).unwrap(), Some((4, 2)));
        assert_eq!(reader.edge_run(&mut db, 4, 1).unwrap(), Some((6, 1)));
        // A pair with no edge, and a source with no list at all.
        assert_eq!(reader.edge_run(&mut db, 0, 3).unwrap(), None);
        assert_eq!(reader.edge_run(&mut db, 1, 0).unwrap(), None);

        // The first row of the run is what the single-edge lookup
        // answers with, and the rows the run names are the rows the
        // forward walk names.
        let mut ords = Vec::new();
        for node in 0..5 {
            reader
                .neighbor_ordinals_into(&mut db, node, Direction::Fwd, &mut ords)
                .unwrap();
            let list = reader.neighbors_dir(&mut db, node, Direction::Fwd).unwrap();
            let list = list.to_vec();
            for (i, &dst) in list.iter().enumerate() {
                let (base, count) = reader.edge_run(&mut db, node, dst).unwrap().unwrap();
                assert_eq!(reader.edge_ordinal(&mut db, node, dst).unwrap(), Some(base));
                assert!(ords[i] >= base && ords[i] < base + count, "{node} -> {dst}");
            }
        }
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
        let mut reader = GraphReader::load(&mut db).unwrap();
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

    /// The ordinal reads the same group cache the neighbor lists read,
    /// so the two have to be interleavable: a walk that asks for a list
    /// and then for an ordinal in another group, over and over, gets
    /// the same answers as a walk that asks for one kind at a time.
    #[test]
    fn ordinals_and_neighbor_lists_share_a_cache_without_disturbing_each_other() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("g.zu1");
        // Enough nodes for several groups, and a source in the first
        // group next to a source in the last one so consecutive calls
        // land in different groups and the single slot has to reload.
        let node_count = 300_000u64;
        let rows = node_count / 4;
        let mut edges: Vec<(u32, u32)> = (0..node_count)
            .step_by(7)
            .flat_map(|s| [(s as u32, (s % 991) as u32), (s as u32, (s % 97) as u32)])
            .collect();
        let edges = sorted_edges(&mut edges).to_vec();
        {
            let mut db = Zu1File::create(&path).unwrap();
            bulk_load(&mut db, node_count, &edges).unwrap();
        }
        let mut db = Zu1File::open(&path).unwrap();
        let mut reader = GraphReader::load(&mut db).unwrap();

        let apart: Vec<(Vec<u64>, Option<u64>)> = edges
            .iter()
            .step_by(97)
            .map(|&(s, d)| {
                (
                    reader.neighbors(&mut db, u64::from(s)).unwrap().to_vec(),
                    reader
                        .edge_ordinal(&mut db, u64::from(s), u64::from(d))
                        .unwrap(),
                )
            })
            .collect();

        // The same reads with a far-away group touched in between, which
        // is what evicts the slot each time.
        for (i, &(s, d)) in edges.iter().step_by(97).enumerate() {
            let far = if s as u64 > rows { 0 } else { node_count - 7 };
            let _ = reader.neighbors(&mut db, far).unwrap();
            let ordinal = reader
                .edge_ordinal(&mut db, u64::from(s), u64::from(d))
                .unwrap();
            let _ = reader.neighbors(&mut db, far).unwrap();
            let list = reader.neighbors(&mut db, u64::from(s)).unwrap().to_vec();
            assert_eq!((list, ordinal), apart[i], "edge {s}->{d}");
        }

        // Every ordinal is the edge's index in the load order, which is
        // the sorted list, whatever order they were asked for in.
        for (want, &(s, d)) in edges.iter().enumerate().step_by(391) {
            assert_eq!(
                reader
                    .edge_ordinal(&mut db, u64::from(s), u64::from(d))
                    .unwrap(),
                Some(want as u64),
                "edge {s}->{d}"
            );
        }
    }

    #[test]
    fn a_rel_table_runs_between_two_node_tables() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("g.zu1");
        // Four people, two cities, and one of each that the edges never
        // name, so the row domains are visibly different sizes and the
        // ends cannot be standing in for one another.
        let edges = [(0u32, 1u32), (1, 0), (2, 1)];
        {
            let mut db = Zu1File::create(&path).unwrap();
            bulk_load_between(
                &mut db,
                Ends::between(("person", 4), ("city", 2)),
                "lives_in",
                &edges,
                false,
            )
            .unwrap();
        }
        let mut db = Zu1File::open(&path).unwrap();
        let catalog = Catalog::load(&mut db).unwrap();
        let rel = catalog.rel_by_name("lives_in").unwrap();
        assert_ne!(rel.from, rel.to);
        assert_eq!(catalog.node_by_id(rel.from).unwrap().name, "person");
        assert_eq!(catalog.node_by_id(rel.to).unwrap().name, "city");
        assert_eq!(catalog.node_by_name("person").unwrap().node_count, 4);
        assert_eq!(catalog.node_by_name("city").unwrap().node_count, 2);

        let mut reader = GraphReader::load_table(&mut db, "lives_in").unwrap();
        assert_eq!(reader.directory().from_count, 4);
        assert_eq!(reader.directory().to_count, 2);
        assert_eq!(reader.neighbors(&mut db, 2).unwrap(), &[1]);
        assert!(reader.neighbors(&mut db, 3).unwrap().is_empty());
        assert_eq!(
            reader
                .neighbors_dir(&mut db, 1, Direction::Bwd)
                .unwrap()
                .to_vec(),
            vec![0, 2]
        );
        // A person row is not a city row: 3 is a person and reading it
        // as a destination is out of range, not an empty answer.
        assert!(reader.neighbors_dir(&mut db, 3, Direction::Bwd).is_err());
        assert!(reader.neighbors(&mut db, 4).is_err());
        assert_eq!(reader.edge_ordinal(&mut db, 1, 0).unwrap(), Some(1));
        assert!(reader.directory().one_domain().is_err());
    }

    #[test]
    fn the_shorter_end_of_a_rel_table_pads_out_to_the_longer() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("g.zu1");
        // The destination table spans two groups and the source table
        // three rows, so the forward direction runs out of groups first
        // and every reader still finds one to read.
        let far = GROUP_ROWS + 3;
        let edges = [(0u32, 5u32), (1, far), (2, 0)];
        {
            let mut db = Zu1File::create(&path).unwrap();
            bulk_load_between(
                &mut db,
                Ends::between(("post", 3), ("tag", u64::from(far) + 1)),
                "tagged",
                &edges,
                false,
            )
            .unwrap();
        }
        let mut db = Zu1File::open(&path).unwrap();
        let mut reader = GraphReader::load_table(&mut db, "tagged").unwrap();
        assert_eq!(reader.directory().groups.len(), 2);
        assert_eq!(reader.directory().groups[1].row_count, 0);
        assert_eq!(reader.neighbors(&mut db, 1).unwrap(), &[u64::from(far)]);
        assert_eq!(
            reader
                .neighbors_dir(&mut db, u64::from(far), Direction::Bwd)
                .unwrap(),
            &[1]
        );
        assert!(reader.neighbors(&mut db, 3).is_err());
        assert_eq!(
            reader
                .degree_of(&mut db, u64::from(far), Direction::Bwd)
                .unwrap(),
            1
        );
    }

    #[test]
    fn hostile_group_count_rejected() {
        // A header claiming u32::MAX groups must die on the size check,
        // not in the allocator.
        let mut bytes = DIRECTORY_VERSION.to_le_bytes().to_vec();
        bytes.extend_from_slice(&10u64.to_le_bytes());
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
    fn an_older_directory_version_is_refused() {
        // A version 8 header is a byte shorter and carries one node
        // count where version 9 carries two. Reading it as version 9
        // would take the edge count for the to domain and every row
        // range check after that would be wrong, so the version gate
        // has to fire before any field is believed.
        let mut bytes = 8u16.to_le_bytes().to_vec();
        bytes.extend_from_slice(&10u64.to_le_bytes());
        bytes.extend_from_slice(&20u64.to_le_bytes());
        bytes.extend_from_slice(&NULL_BLOCK.to_le_bytes());
        bytes.extend_from_slice(&1u32.to_le_bytes());
        bytes.push(0);
        let err = Directory::decode(&bytes).unwrap_err();
        assert!(
            matches!(
                err,
                ZuError::Unsupported {
                    what: "group directory version",
                    id: 8
                }
            ),
            "{err}"
        );
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

    /// The property and CSR blocks a table's storage names, in the
    /// order the copy walks them, so two tables' lists line up entry
    /// for entry when one is a copy of the other. The meta chain is not
    /// in here: a chain block holds the pointers to the segments and to
    /// the next chain block, and those are what a copy has of its own.
    fn segment_blocks(db: &mut Zu1File, root: BlockPtr, kind: ElementKind) -> Vec<BlockPtr> {
        let mut out = Vec::new();
        match kind {
            ElementKind::Node => props_segment_blocks(db, root, &mut out),
            ElementKind::Edge => {
                let directory = Directory::decode(&meta::read_chain(db, root).unwrap()).unwrap();
                if directory.props != NULL_BLOCK {
                    props_segment_blocks(db, directory.props, &mut out);
                }
                if let Some(keys) = &directory.keys {
                    out.extend(&keys.keys.blocks);
                    out.extend(&keys.rows.blocks);
                }
                for group in &directory.groups {
                    for seg in [
                        &group.fwd.offsets,
                        &group.fwd.neighbors,
                        &group.bwd.offsets,
                        &group.bwd.neighbors,
                    ] {
                        out.extend(&seg.blocks);
                    }
                }
            }
        }
        out
    }

    fn props_segment_blocks(db: &mut Zu1File, root: BlockPtr, out: &mut Vec<BlockPtr>) {
        let directory =
            crate::props::PropsDirectory::decode(&meta::read_chain(db, root).unwrap()).unwrap();
        out.extend(directory.labels.iter().flat_map(|m| &m.blocks));
        for col in &directory.columns {
            out.extend(&col.meta.blocks);
            out.extend(col.validity.iter().flat_map(|m| &m.blocks));
        }
    }

    #[test]
    fn a_graph_copy_holds_the_same_bytes_in_blocks_of_its_own() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("copy.zu1");
        let mut db = Zu1File::create(&path).unwrap();
        let mut edges = vec![(0, 1), (0, 3), (1, 2), (3, 0)];
        let edges = sorted_edges(&mut edges).to_vec();
        bulk_load_keyed(
            &mut db,
            "person",
            "follows",
            4,
            &edges,
            Some(&[10, 20, 30, 40]),
        )
        .unwrap();
        crate::props::store_props(
            &mut db,
            "person",
            &[("age", crate::props::PropValues::Int(&[31, 32, 33, 34]))],
        )
        .unwrap();
        db.checkpoint().unwrap();

        let mut catalog = Catalog::load(&mut db).unwrap();
        let source = catalog.home_graph_id();
        let target = catalog
            .add_graph(
                "twin",
                crate::catalog::ROOT_SCHEMA,
                crate::catalog::GraphTypeOf::Open,
            )
            .unwrap();
        let tables = catalog.copy_graph_tables(source, target).unwrap();
        copy_graph_storage(&mut db, &tables).unwrap();
        catalog.store(&mut db).unwrap();
        // The catalog validates on store, so a copy that got its table
        // names or its endpoints wrong never gets this far.
        assert_eq!(tables.len(), 2);

        let index = TableIndex::load(&mut db).unwrap();
        for (from, to, kind) in tables {
            let (from, to) = (index.get(from).unwrap(), index.get(to).unwrap());
            let source = segment_blocks(&mut db, from, kind);
            let copy = segment_blocks(&mut db, to, kind);
            assert_eq!(source.len(), copy.len(), "{kind:?}");
            assert!(!source.is_empty(), "{kind:?} stores something");
            for (&a, &b) in source.iter().zip(&copy) {
                assert_ne!(a, b, "a copied block is a block of its own");
                assert_eq!(db.read_block(a).unwrap(), db.read_block(b).unwrap());
            }
        }
        crate::verify(&path).unwrap();
    }
}
