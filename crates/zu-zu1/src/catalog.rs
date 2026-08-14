//! The catalog and the table index, the two meta chains that make a zu1
//! file self-describing.
//!
//! The catalog behind `catalog_root` holds table definitions: node tables
//! with their row domain, rel tables with their FROM and TO node tables
//! and edge count. The table index behind `table_index_root` maps a rel
//! table id to the root of its group directory chain, so DDL rewrites
//! only the catalog while a bulk load rewrites only the index entry it
//! touches. Both encodings are version-prefixed and little-endian per
//! `docs/04-storage-zu1-format.md` §1.
//!
//! Catalog layout: `version: u16`, `node_table_count: u32`,
//! `rel_table_count: u32`, `label_count: u32`, then per node table
//! `id: u32`, `name_len: u16` + UTF-8 bytes, `node_count: u64`,
//! `declared_label_count: u16` and that many `label: u16`, then per rel
//! table `id: u32`, name, `from: u32`, `to: u32`, `edge_count: u64`,
//! then the label dictionary as `label_count` names.
//!
//! The dictionary is per file and a label is its position in it, which
//! is what makes a label set a bitset: a node's labels are a word with
//! one bit per dictionary entry, and a pattern's labels are a mask over
//! the same word. A node table declares which labels its rows may
//! carry, the first of which is the table's own name and the one every
//! row carries, so a label a table never declares prunes that table at
//! plan time rather than being tested a row at a time.
//!
//! Table index layout: `version: u16`, `entry_count: u32`, then per
//! entry `table_id: u32`, `directory_root: u64`.

use zu_common::{Result, ZuError};

use crate::file::{BlockPtr, NULL_BLOCK, Zu1File};
use crate::meta;

/// Version 1 had no labels: a node's only label was the name of its
/// table. Version 2 adds the label dictionary and the set each node
/// table declares. A version 1 catalog still reads, and reads as the
/// graph it always was, one label per table carrying the table's name.
const CATALOG_VERSION: u16 = 2;
const TABLE_INDEX_VERSION: u16 = 1;

/// The label dictionary is bounded by the width of the bitset a node
/// carries. One word per node is the fast path the whole design is
/// sized for (LDBC SNB declares 13 labels, LinkBench and Graph500 one),
/// and a wider set waits for the spill representation.
pub const MAX_LABELS: usize = 64;

/// Table ids live in the 14-bit field of `NodeId`.
pub const MAX_TABLE_ID: u32 = (1 << 14) - 1;

const MAX_NAME_LEN: usize = 256;

/// A node table: the row domain `0..node_count` that rel tables index
/// into. Property columns land with the column catalog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeTable {
    pub id: u32,
    pub name: String,
    pub node_count: u64,
    /// The labels rows of this table may carry, as dictionary ids.
    /// `labels[0]` is the table's own name, which every row carries;
    /// the rest are optional per row and the bitset column says which
    /// rows have them.
    pub labels: Vec<u16>,
}

impl NodeTable {
    /// The label every row of this table carries: the table's name.
    pub fn primary_label(&self) -> u16 {
        self.labels[0]
    }

    /// The declared set as a mask, which is what a plan time prune
    /// tests a pattern's mask against.
    pub fn label_mask(&self) -> u64 {
        self.labels.iter().fold(0u64, |m, &l| m | 1 << l)
    }
}

/// A rel table: a bulk-loaded CSR pair over `from` and `to` node tables,
/// with its group directory reachable through the table index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelTable {
    pub id: u32,
    pub name: String,
    pub from: u32,
    pub to: u32,
    pub edge_count: u64,
}

/// The table definitions of one zu1 file. Names share a single
/// namespace across both kinds, so a rel table cannot shadow a node
/// table.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Catalog {
    nodes: Vec<NodeTable>,
    rels: Vec<RelTable>,
    labels: Vec<String>,
}

fn corrupt(what: &'static str, detail: String) -> ZuError {
    ZuError::Corrupt { what, detail }
}

fn encode_name(out: &mut Vec<u8>, name: &str) {
    out.extend_from_slice(&(name.len() as u16).to_le_bytes());
    out.extend_from_slice(name.as_bytes());
}

