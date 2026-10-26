//! Format adapters: one module per harness, each importing its native store
//! into a ledger set and exporting a ledger set back out (SPEC §7).

use crate::ledger::LedgerSet;
use crate::model::{Event, Kind};
use anyhow::{Result, bail};
use std::collections::BTreeMap;
use std::path::Path;

pub mod atif;
pub mod claude_code;
pub mod codex;
pub mod copilot_cli;
pub mod cursor;
pub mod deepagents;
pub mod droid;
pub mod gemini_cli;
pub mod hermes;
pub mod letta_code;
pub mod omp;
pub mod openclaw;
pub mod opencode;
pub mod openhands;
pub mod pi;
pub(crate) mod pi_session;
pub mod protoledger;
pub mod slack;
#[cfg(feature = "sqlite")]
pub(crate) mod sqlite_header;

/// One supported shape of a format's native data. A format has several when
/// its storage changed between producer versions.
#[derive(Debug, Clone, Copy)]
pub struct Variant {
    pub name: &'static str,
    /// Producer versions this variant covers, human-readable (e.g. ">= 1.2, < 2.0").
    pub versions: &'static str,
    pub import: bool,
    pub export: bool,
    pub notes: &'static str,
}

#[derive(Debug, Clone)]
pub struct Detection {
    pub variant: &'static str,
    /// 0-100.
    pub confidence: u8,
    pub version: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct ImportOptions {
    /// Force a variant instead of detecting it.
    pub variant: Option<String>,
    /// Restore carried protoledger events found in native records (SPEC §6).
    pub restore_carried: bool,
}

#[derive(Debug, Clone, Default)]
pub struct ExportOptions {
    pub variant: Option<String>,
    /// Write `protoledger` carrier fields where the format tolerates them.
    pub carry: bool,
    /// Producer version to impersonate where the format records one.
    pub target_version: Option<String>,
}

impl ImportOptions {
    pub fn standard() -> Self {
        ImportOptions { variant: None, restore_carried: true }
    }
}

impl ExportOptions {
    pub fn standard() -> Self {
        ExportOptions { variant: None, carry: true, target_version: None }
    }
    pub fn no_carry() -> Self {
        ExportOptions { variant: None, carry: false, target_version: None }
    }
}

/// Files produced by an export, keyed by path relative to the output root.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Output {
    pub files: BTreeMap<String, Vec<u8>>,
    /// Files that are shared, append-only indexes in the harness's store
    /// (Codex's `session_index.jsonl`): written into an existing store, their
    /// lines are appended (skipping lines already present), never replaced.
    pub appends: std::collections::BTreeSet<String>,
}

impl Output {
    pub fn put(&mut self, path: impl Into<String>, bytes: impl Into<Vec<u8>>) {
        self.files.insert(path.into(), bytes.into());
    }
    /// Mark `path` as an append-only shared index (see `appends`).
    pub fn put_append(&mut self, path: impl Into<String>, bytes: impl Into<Vec<u8>>) {
        let path = path.into();
        self.appends.insert(path.clone());
        self.files.insert(path, bytes.into());
    }

    pub fn write_to(&self, root: &Path, overwrite: bool) -> Result<()> {
        for (rel, bytes) in &self.files {
            let p = root.join(rel);
            if self.appends.contains(rel) && p.exists() {
                append_new_lines(&p, bytes)?;
                continue;
            }
            if p.exists() && !overwrite {
                bail!("{} exists (pass --force to overwrite)", p.display());
            }
            crate::ledger::write_atomic(&p, bytes)?;
        }
        Ok(())
    }

    /// Write without fsync, for scratch copies that need no durability.
    pub fn write_scratch(&self, root: &Path) -> Result<()> {
        for (rel, bytes) in &self.files {
            let p = root.join(rel);
            if let Some(d) = p.parent() {
                std::fs::create_dir_all(d)?;
            }
            std::fs::write(&p, bytes)?;
        }
        Ok(())
    }
}

