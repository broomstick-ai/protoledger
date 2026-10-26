//! `protoledger`: import, export and convert agent threads.

use anyhow::{Context, Result, anyhow, bail};
use clap::{Parser, Subcommand};
use protoledger::LedgerSet;
use protoledger::formats::{self, ExportOptions, Format, ImportOptions};
use protoledger::roundtrip;
use std::path::{Path, PathBuf};

#[derive(Parser)]
#[command(name = "protoledger", version, about = "Import, export and convert coding-agent threads through the protoledger format.")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// List supported formats and their variants.
    Formats,
    /// Say which format and variant a file or directory is.
    Detect { input: PathBuf },
    /// Native → protoledger. Writes one <agent>.jsonl per agent into OUT.
    Import {
        /// Source format, or `auto`.
        format: String,
        input: PathBuf,
        #[arg(short, long)]
        out: PathBuf,
        /// Force a source variant instead of detecting it.
        #[arg(long)]
        variant: Option<String>,
        /// Do not restore carried protoledger events.
        #[arg(long)]
        no_restore: bool,
        #[arg(long)]
        force: bool,
    },
    /// protoledger → native. INPUT is a ledger file or a directory of them.
    Export {
        format: String,
        input: PathBuf,
        #[arg(short, long)]
        out: PathBuf,
        #[arg(long)]
        variant: Option<String>,
        /// Do not write protoledger carrier fields into native records.
        #[arg(long)]
        no_carry: bool,
        /// Producer version to write where the format records one.
        #[arg(long)]
        target_version: Option<String>,
        #[arg(long)]
        force: bool,
    },
    /// Native → native, through protoledger.
    Convert {
        /// Source format, or `auto`.
        from: String,
        to: String,
        input: PathBuf,
        #[arg(short, long)]
        out: PathBuf,
        #[arg(long)]
        no_carry: bool,
        #[arg(long)]
        force: bool,
    },
    /// Check a ledger set against the spec.
    Validate { input: PathBuf },
    /// Run a round trip and print what it lost. With --via, X → P → Y → P → X.
    Roundtrip {
        /// Source format, or `auto`.
        format: String,
        input: PathBuf,
        #[arg(long)]
        via: Option<String>,
        #[arg(long)]
        no_carry: bool,
        /// Print the full JSON report.
        #[arg(long)]
        json: bool,
    },
    /// A store directory: ledgers plus a rebuildable index (SPEC §9).
    #[cfg(feature = "store")]
    Store {
        #[command(subcommand)]
        cmd: StoreCmd,
    },
    /// Write an agent's ledger in a store to stdout exactly: its JSONL, the
    /// decompressed stream (SPEC §10.1).
    #[cfg(feature = "store")]
    Cat { store: PathBuf, agent: String },
    /// Export an agent (and its subagents) from a store into a harness's
    /// directory and bind it: what the harness appends there comes back (SPEC §9.2).
    #[cfg(feature = "sync")]
    Project {
        store: PathBuf,
        agent: String,
        format: String,
        /// The directory to write into (e.g. ~/.claude/projects/<slug>).
        dir: PathBuf,
    },
    /// Import a native session into a store and bind it, so what is appended
    /// there keeps coming in. Only the location named is read.
    #[cfg(feature = "sync")]
    Attach {
        store: PathBuf,
        /// Source format, or `auto`.
        format: String,
        location: PathBuf,
    },
    /// Stop following a binding.
    #[cfg(feature = "sync")]
    Unbind { store: PathBuf, binding: String },
    /// Write a projection binding's location again from the projected view
    /// (SPEC §11.2), between harness runs: sync it first, write the render
    /// to a temporary file, append protoledger/projected, rename it into
    /// place. Refuses a binding that is not active, an attachment, a location
    /// that is not a single append-only transcript (Claude Code, Codex), and
    /// a location found diverged. Prints the binding's projected fact (JSON).
    #[cfg(feature = "sync")]
    Reproject { store: PathBuf, binding: String },
    /// Follow every active binding of a store in the foreground, printing each
    /// appended event as a JSON line.
    #[cfg(feature = "sync")]
    Watch {
        store: PathBuf,
        /// Quiet period after a change before the location is read.
        #[arg(long, default_value_t = 150)]
        debounce_ms: u64,
        /// How often every location is checked for changes regardless of the watcher.
        #[arg(long, default_value_t = 1000)]
        poll_ms: u64,
        /// Do not use the filesystem watcher; poll only.
        #[arg(long)]
        no_watcher: bool,
    },
    /// Serve a store: every append streamed over a WebSocket, queries and
    /// project/attach/unbind over HTTP, and every active binding followed
    /// (docs/SERVE.md). Prints `listening on <addr>` once it accepts connections.
    #[cfg(feature = "serve")]
    Serve {
        #[arg(long)]
        store: PathBuf,
        /// Address to listen on (ip:port, [ipv6]:port or host:port); port 0
        /// picks a free one. Loopback only unless --allow-remote.
        #[arg(long, default_value = "127.0.0.1:7417")]
        listen: String,
        /// Listen on an address that is not loopback. serve has no
        /// authentication: anyone who can reach the port can read and write.
        #[arg(long)]
        allow_remote: bool,
        /// Quiet period after a change before a bound location is read.
        #[arg(long, default_value_t = 150)]
        debounce_ms: u64,
        /// How often every bound location is checked regardless of the watcher.
        #[arg(long, default_value_t = 1000)]
        poll_ms: u64,
        /// Do not use the filesystem watcher; poll only.
        #[arg(long)]
        no_watcher: bool,
    },
}