fn decode_name(bytes: &[u8], pos: &mut usize, what: &'static str) -> Result<String> {
    let len = bytes
        .get(*pos..*pos + 2)
        .ok_or_else(|| corrupt(what, "truncated name length".into()))?;
    let len = u16::from_le_bytes(len.try_into().unwrap()) as usize;
    *pos += 2;
    if len == 0 || len > MAX_NAME_LEN {
        return Err(corrupt(
            what,
            format!("name length {len} out of 1..{MAX_NAME_LEN}"),
        ));
    }
    let raw = bytes
        .get(*pos..*pos + len)
        .ok_or_else(|| corrupt(what, "truncated name".into()))?;
    *pos += len;
    String::from_utf8(raw.to_vec()).map_err(|_| corrupt(what, "name is not UTF-8".into()))
}

fn read_u32(bytes: &[u8], pos: &mut usize, what: &'static str) -> Result<u32> {
    let raw = bytes
        .get(*pos..*pos + 4)
        .ok_or_else(|| corrupt(what, "truncated entry".into()))?;
    *pos += 4;
    Ok(u32::from_le_bytes(raw.try_into().unwrap()))
}

fn read_u16(bytes: &[u8], pos: &mut usize, what: &'static str) -> Result<u16> {
    let raw = bytes
        .get(*pos..*pos + 2)
        .ok_or_else(|| corrupt(what, "truncated entry".into()))?;
    *pos += 2;
    Ok(u16::from_le_bytes(raw.try_into().unwrap()))
}

fn read_u64(bytes: &[u8], pos: &mut usize, what: &'static str) -> Result<u64> {
    let raw = bytes
        .get(*pos..*pos + 8)
        .ok_or_else(|| corrupt(what, "truncated entry".into()))?;
    *pos += 8;
    Ok(u64::from_le_bytes(raw.try_into().unwrap()))
}

impl Catalog {
    /// Loads the committed catalog; a `NULL_BLOCK` root reads as empty.
    pub fn load(db: &mut Zu1File) -> Result<Self> {
        let root = db.db_header().catalog_root;
        if root == NULL_BLOCK {
            return Ok(Self::default());
        }
        Self::decode(&meta::read_chain(db, root)?)
    }

    pub fn node_tables(&self) -> &[NodeTable] {
        &self.nodes
    }

    pub fn rel_tables(&self) -> &[RelTable] {
        &self.rels
    }

    pub fn node_by_name(&self, name: &str) -> Option<&NodeTable> {
        self.nodes.iter().find(|t| t.name == name)
    }

    pub fn rel_by_name(&self, name: &str) -> Option<&RelTable> {
        self.rels.iter().find(|t| t.name == name)
    }

    pub fn node_by_id(&self, id: u32) -> Option<&NodeTable> {
        self.nodes.iter().find(|t| t.id == id)
    }

    pub fn rel_by_id(&self, id: u32) -> Option<&RelTable> {
        self.rels.iter().find(|t| t.id == id)
    }

    /// The label dictionary, a label's id being its position.
    pub fn labels(&self) -> &[String] {
        &self.labels
    }

    pub fn label_id(&self, name: &str) -> Option<u16> {
        self.labels.iter().position(|l| l == name).map(|i| i as u16)
    }

    pub fn label_name(&self, id: u16) -> Option<&str> {
        self.labels.get(id as usize).map(String::as_str)
    }

    /// Interns a label name and returns its id, which is stable for the
    /// life of the file because ids are positions and nothing is ever
    /// removed from the dictionary.
    pub fn intern_label(&mut self, name: &str) -> Result<u16> {
        if let Some(id) = self.label_id(name) {
            return Ok(id);
        }
        if name.is_empty() || name.len() > MAX_NAME_LEN {
            return Err(ZuError::InvalidArgument(format!(
                "label name length {} out of 1..{MAX_NAME_LEN}",
                name.len()
            )));
        }
        if self.labels.len() == MAX_LABELS {
            return Err(ZuError::Unsupported {
                what: "a label dictionary wider than one bitset word",
                id: MAX_LABELS as u32,
            });
        }
        self.labels.push(name.to_string());
        Ok((self.labels.len() - 1) as u16)
    }