/// Append the lines of `bytes` that `path` does not already hold, then fsync.
fn append_new_lines(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    let existing = std::fs::read(path)?;
    let have: std::collections::HashSet<&[u8]> = existing.split(|b| *b == b'\n').collect();
    let mut add = Vec::new();
    if !existing.is_empty() && !existing.ends_with(b"\n") {
        add.push(b'\n');
    }
    for line in bytes.split(|b| *b == b'\n').filter(|l| !l.is_empty()) {
        if !have.contains(line) {
            add.extend_from_slice(line);
            add.push(b'\n');
        }
    }
    if add.iter().any(|b| *b != b'\n') {
        let mut f = std::fs::OpenOptions::new().append(true).open(path)?;
        f.write_all(&add)?;
        f.sync_data()?;
    }
    Ok(())
}

/// A named loss (SPEC §7.2). Counts aggregate by `code`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
pub struct Loss {
    pub code: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<Kind>,
    pub count: u64,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub detail: String,
}

#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Losses(pub Vec<Loss>);

impl Losses {
    pub fn add(&mut self, code: &str, kind: Option<Kind>, detail: &str) {
        self.add_n(code, kind, 1, detail);
    }
    pub fn add_n(&mut self, code: &str, kind: Option<Kind>, n: u64, detail: &str) {
        if n == 0 {
            return;
        }
        if let Some(l) = self.0.iter_mut().find(|l| l.code == code && l.kind == kind) {
            l.count += n;
            return;
        }
        self.0.push(Loss { code: code.to_string(), kind, count: n, detail: detail.to_string() });
    }
    pub fn extend(&mut self, other: Losses) {
        for l in other.0 {
            self.add_n(&l.code, l.kind, l.count, &l.detail);
        }
    }
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
    pub fn total(&self) -> u64 {
        self.0.iter().map(|l| l.count).sum()
    }
    pub fn sorted(mut self) -> Self {
        self.0.sort();
        self
    }
}

pub struct Imported {
    pub set: LedgerSet,
    pub losses: Losses,
    pub variant: String,
}

pub struct Exported {
    pub output: Output,
    pub losses: Losses,
    pub variant: String,
}

/// One growing file followed by sync (SPEC §9.3): what a format keeps between
/// reads of it, made by [`Format::follow`].
// `len` is the byte offset read so far, not a collection's size: there is no
// meaningful `is_empty`.
#[allow(clippy::len_without_is_empty)]
pub trait Follow: Send {
    /// Read `appended`, the bytes written to the file after everything read so
    /// far. A final line with no newline yet is held, not parsed, until the
    /// rest of it arrives. Returns what a whole re-import of the file's
    /// complete lines would now hold beyond what this follow has yielded: per
    /// agent, its header and its new events (each with the `seq` the whole
    /// import gives it), and every agent new to the import with all its events.
    /// `None` when the new lines change what earlier lines made, an agent's
    /// header, or the import's variant: only a whole re-import can then say
    /// what the file holds, and the follow is spent.
    fn feed(&mut self, appended: &[u8]) -> Result<Option<LedgerSet>>;
    /// How many bytes of the file this follow has read (a held partial line included).
    fn len(&self) -> u64;
}

pub trait Format: Sync {
    /// The format's id, as used on the command line (`claude-code`).
    fn name(&self) -> &'static str;
    fn describe(&self) -> &'static str;
    fn variants(&self) -> &'static [Variant];
    /// Does `input` (a file or directory) look like this format?
    fn detect(&self, input: &Path) -> Option<Detection>;
    fn import(&self, input: &Path, opts: &ImportOptions) -> Result<Imported>;
    fn export(&self, set: &LedgerSet, opts: &ExportOptions) -> Result<Exported>;
    /// Import the single file `input` and keep what reading on from its end
    /// needs: the import, exactly as [`Format::import`] makes it from the
    /// file's complete lines, and the [`Follow`] that reads what is appended.
    /// `None` (the default) when the format cannot follow `input`: sync then
    /// re-imports the location whole whenever it changes.
    fn follow(&self, _input: &Path, _opts: &ImportOptions) -> Option<Result<(Imported, Box<dyn Follow>)>> {
        None
    }
    /// Weight of this format in the cross-format matrix (primary harnesses are 3).
    fn weight(&self) -> u8 {
        1
    }
}