#[cfg(feature = "serve")]
fn serve_cmd(store: PathBuf, listen: &str, allow_remote: bool, debounce_ms: u64, poll_ms: u64, no_watcher: bool) -> Result<()> {
    use protoledger::serve::{ServeOptions, Server, resolve_listen};
    use protoledger::sync::FollowOptions;
    use std::io::Write;
    use std::time::Duration;
    let follow = FollowOptions { debounce: Duration::from_millis(debounce_ms), poll: Duration::from_millis(poll_ms), watcher: !no_watcher };
    report_damaged(&store);
    let server = Server::start(&store, resolve_listen(listen, allow_remote)?, ServeOptions { follow, allow_remote })?;
    let mut out = std::io::stdout().lock();
    writeln!(out, "listening on {}", server.addr())?;
    out.flush()?;
    drop(out);
    server.wait()
}

/// Open the store at `root`, and say on stderr which ledgers it could not
/// read: each is skipped, and every operation on it fails naming it.
/// Say on stderr which ledgers of the store at `root` cannot be read, for a
/// command that does not open it as [`open_store`] does: found without
/// writing anything. A store that cannot be read at all is the command's
/// own error to report.
#[cfg(feature = "store")]
fn report_damaged(root: &std::path::Path) {
    if let Ok(ds) = protoledger::Store::damaged_at(root) {
        for d in ds {
            eprintln!("protoledger: {d}");
        }
    }
}

#[cfg(feature = "store")]
fn open_store(root: &std::path::Path) -> Result<protoledger::Store> {
    let s = protoledger::Store::open(root)?;
    for d in s.damaged() {
        eprintln!("protoledger: {d}");
    }
    Ok(s)
}

#[cfg(feature = "sync")]
fn print_synced(trigger: &str, s: &protoledger::sync::Synced) -> Result<()> {
    use std::io::Write;
    let mut out = std::io::stdout().lock();
    for p in &s.appended {
        let line = serde_json::json!({
            "binding": s.binding, "trigger": trigger, "agent": p.agent, "seq": p.seq, "event": p.event,
        });
        writeln!(out, "{line}")?;
    }
    out.flush()?;
    for a in &s.created {
        eprintln!("binding {}: new agent {a}", s.binding);
    }
    if let Some(d) = &s.diverged {
        eprintln!("binding {} diverged and is no longer followed: {d}", s.binding);
    }
    if let Some(d) = &s.deferred {
        eprintln!("binding {} not read: {d}", s.binding);
    }
    Ok(())
}