    /// Declares that rows of `table` may carry `label`, interning the
    /// name if the graph has not seen it, and returns its id. Declaring
    /// is what a plan time prune reads: a table that never declared a
    /// label cannot hold a row with it.
    pub fn declare_label(&mut self, table: u32, label: &str) -> Result<u16> {
        let id = self.intern_label(label)?;
        let table = self
            .nodes
            .iter_mut()
            .find(|t| t.id == table)
            .ok_or_else(|| ZuError::InvalidArgument(format!("no node table with id {table}")))?;
        if !table.labels.contains(&id) {
            table.labels.push(id);
        }
        Ok(id)
    }

    /// Every node table that declares `label`, which is the scan set a
    /// pattern naming that label alone runs over.
    pub fn tables_with_label(&self, label: u16) -> Vec<u32> {
        self.nodes
            .iter()
            .filter(|t| t.labels.contains(&label))
            .map(|t| t.id)
            .collect()
    }

    fn name_taken_by_other_kind(&self, name: &str, node: bool) -> bool {
        if node {
            self.rel_by_name(name).is_some()
        } else {
            self.node_by_name(name).is_some()
        }
    }

    fn next_id(&self) -> Result<u32> {
        let used = self
            .nodes
            .iter()
            .map(|t| t.id)
            .chain(self.rels.iter().map(|t| t.id))
            .max();
        match used {
            None => Ok(0),
            Some(id) if id < MAX_TABLE_ID => Ok(id + 1),
            Some(_) => Err(ZuError::InvalidArgument(format!(
                "table id space exhausted at {MAX_TABLE_ID}"
            ))),
        }
    }

    /// Creates or updates a node table and returns its id. The row
    /// domain only grows: a load declaring fewer nodes than an earlier
    /// one leaves the count alone, so rel tables already built over the
    /// larger domain stay valid.
    pub fn upsert_node(&mut self, name: &str, node_count: u64) -> Result<u32> {
        if self.name_taken_by_other_kind(name, true) {
            return Err(ZuError::InvalidArgument(format!(
                "'{name}' is already a rel table"
            )));
        }
        if let Some(t) = self.nodes.iter_mut().find(|t| t.name == name) {
            t.node_count = t.node_count.max(node_count);
            return Ok(t.id);
        }
        let id = self.next_id()?;
        // The table's name is a label like any other, and the one every
        // row carries, so it is interned before the table exists and
        // heads the declared set.
        let primary = self.intern_label(name)?;
        self.nodes.push(NodeTable {
            id,
            name: name.to_string(),
            node_count,
            labels: vec![primary],
        });
        Ok(id)
    }