/// Every format, each behind the export entry point ([`export`]): what
/// [`all`], [`by_name`] and [`detect`] hand out exports the projected view
/// (SPEC §11.1), whoever calls it. An adapter's own `export` (the inner
/// format, reached only by naming its type) renders whatever ledger it is
/// given and carries no filter of its own.
pub fn all() -> Vec<&'static dyn Format> {
    static ALL: [&(dyn Format + Sync); 17] = [
        &Projected(claude_code::ClaudeCode),
        &Projected(codex::Codex),
        &Projected(opencode::Opencode),
        &Projected(gemini_cli::GeminiCli),
        &Projected(cursor::Cursor),
        &Projected(openhands::OpenHands),
        &Projected(pi::Pi),
        &Projected(letta_code::LettaCode),
        &Projected(copilot_cli::CopilotCli),
        &Projected(droid::Droid),
        &Projected(hermes::Hermes),
        &Projected(deepagents::DeepAgents),
        &Projected(omp::Omp),
        &Projected(openclaw::OpenClaw),
        &Projected(atif::Atif),
        &Projected(slack::Slack),
        &Projected(protoledger::Protoledger),
    ];
    ALL.iter().map(|f| *f as &'static dyn Format).collect()
}

/// The ledger set an export of `set` as `format` renders (SPEC §11.1): each
/// ledger's [`Ledger::projected`](crate::ledger::Ledger::projected) form,
/// keeping the §11 facts only for the `protoledger` format, whose own records
/// they are. Borrowed, unchanged, when no ledger leaves anything out.
pub fn projected<'a>(set: &'a LedgerSet, format: &str) -> std::borrow::Cow<'a, LedgerSet> {
    let keep_facts = format == "protoledger";
    let mut changed: Vec<crate::ledger::Ledger> = set.iter().filter_map(|l| l.projected(keep_facts)).collect();
    if changed.is_empty() {
        return std::borrow::Cow::Borrowed(set);
    }
    let mut out = set.clone();
    for l in changed.drain(..) {
        out.insert(l);
    }
    std::borrow::Cow::Owned(out)
}

/// Does `l` hold no conversation? True when it holds at least one event and
/// every event is a `native` record or an `app` record. One event of any other
/// kind (a `text`, a `settings`, a `title`, …, or of a kind this version does
/// not know) makes it a conversation, and so does holding no event at all.
pub fn holds_no_conversation(l: &crate::ledger::Ledger) -> bool {
    !l.events.is_empty() && l.events.iter().all(|e| matches!(e.kind, Kind::Native | Kind::App) && e.unknown_kind.is_none())
}

/// Is `l` a store container in `set`: a ledger that holds no conversation
/// ([`holds_no_conversation`]) and that another ledger of `set` names with a
/// `member-of` link, such as the ledger an `opencode.db` keeps its
/// session-less rows in, or a Slack export's `slack:workspace`?
pub fn is_store_container(l: &crate::ledger::Ledger, set: &LedgerSet) -> bool {
    holds_no_conversation(l) && set.linked_from(l.id(), "member-of").next().is_some()
}

/// Is `format` the ledger's own format, the one that writes its records back
/// where they came from: the format it was imported from, or the format of any
/// of its native records? `protoledger` is every ledger's own format.
fn own_format(l: &crate::ledger::Ledger, format: &str) -> bool {
    format == "protoledger"
        || l.agent().source.as_ref().is_some_and(|s| s.format == format)
        || l.events.iter().any(|e| e.native.as_ref().is_some_and(|n| n.format == format))
}

/// Export loss: a store container ([`is_store_container`]) left out of an
/// export ([`leaves_out`]).
pub(crate) const CONTAINER: &str = "container-not-rendered";

/// Does an export of `set` as `format` leave `l` out? Only a store container
/// ([`is_store_container`]), only in an export without carry, and never from
/// its own format ([`own_format`]), which writes its records back. With carry
/// the target's rendering of it is what carries its records back to its own
/// format exactly, so it is rendered as before.
pub fn leaves_out(l: &crate::ledger::Ledger, set: &LedgerSet, format: &str, opts: &ExportOptions) -> bool {
    !opts.carry && is_store_container(l, set) && !own_format(l, format)
}