#[cfg(feature = "sync")]
fn sync_cmd(cmd: Cmd) -> Result<()> {
    use protoledger::sync::{self, FollowOptions, Follower, Trigger};
    use std::time::{Duration, Instant};
    match cmd {
        Cmd::Project { store, agent, format, dir } => {
            let mut s = open_store(&store)?;
            let b = sync::project(&mut s, &agent, &format, &dir)?;
            eprintln!("projected {agent} as {} {} → {}", b.format, b.variant.as_deref().unwrap_or(""), b.location);
            println!("{}", b.binding);
        }
        Cmd::Attach { store, format, location } => {
            let mut s = open_store(&store)?;
            let (b, r) = sync::attach(&mut s, &format, &location)?;
            eprintln!("attached {} {} as agent {}: {} agents created, {} events appended", b.format, b.location, b.agent, r.created.len(), r.appended.len());
            println!("{}", b.binding);
        }
        Cmd::Unbind { store, binding } => {
            let mut s = open_store(&store)?;
            sync::unbind(&mut s, &binding)?;
            eprintln!("unbound {binding}");
        }
        Cmd::Reproject { store, binding } => {
            let mut s = open_store(&store)?;
            let r = sync::reproject(&mut s, &binding)?;
            let fact = match r.fact {
                Some(at) => format!("protoledger/projected appended at seq {}", at.seq),
                None => "no protoledger/projected fact owed (the view is unchanged)".to_string(),
            };
            eprintln!(
                "reprojected {binding}: its sync appended {} events; {} files written; {fact}; {} bytes",
                r.synced.appended.len(),
                r.written.len(),
                r.projected.bytes
            );
            println!("{}", serde_json::to_string(&r.projected)?);
        }
        Cmd::Watch { store, debounce_ms, poll_ms, no_watcher } => {
            let mut s = open_store(&store)?;
            // Catch up with whatever happened while nobody was watching.
            for r in sync::sync_all(&mut s)? {
                print_synced("start", &r)?;
            }
            let opts = FollowOptions { debounce: Duration::from_millis(debounce_ms), poll: Duration::from_millis(poll_ms), watcher: !no_watcher };
            let mut f = Follower::new(&s, opts)?;
            let n = s.bindings()?.into_iter().filter(|b| b.active).count();
            eprintln!("watching {n} bindings in {}", store.display());
            loop {
                for (t, r) in f.step(&mut s, Instant::now() + Duration::from_secs(3600))? {
                    print_synced(if t == Trigger::Watcher { "watcher" } else { "poll" }, &r)?;
                }
            }
        }
        _ => unreachable!("not a sync command"),
    }
    Ok(())
}