    /// Creates or updates a rel table and returns its id.
    pub fn upsert_rel(&mut self, name: &str, from: u32, to: u32, edge_count: u64) -> Result<u32> {
        if self.name_taken_by_other_kind(name, false) {
            return Err(ZuError::InvalidArgument(format!(
                "'{name}' is already a node table"
            )));
        }
        if let Some(t) = self.rels.iter_mut().find(|t| t.name == name) {
            t.from = from;
            t.to = to;
            t.edge_count = edge_count;
            return Ok(t.id);
        }
        let id = self.next_id()?;
        self.rels.push(RelTable {
            id,
            name: name.to_string(),
            from,
            to,
            edge_count,
        });
        Ok(id)
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&CATALOG_VERSION.to_le_bytes());
        out.extend_from_slice(&(self.nodes.len() as u32).to_le_bytes());
        out.extend_from_slice(&(self.rels.len() as u32).to_le_bytes());
        out.extend_from_slice(&(self.labels.len() as u32).to_le_bytes());
        for t in &self.nodes {
            out.extend_from_slice(&t.id.to_le_bytes());
            encode_name(&mut out, &t.name);
            out.extend_from_slice(&t.node_count.to_le_bytes());
            out.extend_from_slice(&(t.labels.len() as u16).to_le_bytes());
            for &label in &t.labels {
                out.extend_from_slice(&label.to_le_bytes());
            }
        }
        for t in &self.rels {
            out.extend_from_slice(&t.id.to_le_bytes());
            encode_name(&mut out, &t.name);
            out.extend_from_slice(&t.from.to_le_bytes());
            out.extend_from_slice(&t.to.to_le_bytes());
            out.extend_from_slice(&t.edge_count.to_le_bytes());
        }
        for label in &self.labels {
            encode_name(&mut out, label);
        }
        out
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        const WHAT: &str = "catalog";
        let head = bytes
            .get(..10)
            .ok_or_else(|| corrupt(WHAT, "truncated header".into()))?;
        let version = u16::from_le_bytes(head[..2].try_into().unwrap());
        if version == 0 || version > CATALOG_VERSION {
            return Err(ZuError::Unsupported {
                what: "catalog version",
                id: u32::from(version),
            });
        }
        let node_count = u32::from_le_bytes(head[2..6].try_into().unwrap()) as usize;
        let rel_count = u32::from_le_bytes(head[6..10].try_into().unwrap()) as usize;
        let mut pos = 10;
        let label_count = if version >= 2 {
            read_u32(bytes, &mut pos, WHAT)? as usize
        } else {
            0
        };
        let mut catalog = Self::default();
        for _ in 0..node_count {
            let id = read_u32(bytes, &mut pos, WHAT)?;
            let name = decode_name(bytes, &mut pos, WHAT)?;
            let node_count = read_u64(bytes, &mut pos, WHAT)?;
            let mut labels = Vec::new();
            if version >= 2 {
                let count = read_u16(bytes, &mut pos, WHAT)? as usize;
                if count > label_count {
                    return Err(corrupt(
                        WHAT,
                        format!("table '{name}' declares {count} of {label_count} labels"),
                    ));
                }
                for _ in 0..count {
                    labels.push(read_u16(bytes, &mut pos, WHAT)?);
                }
            }
            catalog.nodes.push(NodeTable {
                id,
                name,
                node_count,
                labels,
            });
        }
        for _ in 0..rel_count {
            let id = read_u32(bytes, &mut pos, WHAT)?;
            let name = decode_name(bytes, &mut pos, WHAT)?;
            let from = read_u32(bytes, &mut pos, WHAT)?;
            let to = read_u32(bytes, &mut pos, WHAT)?;
            let edge_count = read_u64(bytes, &mut pos, WHAT)?;
            catalog.rels.push(RelTable {
                id,
                name,
                from,
                to,
                edge_count,
            });
        }
        for _ in 0..label_count {
            catalog.labels.push(decode_name(bytes, &mut pos, WHAT)?);
        }
        if pos != bytes.len() {
            return Err(corrupt(WHAT, "trailing bytes".into()));
        }
        if version < 2 {
            // A version 1 file is the graph it always was: one label
            // per node table, the table's name, carried by every row.
            // Interning them in table order is what makes reading such
            // a file twice give the same ids.
            for i in 0..catalog.nodes.len() {
                let name = catalog.nodes[i].name.clone();
                let id = catalog.intern_label(&name)?;
                catalog.nodes[i].labels.push(id);
            }
        }
        catalog.validate()?;
        Ok(catalog)
    }

    fn validate(&self) -> Result<()> {
        let mut ids: Vec<u32> = self
            .nodes
            .iter()
            .map(|t| t.id)
            .chain(self.rels.iter().map(|t| t.id))
            .collect();
        for &id in &ids {
            if id > MAX_TABLE_ID {
                return Err(corrupt(
                    "catalog",
                    format!("table id {id} above {MAX_TABLE_ID}"),
                ));
            }
        }
        let total = ids.len();
        ids.sort_unstable();
        ids.dedup();
        if ids.len() != total {
            return Err(corrupt("catalog", "duplicate table id".into()));
        }
        let mut names: Vec<&str> = self
            .nodes
            .iter()
            .map(|t| t.name.as_str())
            .chain(self.rels.iter().map(|t| t.name.as_str()))
            .collect();
        names.sort_unstable();
        if names.windows(2).any(|w| w[0] == w[1]) {
            return Err(corrupt("catalog", "duplicate table name".into()));
        }
        if self.labels.len() > MAX_LABELS {
            return Err(corrupt(
                "catalog",
                format!("{} labels above {MAX_LABELS}", self.labels.len()),
            ));
        }
        let mut label_names: Vec<&str> = self.labels.iter().map(String::as_str).collect();
        label_names.sort_unstable();
        if label_names.windows(2).any(|w| w[0] == w[1]) {
            return Err(corrupt("catalog", "duplicate label name".into()));
        }
        for table in &self.nodes {
            // The declared set heads with the table's own name, which is
            // the label every row carries and the one a pattern naming
            // the table is asking for.
            match table.labels.first() {
                Some(&first) if self.label_name(first) == Some(table.name.as_str()) => {}
                _ => {
                    return Err(corrupt(
                        "catalog",
                        format!("node table '{}' does not declare its own name", table.name),
                    ));
                }
            }
            let mut declared = table.labels.clone();
            declared.sort_unstable();
            let total = declared.len();
            declared.dedup();
            if declared.len() != total {
                return Err(corrupt(
                    "catalog",
                    format!("node table '{}' declares a label twice", table.name),
                ));
            }
            if let Some(&worst) = declared.last()
                && worst as usize >= self.labels.len()
            {
                return Err(corrupt(
                    "catalog",
                    format!(
                        "node table '{}' declares label {worst} of {}",
                        table.name,
                        self.labels.len()
                    ),
                ));
            }
        }
        for rel in &self.rels {
            for end in [rel.from, rel.to] {
                if self.node_by_id(end).is_none() {
                    return Err(corrupt(
                        "catalog",
                        format!(
                            "rel table '{}' references missing node table id {end}",
                            rel.name
                        ),
                    ));
                }
            }
        }
        Ok(())
    }
}

