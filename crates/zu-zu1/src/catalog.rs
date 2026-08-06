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
//! `rel_table_count: u32`, then per node table `id: u32`,
//! `name_len: u16` + UTF-8 bytes, `node_count: u64`, then per rel table
//! `id: u32`, name, `from: u32`, `to: u32`, `edge_count: u64`.
//!
//! Table index layout: `version: u16`, `entry_count: u32`, then per
//! entry `table_id: u32`, `directory_root: u64`.

use zu_common::{Result, ZuError};

use crate::file::{BlockPtr, NULL_BLOCK, Zu1File};
use crate::meta;

const CATALOG_VERSION: u16 = 1;
const TABLE_INDEX_VERSION: u16 = 1;

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
        self.nodes.push(NodeTable {
            id,
            name: name.to_string(),
            node_count,
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
        for t in &self.nodes {
            out.extend_from_slice(&t.id.to_le_bytes());
            encode_name(&mut out, &t.name);
            out.extend_from_slice(&t.node_count.to_le_bytes());
        }
        for t in &self.rels {
            out.extend_from_slice(&t.id.to_le_bytes());
            encode_name(&mut out, &t.name);
            out.extend_from_slice(&t.from.to_le_bytes());
            out.extend_from_slice(&t.to.to_le_bytes());
            out.extend_from_slice(&t.edge_count.to_le_bytes());
        }
        out
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        const WHAT: &str = "catalog";
        let head = bytes
            .get(..10)
            .ok_or_else(|| corrupt(WHAT, "truncated header".into()))?;
        let version = u16::from_le_bytes(head[..2].try_into().unwrap());
        if version != CATALOG_VERSION {
            return Err(ZuError::Unsupported {
                what: "catalog version",
                id: u32::from(version),
            });
        }
        let node_count = u32::from_le_bytes(head[2..6].try_into().unwrap()) as usize;
        let rel_count = u32::from_le_bytes(head[6..10].try_into().unwrap()) as usize;
        let mut pos = 10;
        let mut catalog = Self::default();
        for _ in 0..node_count {
            let id = read_u32(bytes, &mut pos, WHAT)?;
            let name = decode_name(bytes, &mut pos, WHAT)?;
            let node_count = read_u64(bytes, &mut pos, WHAT)?;
            catalog.nodes.push(NodeTable {
                id,
                name,
                node_count,
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
        if pos != bytes.len() {
            return Err(corrupt(WHAT, "trailing bytes".into()));
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
    }
}