#[cfg(feature = "store")]
#[derive(Subcommand)]
enum StoreCmd {
    /// Create a store (ledgers/ and index.db) at STORE, or bring an existing one's index up to date.
    Init { store: PathBuf },
    /// List the store's agents.
    Ls { store: PathBuf },
    /// Print an agent's events as JSONL, read from its ledger through the index.
    Show {
        store: PathBuf,
        agent: String,
        /// First seq to print.
        #[arg(long, default_value_t = 1)]
        from: u64,
        #[arg(long)]
        limit: Option<usize>,
    },
    /// Rebuild index.db from the ledgers alone.
    Reindex { store: PathBuf },
    /// Show or change the store's configuration, store.json (SPEC §10.10),
    /// and print it. `--compression on` records the setting and migrates
    /// every plain ledger to the compressed form (`off`: every compressed
    /// ledger to the plain form), each checked byte for byte before its old
    /// form is deleted; a ledger another writer holds is skipped and named,
    /// and running the command again migrates it.
    Config {
        store: PathBuf,
        /// Keep new ledgers compressed, and migrate every ledger: on or off.
        #[arg(long, value_parser = ["on", "off"])]
        compression: Option<String>,
        /// The hot tail's size, in bytes, at which a writer seals it into a block.
        #[arg(long)]
        seal_bytes: Option<u64>,
        /// The default dictionary for new blocks: an id from `store dict ls`,
        /// or `none`. Existing blocks are not touched (see `store recompress`).
        #[arg(long)]
        dictionary: Option<String>,
    },
    /// Compression dictionaries (SPEC §10.7): add, train, list, remove.
    Dict {
        #[command(subcommand)]
        cmd: DictCmd,
    },
    /// Rewrite compressed ledgers (every compressed ledger when none is
    /// named) with a dictionary (SPEC §10.9): the store's default, or
    /// `--dictionary <id>|none`. Each new block file is read back and checked
    /// byte for byte against the ledger before it replaces the old one; a
    /// plain ledger, or one another writer holds, is skipped and named.
    Recompress {
        store: PathBuf,
        /// A dictionary id, or `none` for no dictionary. Default: the store's.
        #[arg(long)]
        dictionary: Option<String>,
        /// The agents to recompress.
        agents: Vec<String>,
    },
    /// Import a native thread (or ledgers, format `protoledger`) into the store.
    Import {
        store: PathBuf,
        /// Source format, or `auto`.
        format: String,
        input: PathBuf,
    },
    /// Redact events of an agent (SPEC §11.1): append one protoledger/redacted
    /// fact naming them. The ledger keeps every event, unchanged; every
    /// projection and export leaves the redacted ones out. Refused, appending
    /// nothing, when no event is named, or one is not in the ledger, or is a
    /// redaction or unredaction fact.
    Redact {
        store: PathBuf,
        agent: String,
        /// Ids of the events to redact.
        events: Vec<String>,
        /// Why (recorded in the fact).
        #[arg(long)]
        reason: String,
    },
    /// Withdraw redactions (SPEC §11.1): append one protoledger/unredacted
    /// fact naming the events. Refused when no event is named or one is not
    /// in the ledger.
    Unredact {
        store: PathBuf,
        agent: String,
        /// Ids of the events to unredact.
        events: Vec<String>,
    },
    /// Remove whole ledgers chosen by an explicit policy (SPEC §9.5), with
    /// their index rows and bindings. Policy flags combine with AND; at least
    /// one is required. A subagent goes with its parent. Prints what would be
    /// removed and changes nothing unless --yes is given. Never edits a ledger.
    Gc {
        store: PathBuf,
        /// Older than this: 30d, 12h, 90m, 45s, 2w. Aged by its newest event's
        /// ts, else its header's created_at, else its file's mtime (the plan's
        /// `clock` lines name which, per ledger).
        #[arg(long)]
        older_than: Option<String>,
        /// Ledger larger than this, measured on its decompressed stream (a
        /// compressed ledger's size is its plain form's): 1500, 10K (KiB), 10KB, 5M, 1G.
        #[arg(long)]
        larger_than: Option<String>,
        /// Archived: the ledger's newest protoledger/archived|unarchived app event is archived.
        #[arg(long)]
        archived: bool,
        /// Also delete the native file each removed projection binding names.
        #[arg(long)]
        remove_projections: bool,
        /// Act. Without it, a dry run.
        #[arg(long)]
        yes: bool,
    },
}

#[cfg(feature = "store")]
#[derive(Subcommand)]
enum DictCmd {
    /// Add a dictionary from a file: any file zstd accepts as one (a trained
    /// dictionary, or raw content). Prints its id. Adding it again changes
    /// nothing; it becomes the default only through `store config --dictionary`.
    Add { store: PathBuf, file: PathBuf },
    /// Train a dictionary from the store's own ledgers and add it. Prints its id.
    Train {
        store: PathBuf,
        /// The dictionary's size in bytes.
        #[arg(long, default_value_t = protoledger::store::DEFAULT_TRAIN_SIZE)]
        size: usize,
    },
    /// List every dictionary: id, size, whether it is the default, how many
    /// sealed blocks it encodes, then each ledger using it.
    Ls { store: PathBuf },
    /// Delete a dictionary. Refused while it is the default or any block in
    /// the store uses it.
    Rm { store: PathBuf, id: String },
}

#[cfg(feature = "store")]
fn dict_cmd(cmd: DictCmd) -> Result<()> {
    use protoledger::Store;
    match &cmd {
        DictCmd::Add { store, .. } | DictCmd::Train { store, .. } | DictCmd::Rm { store, .. } => report_damaged(store),
        DictCmd::Ls { .. } => {} // opens the store, which says
    }
    match cmd {
        DictCmd::Add { store, file } => {
            let bytes = std::fs::read(&file).with_context(|| format!("reading {}", file.display()))?;
            println!("{}", Store::add_dictionary(&store, &bytes)?);
        }
        DictCmd::Train { store, size } => println!("{}", Store::train_dictionary(&store, size)?),
        DictCmd::Ls { store } => {
            for d in open_store(&store)?.dictionaries()? {
                println!("{}\tsize={}\tdefault={}\tblocks={}", d.id, d.size, if d.default { "yes" } else { "no" }, d.blocks());
                for (agent, n) in &d.ledgers {
                    println!("\t{agent}\tblocks={n}");
                }
            }
        }
        DictCmd::Rm { store, id } => {
            Store::remove_dictionary(&store, &id)?;
            eprintln!("removed dictionary {id}");
        }
    }
    Ok(())
}