/// **The** export entry point (SPEC §11.1): `f` renders the projected view
/// of `set`, never the ledger itself, so a redacted event (and its native
/// record) is in no export, and every §11 fact is left out of every format
/// but `protoledger`'s. A store container is not rendered as an empty
/// conversation: an export without carry leaves it out ([`leaves_out`]),
/// named `container-not-rendered` with the number of its records. Every
/// format [`all`] returns exports through here.
pub fn export(f: &dyn Format, set: &LedgerSet, opts: &ExportOptions) -> Result<Exported> {
    let name = f.name();
    let view = projected(set, name);
    let left_out: Vec<String> = view.iter().filter(|l| leaves_out(l, &view, name, opts)).map(|l| l.id().to_string()).collect();
    if left_out.is_empty() {
        return f.export(&view, opts);
    }
    let mut rest = view.into_owned();
    let mut records = 0u64;
    for id in &left_out {
        records += rest.ledgers.remove(id).map_or(0, |l| l.events.len() as u64);
    }
    let mut ex = f.export(&rest, opts)?;
    let detail = format!("a store container holds no conversation, and {name} does not write its records");
    ex.losses.add_n(CONTAINER, None, records, &detail);
    Ok(ex)
}

/// A format whose `export` is the entry point [`export`]; everything else is
/// the inner format's own.
struct Projected<F>(F);

impl<F: Format> Format for Projected<F> {
    fn name(&self) -> &'static str {
        self.0.name()
    }
    fn describe(&self) -> &'static str {
        self.0.describe()
    }
    fn variants(&self) -> &'static [Variant] {
        self.0.variants()
    }
    fn detect(&self, input: &Path) -> Option<Detection> {
        self.0.detect(input)
    }
    fn import(&self, input: &Path, opts: &ImportOptions) -> Result<Imported> {
        self.0.import(input, opts)
    }
    fn export(&self, set: &LedgerSet, opts: &ExportOptions) -> Result<Exported> {
        export(&self.0, set, opts)
    }
    fn follow(&self, input: &Path, opts: &ImportOptions) -> Option<Result<(Imported, Box<dyn Follow>)>> {
        self.0.follow(input, opts)
    }
    fn weight(&self) -> u8 {
        self.0.weight()
    }
}

pub fn by_name(name: &str) -> Option<&'static dyn Format> {
    all().into_iter().find(|f| f.name() == name)
}

/// The best detection across all formats, if any.
pub fn detect(input: &Path) -> Option<(&'static dyn Format, Detection)> {
    all().into_iter().filter_map(|f| f.detect(input).map(|d| (f, d))).max_by_key(|(f, d)| (d.confidence, f.weight()))
}

/// Export loss: events on an abandoned branch (SPEC §2.3) are not rendered.
pub(crate) const ABANDONED: &str = "abandoned-branch-not-rendered";

/// For an exporter whose target cannot express a branch (SPEC §2.3): which of
/// `l`'s events are on its active path, by index, naming the rest
/// `abandoned-branch-not-rendered`. The exporter renders the active path, and
/// carries the abandoned events as it carries any event it has no record for.
/// An event `own` says is the target's own native record keeps its place: the
/// target wrote that branch itself and reads it back.
pub(crate) fn active_path_only(l: &crate::ledger::Ledger, losses: &mut Losses, carried: &str, own: impl Fn(&Event) -> bool) -> Vec<bool> {
    let on = l.on_path(|e| !own(e));
    let detail = format!("events on an abandoned branch (SPEC §2.3) are not part of the conversation; {carried}");
    losses.add_n(ABANDONED, None, on.iter().filter(|o| !**o).count() as u64, &detail);
    on
}

/// Refuse a database whose schema text is not UTF-8 (a damaged file):
/// rusqlite panics on a column name that is not, and every column name a
/// query returns comes from our own SQL or from this schema.
#[cfg(feature = "sqlite")]
pub(crate) fn sqlite_schema_is_utf8(c: &rusqlite::Connection) -> Result<()> {
    let mut st = c.prepare("SELECT name, tbl_name, sql FROM sqlite_master")?;
    let mut q = st.query([])?;
    while let Some(r) = q.next()? {
        for i in 0..3 {
            if let rusqlite::types::ValueRef::Text(b) = r.get_ref(i)?
                && std::str::from_utf8(b).is_err()
            {
                bail!("the database schema holds text that is not UTF-8 (a damaged file)");
            }
        }
    }
    Ok(())
}