/// The table index: rel table id to group directory root.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TableIndex {
    entries: Vec<(u32, BlockPtr)>,
}

impl TableIndex {
    /// Loads the committed index; a `NULL_BLOCK` root reads as empty.
    pub fn load(db: &mut Zu1File) -> Result<Self> {
        let root = db.db_header().table_index_root;
        if root == NULL_BLOCK {
            return Ok(Self::default());
        }
        Self::decode(&meta::read_chain(db, root)?)
    }

    pub fn entries(&self) -> &[(u32, BlockPtr)] {
        &self.entries
    }

    pub fn get(&self, table_id: u32) -> Option<BlockPtr> {
        self.entries
            .iter()
            .find(|(id, _)| *id == table_id)
            .map(|&(_, root)| root)
    }

    pub fn set(&mut self, table_id: u32, root: BlockPtr) {
        match self.entries.iter_mut().find(|(id, _)| *id == table_id) {
            Some(entry) => entry.1 = root,
            None => self.entries.push((table_id, root)),
        }
    }

    pub fn remove(&mut self, table_id: u32) {
        self.entries.retain(|(id, _)| *id != table_id);
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&TABLE_INDEX_VERSION.to_le_bytes());
        out.extend_from_slice(&(self.entries.len() as u32).to_le_bytes());
        for &(id, root) in &self.entries {
            out.extend_from_slice(&id.to_le_bytes());
            out.extend_from_slice(&root.to_le_bytes());
        }
        out
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        const WHAT: &str = "table index";
        let head = bytes
            .get(..6)
            .ok_or_else(|| corrupt(WHAT, "truncated header".into()))?;
        let version = u16::from_le_bytes(head[..2].try_into().unwrap());
        if version != TABLE_INDEX_VERSION {
            return Err(ZuError::Unsupported {
                what: "table index version",
                id: u32::from(version),
            });
        }
        let count = u32::from_le_bytes(head[2..6].try_into().unwrap()) as usize;
        // Twelve bytes per entry; a count the payload cannot hold is
        // rejected before it sizes an allocation.
        if count > bytes.len().saturating_sub(6) / 12 {
            return Err(corrupt(WHAT, "truncated entry".into()));
        }
        let mut pos = 6;
        let mut entries = Vec::with_capacity(count);
        for _ in 0..count {
            let id = read_u32(bytes, &mut pos, WHAT)?;
            let root = read_u64(bytes, &mut pos, WHAT)?;
            if root == NULL_BLOCK {
                return Err(corrupt(
                    WHAT,
                    format!("table {id} has a null directory root"),
                ));
            }
            entries.push((id, root));
        }
        if pos != bytes.len() {
            return Err(corrupt(WHAT, "trailing bytes".into()));
        }
        let mut ids: Vec<u32> = entries.iter().map(|&(id, _)| id).collect();
        ids.sort_unstable();
        ids.dedup();
        if ids.len() != entries.len() {
            return Err(corrupt(WHAT, "duplicate table id".into()));
        }
        Ok(Self { entries })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Catalog {
        let mut c = Catalog::default();
        let person = c.upsert_node("person", 500).unwrap();
        let org = c.upsert_node("org", 40).unwrap();
        c.upsert_rel("follows", person, person, 4000).unwrap();
        c.upsert_rel("works_at", person, org, 450).unwrap();
        c
    }