#[cfg(feature = "store")]
fn store_cmd(cmd: StoreCmd) -> Result<()> {
    use protoledger::Store;
    match cmd {
        StoreCmd::Init { store } => {
            let s = open_store(&store)?;
            eprintln!("{}: {} agents", store.display(), s.agents()?.len());
        }
        StoreCmd::Ls { store } => {
            let s = open_store(&store)?;
            for a in s.agents()? {
                // A damaged ledger's row is what the index read before the
                // damage; say so beside it.
                if s.damaged_agent(&a.id).is_some() {
                    println!("{}\tdamaged (the index row predates it; every operation on it is refused)", a.id);
                    continue;
                }
                let parent = a.parent.as_ref().map(|p| format!(" parent={}", p.agent)).unwrap_or_default();
                let source = a.source.as_ref().map(|s| format!(" source={}", s.format)).unwrap_or_default();
                let title = a.title.as_deref().map(|t| format!(" title={t:?}")).unwrap_or_default();
                println!("{}\tevents={}{parent}{source}{title}", a.id, a.last_seq);
            }
        }
        StoreCmd::Show { store, agent, from, limit } => {
            let s = open_store(&store)?;
            if s.agent(&agent)?.is_none() {
                bail!("no agent {agent:?} in {}", store.display());
            }
            for row in s.events(&agent, from, limit.unwrap_or(usize::MAX))? {
                let e = s.event_at(&agent, row.seq)?.ok_or_else(|| anyhow!("{agent} seq {} vanished", row.seq))?;
                println!("{}", serde_json::to_string(&e)?);
            }
        }
        StoreCmd::Reindex { store } => {
            let mut s = open_store(&store)?;
            let r = s.reindex()?;
            eprintln!("{}: reindexed {} agents, {} events", store.display(), r.agents, r.events);
        }
        StoreCmd::Config { store, compression, seal_bytes, dictionary } => {
            let mut s = open_store(&store)?;
            if let Some(n) = seal_bytes {
                s.set_seal_bytes(n)?;
            }
            if let Some(d) = dictionary {
                s.set_dictionary(if d == "none" { None } else { Some(d.as_str()) })?;
            }
            let mut failed = 0;
            if let Some(c) = compression {
                let m = s.set_compression(c == "on")?;
                for a in &m.migrated {
                    eprintln!("migrated\t{a}");
                }
                for (a, why) in &m.skipped {
                    eprintln!("skipped\t{a}\t{why}");
                }
                for (a, why) in &m.failed {
                    eprintln!("failed\t{a}\t{why}");
                }
                eprintln!("compression {c}: {} migrated, {} skipped, {} failed", m.migrated.len(), m.skipped.len(), m.failed.len());
                failed = m.failed.len();
            }
            println!("{}", serde_json::to_string(s.config())?);
            if failed > 0 {
                bail!("{failed} ledgers were not migrated; each was left as it was");
            }
        }
        StoreCmd::Dict { cmd } => dict_cmd(cmd)?,
        StoreCmd::Recompress { store, dictionary, agents } => {
            use protoledger::store::DictChoice;
            let choice = match dictionary.as_deref() {
                None => DictChoice::Default,
                Some("none") => DictChoice::None,
                Some(id) => DictChoice::Id(id.to_string()),
            };
            let mut s = open_store(&store)?;
            let r = s.recompress(choice, &agents)?;
            for a in &r.recompressed {
                eprintln!("recompressed\t{a}");
            }
            for (a, why) in &r.skipped {
                eprintln!("skipped\t{a}\t{why}");
            }
            for (a, why) in &r.failed {
                eprintln!("failed\t{a}\t{why}");
            }
            eprintln!("recompress: {} recompressed, {} skipped, {} failed", r.recompressed.len(), r.skipped.len(), r.failed.len());
            if !r.failed.is_empty() {
                bail!("{} ledgers were not recompressed; each was left as it was", r.failed.len());
            }
        }
        StoreCmd::Gc { store, older_than, larger_than, archived, remove_projections, yes } => {
            use protoledger::store::gc::{Policy, parse_duration, parse_size};
            let policy = Policy {
                older_than: older_than.as_deref().map(parse_duration).transpose()?,
                larger_than: larger_than.as_deref().map(parse_size).transpose()?,
                archived,
                remove_projections,
            };
            // Before the store is opened: opening writes to index.db.
            policy.check()?;
            let now = std::time::SystemTime::now();
            let plan = if yes {
                open_store(&store)?.gc(&policy, now)?
            } else {
                let plan = Store::gc_dry_run(&store, &policy, now)?;
                for d in &plan.damaged {
                    eprintln!("protoledger: {d}");
                }
                plan
            };
            print!("{}", plan.render());
        }
        StoreCmd::Redact { store, agent, events, reason } => {
            let mut s = open_store(&store)?;
            let at = s.redact(&agent, &events, &reason)?;
            eprintln!("{agent}: {} events redacted (protoledger/redacted at seq {})", events.len(), at.seq);
        }
        StoreCmd::Unredact { store, agent, events } => {
            let mut s = open_store(&store)?;
            let at = s.unredact(&agent, &events)?;
            eprintln!("{agent}: {} events unredacted (protoledger/unredacted at seq {})", events.len(), at.seq);
        }
        StoreCmd::Import { store, format: name, input } => {
            let mut s = open_store(&store)?;
            let r = s.import(&name, &input)?;
            report_losses("import", &r.losses);
            eprintln!(
                "{} {}: {} agents created, {} extended, {} events appended, {} already held → {}",
                r.format,
                r.variant,
                r.created.len(),
                r.extended.len(),
                r.events_appended,
                r.events_skipped,
                store.display()
            );
        }
    }
    Ok(())
}