/// Placeholder for a format whose adapter is not written yet.
#[macro_export]
macro_rules! unimplemented_format {
    ($ty:ident, $name:literal, $describe:literal) => {
        pub struct $ty;
        impl $crate::formats::Format for $ty {
            fn name(&self) -> &'static str {
                $name
            }
            fn describe(&self) -> &'static str {
                $describe
            }
            fn variants(&self) -> &'static [$crate::formats::Variant] {
                &[]
            }
            fn detect(&self, _input: &std::path::Path) -> Option<$crate::formats::Detection> {
                None
            }
            fn import(&self, _input: &std::path::Path, _opts: &$crate::formats::ImportOptions) -> anyhow::Result<$crate::formats::Imported> {
                anyhow::bail!(concat!($name, ": import is not implemented yet"))
            }
            fn export(&self, _set: &$crate::ledger::LedgerSet, _opts: &$crate::formats::ExportOptions) -> anyhow::Result<$crate::formats::Exported> {
                anyhow::bail!(concat!($name, ": export is not implemented yet"))
            }
        }
    };
}

#[cfg(test)]
mod branch_tests {
    use super::*;

    /// Exporters whose target cannot express a branch: they write the active
    /// path and name `abandoned-branch-not-rendered` (SPEC §2.3).
    const ACTIVE_PATH: [&str; 11] = ["atif", "copilot-cli", "cursor", "deepagents", "gemini-cli", "hermes", "letta-code", "omp", "openhands", "pi", "slack"];
    /// Exporters that write a branch their target reads back as one (`after`).
    const KEEP_BRANCH: [&str; 2] = ["droid", "openclaw"];
    /// Primaries that write the active path and keep the branch only in a carrier.
    const PRIMARY_ACTIVE: [&str; 3] = ["claude-code", "codex", "opencode"];

    fn said(set: &LedgerSet) -> Vec<String> {
        set.iter().flat_map(|l| l.events.iter()).filter_map(|e| e.str("text").map(str::to_string)).collect()
    }

    /// Every exporter is in exactly one population, and each
    /// population does what it says with `protoledger/branch-rewind`, carry off:
    /// the active-path writers lose the abandoned branch, named; the others
    /// keep it or name it.
    #[test]
    fn every_exporter_writes_a_rewound_ledger_as_its_population_says() {
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/protoledger/branch-rewind/native");
        let src = by_name("protoledger").unwrap().import(&fixture, &ImportOptions::standard()).unwrap();
        let abandoned = "Check the syllables with the counter tool.";
        assert!(said(&src.set).iter().any(|t| t == abandoned), "the fixture is not the one this test is about");
        let mut seen = Vec::new();
        for f in all().into_iter().filter(|f| f.name() != "protoledger" && f.variants().iter().any(|v| v.import && v.export)) {
            let n = f.name();
            seen.push(n);
            // Without `sqlite` these two write their SQLite store for a foreign
            // ledger, so they refuse it, named: the degraded regime and who is in it.
            if !cfg!(feature = "sqlite") && ["deepagents", "hermes"].contains(&n) {
                let e = f.export(&src.set, &ExportOptions::no_carry()).err().unwrap_or_else(|| panic!("{n} exported without sqlite"));
                assert!(format!("{e:#}").contains("`sqlite` feature"), "{n}: {e:#}");
                continue;
            }
            let ex = f.export(&src.set, &ExportOptions::no_carry()).unwrap_or_else(|e| panic!("{n}: {e:#}"));
            let named = ex.losses.0.iter().find(|l| l.code == ABANDONED).map(|l| l.count);
            let dir = tempfile::tempdir().unwrap();
            let path = crate::roundtrip::materialise(&ex.output, dir.path()).unwrap();
            let back = f.import(&path, &ImportOptions { variant: None, restore_carried: false }).unwrap_or_else(|e| panic!("{n}: {e:#}"));
            let kept = said(&back.set).iter().any(|t| t == abandoned);
            if ACTIVE_PATH.contains(&n) || PRIMARY_ACTIVE.contains(&n) {
                assert_eq!((named, kept), (Some(4), false), "{n}: the abandoned branch was written as the conversation");
            } else if KEEP_BRANCH.contains(&n) {
                let branched = back.set.iter().any(|l| l.is_branched());
                assert!(named.is_none() && kept && branched, "{n}: the branch did not come back as a branch ({named:?}, kept={kept}, branched={branched})");
            } else {
                panic!("{n} is in no population: classify it");
            }
        }
        let all_named: Vec<&str> = ACTIVE_PATH.iter().chain(&KEEP_BRANCH).chain(&PRIMARY_ACTIVE).copied().collect();
        assert!(all_named.iter().all(|n| seen.contains(n)), "a population names a format that was not exported: {seen:?}");
    }
}