    #[test]
    fn roundtrip() {
        let c = sample();
        assert_eq!(Catalog::decode(&c.encode()).unwrap(), c);
        let follows = c.rel_by_name("follows").unwrap();
        assert_eq!(follows.from, c.node_by_name("person").unwrap().id);
        assert_eq!(follows.edge_count, 4000);
    }

    #[test]
    fn upsert_updates_in_place() {
        let mut c = sample();
        let before = c.node_by_name("person").unwrap().id;
        // The row domain only grows.
        assert_eq!(c.upsert_node("person", 100).unwrap(), before);
        assert_eq!(c.node_by_name("person").unwrap().node_count, 500);
        assert_eq!(c.upsert_node("person", 900).unwrap(), before);
        assert_eq!(c.node_by_name("person").unwrap().node_count, 900);
        let rel = c.rel_by_name("follows").unwrap().id;
        assert_eq!(c.upsert_rel("follows", before, before, 9).unwrap(), rel);
        assert_eq!(c.rel_by_name("follows").unwrap().edge_count, 9);
        assert_eq!(c.node_tables().len(), 2);
        assert_eq!(c.rel_tables().len(), 2);
    }

    #[test]
    fn one_namespace_across_kinds() {
        let mut c = sample();
        assert!(c.upsert_node("follows", 1).is_err());
        assert!(c.upsert_rel("person", 0, 0, 1).is_err());
    }

    #[test]
    fn decode_rejects_bad_payloads() {
        let good = sample().encode();
        // Wrong version.
        let mut bad = good.clone();
        bad[0] = 99;
        assert!(matches!(
            Catalog::decode(&bad),
            Err(ZuError::Unsupported { .. })
        ));
        // Truncation at every prefix length must error, never panic.
        for len in 0..good.len() {
            assert!(Catalog::decode(&good[..len]).is_err(), "prefix {len}");
        }
        // Trailing bytes.
        let mut bad = good.clone();
        bad.push(0);
        assert!(Catalog::decode(&bad).is_err());
        // A rel endpoint pointing at a missing node table.
        let mut c = sample();
        c.rels[0].from = 77;
        assert!(Catalog::decode(&c.encode()).is_err());
        // Duplicate ids and names.
        let mut c = sample();
        c.nodes[1].id = c.nodes[0].id;
        assert!(Catalog::decode(&c.encode()).is_err());
        let mut c = sample();
        c.nodes[1].name = c.nodes[0].name.clone();
        assert!(Catalog::decode(&c.encode()).is_err());
        // Id above the 14-bit NodeId field.
        let mut c = sample();
        c.nodes[0].id = MAX_TABLE_ID + 1;
        c.rels.clear();
        assert!(Catalog::decode(&c.encode()).is_err());
    }

    #[test]
    fn a_label_is_declared_once_and_named_by_id() {
        let mut c = sample();
        let person = c.node_by_name("person").unwrap().id;
        let org = c.node_by_name("org").unwrap().id;
        // Every table starts with its own name, which is why the ids
        // here begin at 2.
        assert_eq!(c.labels(), ["person", "org"]);
        assert_eq!(c.declare_label(person, "Employee").unwrap(), 2);
        assert_eq!(c.declare_label(org, "Employee").unwrap(), 2);
        assert_eq!(c.declare_label(person, "Employee").unwrap(), 2);
        assert_eq!(c.node_by_id(person).unwrap().labels, [0, 2]);
        assert_eq!(c.label_id("Employee"), Some(2));
        assert_eq!(c.label_id("Manager"), None);
        assert_eq!(c.label_name(1), Some("org"));
        assert_eq!(c.label_name(9), None);
        assert_eq!(c.tables_with_label(2), [person, org]);
        assert_eq!(c.node_by_id(person).unwrap().label_mask(), 0b101);
        assert!(c.declare_label(person, "").is_err());
        assert!(c.declare_label(4242, "Employee").is_err());
        assert_eq!(Catalog::decode(&c.encode()).unwrap(), c);
        // One word per row is the whole budget, so the dictionary ends
        // at 64 names and says which limit it hit.
        for i in c.labels().len()..MAX_LABELS {
            c.declare_label(person, &format!("L{i}")).unwrap();
        }
        assert!(matches!(
            c.declare_label(person, "one_too_many"),
            Err(ZuError::Unsupported { .. })
        ));
    }