fn format(name: &str, input: Option<&Path>) -> Result<&'static dyn Format> {
    if name == "auto" {
        let input = input.ok_or_else(|| anyhow!("`auto` needs an input"))?;
        let (f, d) = formats::detect(input).ok_or_else(|| anyhow!("{}: no format recognised it", input.display()))?;
        eprintln!("detected {} ({}, {}%)", f.name(), d.variant, d.confidence);
        return Ok(f);
    }
    formats::by_name(name).ok_or_else(|| {
        let names: Vec<_> = formats::all().iter().map(|f| f.name()).collect();
        anyhow!("unknown format {name:?}; known: {}", names.join(", "))
    })
}

/// `exact N/T` over every source file, and the altered ones counted apart:
/// a file holding an altered record is neither exact nor lost.
fn files_summary(d: &roundtrip::FileDiff) -> String {
    let total = d.exact.len() + d.differ.len() + d.missing.len() + d.altered.len();
    let mut s = format!("exact {}/{total}", d.exact.len());
    if !d.altered.is_empty() {
        s.push_str(&format!(", altered {}", d.altered.len()));
    }
    s
}

fn report_losses(what: &str, losses: &formats::Losses) {
    for l in &losses.0 {
        let kind = l.kind.map(|k| format!(" [{k}]")).unwrap_or_default();
        eprintln!("{what} loss: {}{kind} ×{} {}", l.code, l.count, l.detail);
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Formats => {
            for f in formats::all() {
                println!("{:<14} {}", f.name(), f.describe());
                if f.variants().is_empty() {
                    println!("    (not implemented)");
                }
                for v in f.variants() {
                    let dir = match (v.import, v.export) {
                        (true, true) => "import+export",
                        (true, false) => "import",
                        (false, true) => "export",
                        _ => "-",
                    };
                    println!("    {:<22} {:<14} {:<24} {}", v.name, dir, v.versions, v.notes);
                }
            }
        }
        Cmd::Detect { input } => match formats::detect(&input) {
            Some((f, d)) => println!("{} {} {}% {}", f.name(), d.variant, d.confidence, d.version.unwrap_or_default()),
            None => bail!("{}: no format recognised it", input.display()),
        },
        Cmd::Import { format: name, input, out, variant, no_restore, force } => {
            let f = format(&name, Some(&input))?;
            let r = f.import(&input, &ImportOptions { variant, restore_carried: !no_restore })?;
            report_losses("import", &r.losses);
            let files = r.set.write_dir(&out, force)?;
            eprintln!("{} {}: {} agents → {}", f.name(), r.variant, files.len(), out.display());
        }
        Cmd::Export { format: name, input, out, variant, no_carry, target_version, force } => {
            let f = format(&name, None)?;
            let set = LedgerSet::read_path(&input)?;
            let r = f.export(&set, &ExportOptions { variant, carry: !no_carry, target_version })?;
            report_losses("export", &r.losses);
            r.output.write_to(&out, force)?;
            eprintln!("{} {}: {} files → {}", f.name(), r.variant, r.output.files.len(), out.display());
        }
        Cmd::Convert { from, to, input, out, no_carry, force } => {
            let src = format(&from, Some(&input))?;
            let dst = format(&to, None)?;
            let i = src.import(&input, &ImportOptions::standard())?;
            report_losses("import", &i.losses);
            let eopts = if no_carry { ExportOptions::no_carry() } else { ExportOptions::standard() };
            let e = dst.export(&i.set, &eopts)?;
            report_losses("export", &e.losses);
            e.output.write_to(&out, force)?;
            eprintln!("{} → {}: {} files → {}", src.name(), dst.name(), e.output.files.len(), out.display());
        }
        Cmd::Validate { input } => {
            let set = LedgerSet::read_path(&input).context("reading ledger set")?;
            let problems = set.validate();
            for p in &problems {
                println!("{p}");
            }
            if !problems.is_empty() {
                bail!("{} problems", problems.len());
            }
            let events: usize = set.iter().map(|l| l.events.len()).sum();
            println!("ok: {} agents, {events} events", set.len());
        }
        Cmd::Roundtrip { format: name, input, via, no_carry, json } => {
            let f = format(&name, Some(&input))?;
            match via {
                None => {
                    let r = roundtrip::self_trip(f, &input)?;
                    if json {
                        println!("{}", serde_json::to_string_pretty(&r)?);
                    } else {
                        println!("{} {}: files {}; re-import {}", f.name(), r.variant, files_summary(&r.files), r.reimport.summary());
                        report_losses("import", &r.import_losses);
                        report_losses("export", &r.export_losses);
                    }
                }
                Some(y) => {
                    let y = format(&y, None)?;
                    let r = roundtrip::cross_trip(f, &input, y, !no_carry)?;
                    if json {
                        println!("{}", serde_json::to_string_pretty(&r)?);
                    } else {
                        println!("{} → {} (carry {}):", f.name(), y.name(), !no_carry);
                        println!("  there: {}", r.there.summary());
                        println!("  back:  files {}; {}", files_summary(&r.back_files), r.back.summary());
                        report_losses("export", &r.export_losses);
                    }
                }
            }
        }
        #[cfg(feature = "store")]
        Cmd::Store { cmd } => store_cmd(cmd)?,
        #[cfg(feature = "store")]
        Cmd::Cat { store, agent } => {
            let s = open_store(&store)?;
            // The reader refuses a damaged ledger, naming it.
            let reader = s.reader(&agent).with_context(|| format!("in {}", store.display()))?;
            let mut out = std::io::stdout().lock();
            reader.copy_to(&mut out)?;
            std::io::Write::flush(&mut out)?;
        }
        #[cfg(feature = "sync")]
        cmd @ (Cmd::Project { .. } | Cmd::Attach { .. } | Cmd::Unbind { .. } | Cmd::Reproject { .. } | Cmd::Watch { .. }) => sync_cmd(cmd)?,
        #[cfg(feature = "serve")]
        Cmd::Serve { store, listen, allow_remote, debounce_ms, poll_ms, no_watcher } => {
            serve_cmd(store, &listen, allow_remote, debounce_ms, poll_ms, no_watcher)?
        }
    }
    Ok(())
}
