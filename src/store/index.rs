//! The store's derived index (SPEC §9.1): schema, rows and the SQL that reads
//! and writes them. Everything here is either read from the ledgers or is a
//! cursor; nothing is the truth.

use super::bindings::Fact;
use super::blocks::{Located, Meta};
use super::layout::Fingerprint;
use crate::model::{Agent, Event, Link, Parent, Source};
use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use std::path::Path;

/// Bumped whenever the schema changes; an index at another version is rebuilt.
pub const SCHEMA_VERSION: i64 = 4;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS agents (
    id              TEXT PRIMARY KEY,
    path            TEXT NOT NULL UNIQUE,
    size            INTEGER NOT NULL,
    mtime_ns        INTEGER NOT NULL,
    last_seq        INTEGER NOT NULL,
    byte_len        INTEGER NOT NULL,
    created_at      TEXT,
    source          TEXT,
    parent_agent    TEXT,
    parent_event    TEXT,
    parent_relation TEXT,
    links           TEXT NOT NULL,
    title           TEXT,
    compressed      INTEGER NOT NULL DEFAULT 0,
    tail_size       INTEGER NOT NULL DEFAULT 0,
    tail_mtime_ns   INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS agents_parent ON agents(parent_agent);
CREATE TABLE IF NOT EXISTS links (
    agent  TEXT NOT NULL,
    idx    INTEGER NOT NULL,
    rel    TEXT NOT NULL,
    target TEXT,
    event  TEXT,
    PRIMARY KEY (agent, idx)
);
CREATE INDEX IF NOT EXISTS links_target ON links(target, rel);
CREATE TABLE IF NOT EXISTS events (
    agent  TEXT NOT NULL,
    seq    INTEGER NOT NULL,
    offset INTEGER NOT NULL,
    len    INTEGER NOT NULL,
    id     TEXT NOT NULL,
    kind   TEXT NOT NULL,
    ts     TEXT,
    grp    TEXT,
    turn   TEXT,
    PRIMARY KEY (agent, seq)
);
CREATE INDEX IF NOT EXISTS events_id ON events(agent, id);
-- Filled from `protoledger/bound`, `protoledger/unbound` and
-- `protoledger/diverged` events whenever a ledger is indexed; `cursor` is the
-- only column that is not read from a ledger (sync's position, SPEC §9.1).
CREATE TABLE IF NOT EXISTS bindings (
    binding   TEXT PRIMARY KEY,
    agent     TEXT NOT NULL,
    format    TEXT NOT NULL,
    variant   TEXT,
    location  TEXT NOT NULL,
    native_id TEXT,
    mode      TEXT NOT NULL,
    active    INTEGER NOT NULL,
    ended     TEXT,
    cursor    TEXT
);
CREATE INDEX IF NOT EXISTS bindings_agent ON bindings(agent);
-- One row per sealed block of a compressed ledger (SPEC §10.8): every column
-- is a meta-frame field or a position in the block file.
CREATE TABLE IF NOT EXISTS blocks (
    agent           TEXT NOT NULL,
    offset          INTEGER NOT NULL,
    len             INTEGER NOT NULL,
    physical_offset INTEGER NOT NULL,
    physical_len    INTEGER NOT NULL,
    dict            TEXT,
    first_seq       INTEGER NOT NULL,
    last_seq        INTEGER NOT NULL,
    generation      INTEGER NOT NULL,
    PRIMARY KEY (agent, offset)
);
-- One row per dictionary in <store>/dicts/ (SPEC §10.8), with its size.
CREATE TABLE IF NOT EXISTS dicts (
    id   TEXT PRIMARY KEY,
    size INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS blocks_dict ON blocks(dict);
";

const TABLES: [&str; 7] = ["meta", "agents", "links", "events", "bindings", "blocks", "dicts"];

/// One agent as the index knows it.
#[derive(Debug, Clone, PartialEq)]
pub struct AgentInfo {
    pub id: String,
    /// Path of the ledger, relative to the store root (`ledgers/<file>.jsonl`):
    /// its plain file, which names it in either form (SPEC §10.2).
    pub path: String,
    /// File size and modification time when the index last read the file
    /// (for a compressed ledger, its block file's; 0 when it has none).
    pub size: u64,
    pub mtime_ns: i64,
    pub last_seq: u64,
    /// Bytes of the file that hold complete, indexed lines.
    pub byte_len: u64,
    pub created_at: Option<String>,
    pub source: Option<Source>,
    pub parent: Option<Parent>,
    pub links: Vec<Link>,
    /// The text of the newest `title` event.
    pub title: Option<String>,
    /// Stored compressed (SPEC §10) when the index last read it.
    pub compressed: bool,
    /// A compressed ledger's hot tail: its size and modification time when
    /// the index last read it (0 for a plain ledger).
    pub tail_size: u64,
    pub tail_mtime_ns: i64,
}

impl AgentInfo {
    /// The ledger's size as garbage collection measures it (SPEC §9.5): its
    /// file's size, or, compressed, its decompressed stream's length.
    pub fn ledger_size(&self) -> u64 {
        if self.compressed { self.byte_len } else { self.size }
    }

    /// The newest modification time among the ledger's files.
    pub fn newest_mtime_ns(&self) -> i64 {
        self.mtime_ns.max(self.tail_mtime_ns)
    }

    pub(crate) fn fingerprint(&self) -> Fingerprint {
        Fingerprint { compressed: self.compressed, size: self.size, mtime_ns: self.mtime_ns, tail_size: self.tail_size, tail_mtime_ns: self.tail_mtime_ns }
    }
}

/// One sealed block as the index knows it (SPEC §10.8).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockRow {
    pub agent: String,
    /// Where the block starts in the decompressed stream, and its length there.
    pub offset: u64,
    pub len: u64,
    /// Where its meta frame starts in the block file, and the two frames' length.
    pub physical_offset: u64,
    pub physical_len: u64,
    /// The dictionary that encoded it, or `None` for none.
    pub dict: Option<String>,
    pub first_seq: u64,
    pub last_seq: u64,
    pub generation: u64,
}

impl BlockRow {
    pub(crate) fn of(agent: &str, b: &Located) -> BlockRow {
        BlockRow {
            agent: agent.to_string(),
            offset: b.meta.offset,
            len: b.meta.len,
            physical_offset: b.physical_offset,
            physical_len: b.physical_len,
            dict: b.meta.dict.clone(),
            first_seq: b.meta.first_seq,
            last_seq: b.meta.last_seq,
            generation: b.meta.generation,
        }
    }

    /// The block as a reader's hint: where to look, and what to expect
    /// there. `zlen` is not in the row; a reader takes it from the meta frame
    /// it finds, after checking that frame against the row.
    pub(crate) fn located(&self) -> Located {
        let meta = Meta {
            block: super::blocks::BLOCK_VERSION,
            dict: self.dict.clone(),
            first_seq: self.first_seq,
            generation: self.generation,
            last_seq: self.last_seq,
            len: self.len,
            offset: self.offset,
            zlen: 0,
        };
        Located { meta, physical_offset: self.physical_offset, physical_len: self.physical_len }
    }
}

/// One event as the index knows it: where it is, and what it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventRow {
    pub agent: String,
    pub seq: u64,
    /// Byte offset of the event's line in its ledger.
    pub offset: u64,
    /// Length of the line, including its newline.
    pub len: u64,
    pub id: String,
    /// The kind as written (an unknown kind keeps its own name).
    pub kind: String,
    pub ts: Option<String>,
    pub group: Option<String>,
    pub turn: Option<String>,
}

impl EventRow {
    pub fn of(agent: &str, offset: u64, len: u64, e: &Event) -> EventRow {
        EventRow {
            agent: agent.to_string(),
            seq: e.seq,
            offset,
            len,
            id: e.id.clone(),
            kind: e.unknown_kind.clone().unwrap_or_else(|| e.kind.as_str().to_string()),
            ts: e.ts.clone(),
            group: e.group.clone(),
            turn: e.turn.clone(),
        }
    }
}

pub fn open(path: &Path) -> Result<Connection> {
    let db = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
    db.busy_timeout(std::time::Duration::from_secs(10))?;
    db.pragma_update(None, "journal_mode", "WAL")?;
    db.pragma_update(None, "synchronous", "NORMAL")?;
    let version: Option<i64> =
        db.query_row("SELECT value FROM meta WHERE key = 'schema'", [], |r| r.get::<_, String>(0)).optional().ok().flatten().and_then(|v| v.parse().ok());
    if version.is_some_and(|v| v != SCHEMA_VERSION) {
        drop_all(&db)?;
    }
    create(&db)?;
    Ok(db)
}

fn create(db: &Connection) -> Result<()> {
    db.execute_batch(SCHEMA)?;
    db.execute("INSERT OR REPLACE INTO meta (key, value) VALUES ('schema', ?1)", params![SCHEMA_VERSION.to_string()])?;
    Ok(())
}

fn drop_all(db: &Connection) -> Result<()> {
    for t in TABLES {
        db.execute_batch(&format!("DROP TABLE IF EXISTS {t};"))?;
    }
    Ok(())
}

/// Empty every table and recreate the schema.
pub fn reset(db: &Connection) -> Result<()> {
    drop_all(db)?;
    create(db)
}

fn json<T: serde::Serialize>(v: &T) -> String {
    serde_json::to_string(v).expect("index value serialises")
}

/// Write an agent's header row and its links, replacing any previous row.
/// `last_seq`, `byte_len`, the files' fingerprint and `title` start as given.
pub fn put_agent(tx: &Transaction, agent: &Agent, path: &str, fp: Fingerprint, last_seq: u64, byte_len: u64, title: Option<&str>) -> Result<()> {
    let (pa, pe, pr) = match &agent.parent {
        Some(p) => (Some(p.agent.as_str()), p.event.as_deref(), Some(json(&p.relation))),
        None => (None, None, None),
    };
    tx.execute(
        "INSERT OR REPLACE INTO agents
         (id, path, size, mtime_ns, last_seq, byte_len, created_at, source,
          parent_agent, parent_event, parent_relation, links, title,
          compressed, tail_size, tail_mtime_ns)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
        params![
            agent.id,
            path,
            fp.size as i64,
            fp.mtime_ns,
            last_seq as i64,
            byte_len as i64,
            agent.created_at,
            agent.source.as_ref().map(json),
            pa,
            pe,
            pr,
            json(&agent.links),
            title,
            fp.compressed,
            fp.tail_size as i64,
            fp.tail_mtime_ns,
        ],
    )?;
    tx.execute("DELETE FROM links WHERE agent = ?1", params![agent.id])?;
    for (i, l) in agent.links.iter().enumerate() {
        tx.execute("INSERT INTO links (agent, idx, rel, target, event) VALUES (?1, ?2, ?3, ?4, ?5)", params![agent.id, i as i64, l.rel, l.agent, l.event])?;
    }
    Ok(())
}

pub fn put_event(tx: &Transaction, r: &EventRow) -> Result<()> {
    tx.execute(
        "INSERT OR REPLACE INTO events (agent, seq, offset, len, id, kind, ts, grp, turn)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![r.agent, r.seq as i64, r.offset as i64, r.len as i64, r.id, r.kind, r.ts, r.group, r.turn],
    )?;
    Ok(())
}

/// Move an agent's progress markers after events were indexed.
pub fn advance(tx: &Transaction, agent: &str, last_seq: u64, byte_len: u64, fp: Fingerprint, title: Option<&str>) -> Result<()> {
    tx.execute(
        "UPDATE agents SET last_seq = ?2, byte_len = ?3, size = ?4, mtime_ns = ?5,
                title = COALESCE(?6, title), compressed = ?7, tail_size = ?8, tail_mtime_ns = ?9
         WHERE id = ?1",
        params![agent, last_seq as i64, byte_len as i64, fp.size as i64, fp.mtime_ns, title, fp.compressed, fp.tail_size as i64, fp.tail_mtime_ns],
    )?;
    Ok(())
}

pub fn forget_agent(tx: &Transaction, agent: &str) -> Result<()> {
    tx.execute("DELETE FROM blocks WHERE agent = ?1", params![agent])?;
    tx.execute("DELETE FROM bindings WHERE agent = ?1", params![agent])?;
    tx.execute("DELETE FROM events WHERE agent = ?1", params![agent])?;
    tx.execute("DELETE FROM links WHERE agent = ?1", params![agent])?;
    tx.execute("DELETE FROM agents WHERE id = ?1", params![agent])?;
    Ok(())
}

const AGENT_COLS: &str = "id, path, size, mtime_ns, last_seq, byte_len, created_at, source,
                          parent_agent, parent_event, parent_relation, links, title,
                          compressed, tail_size, tail_mtime_ns";

fn agent_row(r: &rusqlite::Row) -> rusqlite::Result<AgentInfo> {
    fn de<T: serde::de::DeserializeOwned>(s: Option<String>, col: usize) -> rusqlite::Result<Option<T>> {
        s.map(|s| serde_json::from_str(&s).map_err(|e| rusqlite::Error::FromSqlConversionFailure(col, rusqlite::types::Type::Text, Box::new(e)))).transpose()
    }
    let parent = match r.get::<_, Option<String>>(8)? {
        Some(agent) => Some(Parent { agent, event: r.get(9)?, relation: de(r.get(10)?, 10)?.expect("parent_relation is set with parent_agent") }),
        None => None,
    };
    Ok(AgentInfo {
        id: r.get(0)?,
        path: r.get(1)?,
        size: r.get::<_, i64>(2)? as u64,
        mtime_ns: r.get(3)?,
        last_seq: r.get::<_, i64>(4)? as u64,
        byte_len: r.get::<_, i64>(5)? as u64,
        created_at: r.get(6)?,
        source: de(r.get(7)?, 7)?,
        parent,
        links: de(r.get(11)?, 11)?.unwrap_or_default(),
        title: r.get(12)?,
        compressed: r.get(13)?,
        tail_size: r.get::<_, i64>(14)? as u64,
        tail_mtime_ns: r.get(15)?,
    })
}

fn event_row(r: &rusqlite::Row) -> rusqlite::Result<EventRow> {
    Ok(EventRow {
        agent: r.get(0)?,
        seq: r.get::<_, i64>(1)? as u64,
        offset: r.get::<_, i64>(2)? as u64,
        len: r.get::<_, i64>(3)? as u64,
        id: r.get(4)?,
        kind: r.get(5)?,
        ts: r.get(6)?,
        group: r.get(7)?,
        turn: r.get(8)?,
    })
}

const EVENT_COLS: &str = "agent, seq, offset, len, id, kind, ts, grp, turn";

fn agents_where(db: &Connection, clause: &str, p: impl rusqlite::Params) -> Result<Vec<AgentInfo>> {
    let sql = format!("SELECT {AGENT_COLS} FROM agents {clause} ORDER BY id");
    let mut st = db.prepare(&sql)?;
    let rows = st.query_map(p, agent_row)?.collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

pub fn agents(db: &Connection) -> Result<Vec<AgentInfo>> {
    agents_where(db, "", [])
}

pub fn agent(db: &Connection, id: &str) -> Result<Option<AgentInfo>> {
    Ok(agents_where(db, "WHERE id = ?1", params![id])?.pop())
}

pub fn children(db: &Connection, id: &str) -> Result<Vec<AgentInfo>> {
    agents_where(db, "WHERE parent_agent = ?1", params![id])
}

pub fn linked_from(db: &Connection, id: &str, rel: &str) -> Result<Vec<AgentInfo>> {
    agents_where(db, "WHERE id IN (SELECT agent FROM links WHERE target = ?1 AND rel = ?2)", params![id, rel])
}

pub fn events(db: &Connection, agent: &str, from_seq: u64, limit: usize) -> Result<Vec<EventRow>> {
    let sql = format!("SELECT {EVENT_COLS} FROM events WHERE agent = ?1 AND seq >= ?2 ORDER BY seq LIMIT ?3");
    let mut st = db.prepare(&sql)?;
    let limit = i64::try_from(limit).unwrap_or(i64::MAX);
    let rows = st.query_map(params![agent, from_seq as i64, limit], event_row)?.collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

pub fn event(db: &Connection, agent: &str, seq: u64) -> Result<Option<EventRow>> {
    let sql = format!("SELECT {EVENT_COLS} FROM events WHERE agent = ?1 AND seq = ?2");
    Ok(db.query_row(&sql, params![agent, seq as i64], event_row).optional()?)
}

/// A binding row (SPEC §9.2), as the ledgers say it. The sync cursor is not
/// part of it (it is not read from a ledger); see the private `cursor` module.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BindingRow {
    pub binding: String,
    /// The agent whose ledger records the binding.
    pub agent: String,
    pub format: String,
    pub variant: Option<String>,
    pub location: String,
    pub native_id: Option<String>,
    /// `projection` or `attachment`.
    pub mode: String,
    /// Followed by sync: bound, and neither unbound nor diverged since.
    pub active: bool,
    /// Why it stopped: `unbound` or `diverged`.
    pub ended: Option<String>,
}

const BINDING_COLS: &str = "binding, agent, format, variant, location, native_id, mode, active, ended";

fn binding_row(r: &rusqlite::Row) -> rusqlite::Result<BindingRow> {
    Ok(BindingRow {
        binding: r.get(0)?,
        agent: r.get(1)?,
        format: r.get(2)?,
        variant: r.get(3)?,
        location: r.get(4)?,
        native_id: r.get(5)?,
        mode: r.get(6)?,
        active: r.get::<_, i64>(7)? != 0,
        ended: r.get(8)?,
    })
}

pub fn bindings(db: &Connection) -> Result<Vec<BindingRow>> {
    let mut st = db.prepare(&format!("SELECT {BINDING_COLS} FROM bindings ORDER BY binding"))?;
    let rows = st.query_map([], binding_row)?.collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

pub fn binding(db: &Connection, binding: &str) -> Result<Option<BindingRow>> {
    let sql = format!("SELECT {BINDING_COLS} FROM bindings WHERE binding = ?1");
    Ok(db.query_row(&sql, params![binding], binding_row).optional()?)
}

/// Apply one binding fact read from `agent`'s ledger. Facts are applied in
/// ledger order; an unbound or diverged fact for a binding not (yet) bound in
/// the index changes nothing.
pub fn apply_fact(tx: &Transaction, agent: &str, f: &Fact) -> Result<()> {
    match f {
        Fact::Bound(b) => {
            tx.execute(
                "INSERT INTO bindings (binding, agent, format, variant, location, native_id, mode, active, ended, cursor)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 1, NULL, NULL)
                 ON CONFLICT(binding) DO UPDATE SET agent = ?2, format = ?3, variant = ?4, location = ?5,
                     native_id = ?6, mode = ?7, active = 1, ended = NULL",
                params![b.binding, agent, b.format, b.variant, b.location, b.native_id, b.mode.as_str()],
            )?;
        }
        Fact::Unbound { binding } => {
            tx.execute("UPDATE bindings SET active = 0, ended = 'unbound' WHERE binding = ?1 AND agent = ?2", params![binding, agent])?;
        }
        Fact::Diverged { binding, .. } => {
            tx.execute("UPDATE bindings SET active = 0, ended = 'diverged' WHERE binding = ?1 AND agent = ?2 AND active = 1", params![binding, agent])?;
        }
    }
    Ok(())
}

/// Every cursor the index holds, by binding.
pub fn cursors(db: &Connection) -> Result<Vec<(String, String)>> {
    let mut st = db.prepare("SELECT binding, cursor FROM bindings WHERE cursor IS NOT NULL ORDER BY binding")?;
    let rows = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

pub fn cursors_of(db: &Connection, agent: &str) -> Result<Vec<(String, String)>> {
    let mut st = db.prepare("SELECT binding, cursor FROM bindings WHERE agent = ?1 AND cursor IS NOT NULL")?;
    let rows = st.query_map(params![agent], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

pub fn cursor(db: &Connection, binding: &str) -> Result<Option<String>> {
    Ok(db.query_row("SELECT cursor FROM bindings WHERE binding = ?1", params![binding], |r| r.get::<_, Option<String>>(0)).optional()?.flatten())
}

/// Set the cursors of bindings the index holds; a binding it does not hold is skipped.
pub fn set_cursors(db: &Connection, cursors: &[(String, String)]) -> Result<()> {
    for (b, c) in cursors {
        db.execute("UPDATE bindings SET cursor = ?2 WHERE binding = ?1", params![b, c])?;
    }
    Ok(())
}

/// Replace an agent's block rows (SPEC §10.8).
pub fn put_blocks(tx: &Transaction, agent: &str, rows: &[BlockRow]) -> Result<()> {
    tx.execute("DELETE FROM blocks WHERE agent = ?1", params![agent])?;
    for b in rows {
        tx.execute(
            "INSERT INTO blocks (agent, offset, len, physical_offset, physical_len, dict, first_seq, last_seq, generation)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                agent,
                b.offset as i64,
                b.len as i64,
                b.physical_offset as i64,
                b.physical_len as i64,
                b.dict,
                b.first_seq as i64,
                b.last_seq as i64,
                b.generation as i64
            ],
        )?;
    }
    Ok(())
}

/// An agent's block rows, in stream order.
pub fn blocks(db: &Connection, agent: &str) -> Result<Vec<BlockRow>> {
    let mut st = db.prepare(
        "SELECT agent, offset, len, physical_offset, physical_len, dict, first_seq, last_seq, generation
         FROM blocks WHERE agent = ?1 ORDER BY offset",
    )?;
    let rows = st
        .query_map(params![agent], |r| {
            Ok(BlockRow {
                agent: r.get(0)?,
                offset: r.get::<_, i64>(1)? as u64,
                len: r.get::<_, i64>(2)? as u64,
                physical_offset: r.get::<_, i64>(3)? as u64,
                physical_len: r.get::<_, i64>(4)? as u64,
                dict: r.get(5)?,
                first_seq: r.get::<_, i64>(6)? as u64,
                last_seq: r.get::<_, i64>(7)? as u64,
                generation: r.get::<_, i64>(8)? as u64,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// Replace the dictionary rows with `dicts` (`(id, size)`).
pub fn put_dicts(tx: &Transaction, dicts: &[(String, u64)]) -> Result<()> {
    tx.execute("DELETE FROM dicts", [])?;
    for (id, size) in dicts {
        tx.execute("INSERT INTO dicts (id, size) VALUES (?1, ?2)", params![id, *size as i64])?;
    }
    Ok(())
}

/// The dictionary rows, by id: `(id, size)`.
pub fn dicts(db: &Connection) -> Result<Vec<(String, u64)>> {
    let mut st = db.prepare("SELECT id, size FROM dicts ORDER BY id")?;
    let rows = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as u64)))?.collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// How many sealed blocks of each ledger each dictionary encoded:
/// `(dict, agent, blocks)`, by dictionary then agent. `dict` is `None` for
/// blocks compressed with no dictionary.
pub fn dict_usage(db: &Connection) -> Result<Vec<(Option<String>, String, u64)>> {
    let mut st = db.prepare("SELECT dict, agent, COUNT(*) FROM blocks GROUP BY dict, agent ORDER BY dict, agent")?;
    let rows = st
        .query_map([], |r| Ok((r.get::<_, Option<String>>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)? as u64)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}