    #[test]
    fn a_version_one_catalog_reads_as_one_label_per_table() {
        // Version 1 had no dictionary, so the bytes end after the rel
        // tables and every table carries its own name and nothing else.
        let c = sample();
        let mut old = 1u16.to_le_bytes().to_vec();
        old.extend_from_slice(&(c.nodes.len() as u32).to_le_bytes());
        old.extend_from_slice(&(c.rels.len() as u32).to_le_bytes());
        for t in &c.nodes {
            old.extend_from_slice(&t.id.to_le_bytes());
            encode_name(&mut old, &t.name);
            old.extend_from_slice(&t.node_count.to_le_bytes());
        }
        for t in &c.rels {
            old.extend_from_slice(&t.id.to_le_bytes());
            encode_name(&mut old, &t.name);
            old.extend_from_slice(&t.from.to_le_bytes());
            old.extend_from_slice(&t.to.to_le_bytes());
            old.extend_from_slice(&t.edge_count.to_le_bytes());
        }
        assert_eq!(Catalog::decode(&old).unwrap(), c);
    }

    #[test]
    fn decode_rejects_bad_label_sets() {
        // A table that does not carry its own name first.
        let mut c = sample();
        c.nodes[0].labels[0] = 1;
        assert!(Catalog::decode(&c.encode()).is_err());
        // A label id with no name behind it.
        let mut c = sample();
        c.nodes[0].labels.push(7);
        assert!(Catalog::decode(&c.encode()).is_err());
        // The same label declared twice on one table.
        let mut c = sample();
        c.nodes[0].labels.push(0);
        assert!(Catalog::decode(&c.encode()).is_err());
        // Two names for one id.
        let mut c = sample();
        c.labels[1] = c.labels[0].clone();
        assert!(Catalog::decode(&c.encode()).is_err());
        // A count that outruns the dictionary dies before it allocates.
        let mut c = sample();
        c.nodes[0].labels = vec![0; 300];
        assert!(Catalog::decode(&c.encode()).is_err());
    }

    #[test]
    fn table_index_roundtrip_and_rejects() {
        let mut ix = TableIndex::default();
        ix.set(3, 12);
        ix.set(5, 40);
        ix.set(3, 19);
        assert_eq!(TableIndex::decode(&ix.encode()).unwrap(), ix);
        assert_eq!(ix.get(3), Some(19));
        assert_eq!(ix.get(4), None);
        ix.remove(3);
        assert_eq!(ix.get(3), None);
        assert_eq!(ix.entries().len(), 1);
        // Null root, duplicate id, truncation, trailing bytes.
        let mut bad = TableIndex::default();
        bad.set(1, NULL_BLOCK);
        assert!(TableIndex::decode(&bad.encode()).is_err());
        let raw = {
            let mut ix = TableIndex::default();
            ix.set(1, 7);
            ix.entries.push((1, 8));
            ix.encode()
        };
        assert!(TableIndex::decode(&raw).is_err());
        let good = ix.encode();
        for len in 0..good.len() {
            assert!(TableIndex::decode(&good[..len]).is_err(), "prefix {len}");
        }
        let mut bad = good.clone();
        bad.push(0);
        assert!(TableIndex::decode(&bad).is_err());
        // A six byte header claiming u32::MAX entries must die on the
        // size check, not in the allocator.
        let mut hostile = TABLE_INDEX_VERSION.to_le_bytes().to_vec();
        hostile.extend_from_slice(&u32::MAX.to_le_bytes());
        assert!(TableIndex::decode(&hostile).is_err());
    }
}
