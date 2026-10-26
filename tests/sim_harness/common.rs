//! What the simulated-harness tests share: the temporary root and the paths
//! the run used (the isolation check), running a simulated turn while a
//! follower follows, and the checks on what the ledger gained.

use protoledger::Event;
use protoledger::store::Store;
use protoledger::sync::{FollowOptions, Follower, Synced, Trigger};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// The live-sync bound: the turn's last event is in the ledger this long after the turn ends.
pub const BOUND: Duration = Duration::from_secs(2);
/// How long after the turn ends the follower is stepped at most, waiting for
/// the turn's events (well past [`BOUND`], so a late event is seen late, not lost).
pub const GRACE: Duration = Duration::from_secs(10);
/// How long `PL_SIM_FORCE=late-follower` stops the follower for when the turn ends.
pub const LATE: Duration = Duration::from_millis(2500);
/// How long the follower keeps stepping after the last wanted event is in
/// the ledger, so that a divergence or a re-import found by a later sync is
/// seen: one poll period (1 s, the longest period of the
/// default follow options) and the debounce (150 ms), and some.
pub const QUIET: Duration = Duration::from_millis(1300);

/// Where a compressed store's writers seal during a simulated turn: a few
/// event lines, so that a turn seals several blocks while it is synced (the
/// product's default, 256 KiB, would leave the whole turn in the hot tail).
/// The store is seeded sealing on every append ([`crate::modes::SEAL_BYTES`]),
/// so its ledgers hold blocks before the turn; sealing on every append
/// during the turn too puts two fsyncs and a read-back on each event a sync
/// appends, which under load took a turn past the 2 s bound (2041 ms).
pub const SEAL_BYTES: u64 = 4096;

/// Before the turn, in a compressed mode: seal at [`SEAL_BYTES`].
pub fn live_sealing(m: crate::modes::Mode, s: &mut Store) {
    if m.compressed() {
        s.set_seal_bytes(SEAL_BYTES).unwrap();
    }
}

pub fn fixture(p: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures").join(p)
}

/// `PL_SIM_FORCE`: which guard to force red, if any (see the crate docs).
pub fn force() -> Option<String> {
    std::env::var("PL_SIM_FORCE").ok().filter(|s| !s.is_empty())
}

/// The follower live sync is about: the default options (watcher on, poll 1 s,
/// debounce 150 ms), unless `PL_SIM_FORCE=no-follower`.
pub fn follower(s: &Store) -> Option<Follower> {
    if force().as_deref() == Some("no-follower") {
        eprintln!("sim: PL_SIM_FORCE=no-follower: nothing follows the binding");
        return None;
    }
    let o = FollowOptions::default();
    assert_eq!((o.watcher, o.poll, o.debounce), (true, Duration::from_secs(1), Duration::from_millis(150)), "the default follow options");
    Some(Follower::new(s, o).unwrap())
}

/// The real home directory, as the environment names it: only ever compared
/// against, never resolved or read.
fn home() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").expect("HOME is set (it is only compared against)"))
}

fn canon(p: &Path) -> PathBuf {
    let abs = std::path::absolute(p).unwrap();
    // Nothing under the real home directory is touched, not even to resolve it.
    if abs.starts_with(home()) {
        return abs;
    }
    // A path that does not exist yet: its nearest existing ancestor, resolved.
    let mut cur = abs;
    let mut rest = Vec::new();
    loop {
        if let Ok(c) = cur.canonicalize() {
            let mut out = c;
            for r in rest.iter().rev() {
                out.push(r);
            }
            return out;
        }
        match (cur.file_name().map(|n| n.to_os_string()), cur.parent().map(Path::to_path_buf)) {
            (Some(n), Some(parent)) => {
                rest.push(n);
                cur = parent;
            }
            _ => return std::path::absolute(p).unwrap(),
        }
    }
}

/// The temporary root of one test, and every path the run resolved, wrote or
/// read, by role: the population the isolation check names.
pub struct Paths {
    pub root: PathBuf,
    _dir: tempfile::TempDir,
    used: Vec<(&'static str, PathBuf)>,
}

impl Paths {
    pub fn new() -> Paths {
        let dir = tempfile::tempdir().unwrap();
        // macOS: the temporary directory is under /var, a link to /private/var.
        let root = dir.path().canonicalize().unwrap();
        Paths { root, _dir: dir, used: Vec::new() }
    }
    pub fn dir(&mut self, rel: &str) -> PathBuf {
        let p = self.root.join(rel);
        std::fs::create_dir_all(&p).unwrap();
        self.wrote(&p);
        p
    }
    /// A path the run wrote (or a writer it drove wrote).
    pub fn wrote(&mut self, p: &Path) {
        self.used.push(("wrote", p.to_path_buf()));
    }
    /// A path the product resolved: a store, a ledger, a binding's location.
    pub fn resolved(&mut self, p: &Path) {
        self.used.push(("resolved", p.to_path_buf()));
    }
    /// A committed fixture the run read (repository data, read only).
    pub fn fixture(&mut self, p: &Path) {
        self.used.push(("fixture", p.to_path_buf()));
    }
    /// What a store resolved: its root, its index, every ledger, and every
    /// binding's location with the files sync reads beside it.
    pub fn store(&mut self, s: &Store) {
        self.resolved(s.root());
        self.resolved(&s.root().join("index.db"));
        for a in s.agents().unwrap() {
            self.resolved(&s.ledger_path(&a.id).unwrap());
        }
        for b in s.bindings().unwrap() {
            let loc = PathBuf::from(&b.location);
            self.resolved(&loc);
            self.resolved(&PathBuf::from(format!("{}-wal", loc.display())));
            self.resolved(&loc.with_extension(""));
        }
    }
    /// Every path the run resolved or wrote is under this test's temporary
    /// root, and none lies under the real `$HOME` outside it; every fixture
    /// read is under the repository's `fixtures/`. Prints the population by
    /// role. `PL_SIM_FORCE=home-path` adds one path under the real `$HOME`
    /// (computed, never written) to the population.
    pub fn assert_isolated(&mut self, what: &str) {
        let home = home();
        if force().as_deref() == Some("home-path") {
            let p = home.join(".claude/projects/-pl-sim-forced/session.jsonl");
            eprintln!("sim: PL_SIM_FORCE=home-path: {} handed to the isolation check (computed only)", p.display());
            self.resolved(&p);
        }
        let fixtures = canon(&Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures"));
        assert!(!self.root.starts_with(&home), "{what}: the temporary root {} is itself under $HOME", self.root.display());
        let mut n: HashMap<&str, usize> = HashMap::new();
        for (role, p) in &self.used {
            let c = canon(p);
            *n.entry(role).or_default() += 1;
            if *role == "fixture" {
                assert!(c.starts_with(&fixtures), "{what}: fixture {} is not under {}", c.display(), fixtures.display());
                continue;
            }
            assert!(!(c.starts_with(&home) && !c.starts_with(&self.root)), "{what}: {role} {} lies under the real $HOME outside the temp root", c.display());
            assert!(c.starts_with(&self.root), "{what}: {role} {} is outside the temp root {}", c.display(), self.root.display());
        }
        assert!(n.get("resolved").copied().unwrap_or(0) > 0 && n.get("wrote").copied().unwrap_or(0) > 0, "{what}: an empty population: {n:?}");
        eprintln!(
            "sim: {what}: isolation: {} resolved, {} written, {} fixture paths checked; none under $HOME outside {}",
            n.get("resolved").unwrap_or(&0),
            n.get("wrote").unwrap_or(&0),
            n.get("fixture").unwrap_or(&0),
            self.root.display()
        );
    }
}

/// What the simulator tells the test as it writes.
pub enum Mark {
    /// A line was flushed without its end (the write is still in progress).
    PartialStart(Instant),
    /// The rest of that line is about to be written.
    PartialEnd(Instant),
    /// The turn ended: the simulator's last write is done.
    End(Instant),
}

/// What happened while a simulated turn ran.
pub struct Watched {
    /// When the turn ended.
    pub end: Instant,
    /// Event id → when a sync first appended it, and what made that sync run.
    pub seen: HashMap<String, (Instant, Trigger)>,
    /// Every sync that changed the store, in order.
    pub changed: Vec<Synced>,
    /// How many syncs the follower made in all.
    pub syncs: u64,
    /// While a line was half written: how many syncs the follower made (sync
    /// CALLS, whatever each did), and how many times the binding's cursor
    /// moved (a sync that read the location sets it; one deferred by the
    /// torn-line check does not).
    pub mid_line: Option<MidLine>,
    /// When the follower stopped stepping: [`QUIET`] after the last wanted
    /// event was appended (or [`GRACE`] after the end, if one never was).
    pub stopped: Instant,
}

/// What the follower did while a line was half written.
#[derive(Debug, Clone, Copy)]
pub struct MidLine {
    pub syncs: u64,
    pub cursor_moves: u64,
}

/// Run the simulator `sim` on its own thread and step `follower` (when there
/// is one) on this one, until [`QUIET`] after every id `want` gives (asked
/// once the turn ended) has been appended, or [`GRACE`] after the end.
/// `binding`'s cursor is sampled after every step. With
/// `PL_SIM_FORCE=late-divergence`, `diverge` is called once, as soon as
/// every wanted id is in the ledger: it must change the location so that the
/// next sync finds it diverged (only a check that outlasts a poll period
/// past the last event sees it).
pub fn follow_turn(
    s: &mut Store,
    mut follower: Option<&mut Follower>,
    binding: &str,
    sim: impl FnOnce(mpsc::Sender<Mark>) + Send + 'static,
    want: impl Fn() -> Vec<String>,
    diverge: impl FnOnce(),
) -> Watched {
    // One turn at a time in this test binary: the 2 s bound is a latency
    // measured on a wall clock, and every turn runs in four store modes, so
    // the turns running side by side would measure each other's load.
    static TURN: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _one_at_a_time = TURN.lock().unwrap_or_else(|e| e.into_inner());
    let (tx, rx) = mpsc::channel();
    let th = std::thread::spawn(move || sim(tx));
    let late = force().as_deref() == Some("late-follower");
    let mut end: Option<Instant> = None;
    let mut wanted: Option<Vec<String>> = None;
    let mut seen = HashMap::new();
    let mut changed = Vec::new();
    let cursor = |s: &Store| s.cursor(binding).unwrap();
    // (when a step returned, the follower's sync count then, the binding's cursor then)
    let mut samples: Vec<(Instant, u64, Option<String>)> = vec![(Instant::now(), follower.as_ref().map(|f| f.syncs()).unwrap_or(0), cursor(s))];
    let (mut p_start, mut p_end) = (None, None);
    let mut slept = false;
    let mut all_in: Option<Instant> = None;
    let mut diverge = Some(diverge);
    let late_divergence = force().as_deref() == Some("late-divergence");
    loop {
        while let Ok(m) = rx.try_recv() {
            match m {
                Mark::PartialStart(t) => p_start = Some(t),
                Mark::PartialEnd(t) => p_end = Some(t),
                Mark::End(t) => end = Some(t),
            }
        }
        let now = Instant::now();
        if let Some(e) = end {
            let w = wanted.get_or_insert_with(&want);
            if all_in.is_none() && w.iter().all(|id| seen.contains_key(id)) {
                all_in = Some(now);
                if late_divergence && let Some(d) = diverge.take() {
                    eprintln!("sim: PL_SIM_FORCE=late-divergence: the location is changed to diverge after the last event");
                    d();
                }
            }
            if all_in.is_some_and(|t| now >= t + QUIET) || now >= e + GRACE {
                break;
            }
            if late && !slept {
                eprintln!("sim: PL_SIM_FORCE=late-follower: the follower stops for {LATE:?} at the end of the turn");
                std::thread::sleep(LATE);
                slept = true;
                continue;
            }
        }
        let until = match end {
            Some(e) => (e + GRACE).min(now + Duration::from_millis(20)),
            None => now + Duration::from_millis(20),
        };
        match follower.as_deref_mut() {
            Some(f) => {
                for (trigger, r) in f.step(s, until).unwrap() {
                    let at = Instant::now();
                    for p in &r.appended {
                        seen.entry(p.event.id.clone()).or_insert((at, trigger));
                    }
                    changed.push(r);
                }
                samples.push((Instant::now(), f.syncs(), cursor(s)));
            }
            None => std::thread::sleep(until.saturating_duration_since(Instant::now())),
        }
    }
    let stopped = Instant::now();
    th.join().expect("the simulator");
    let mid_line = match (p_start, p_end) {
        (Some(a), Some(b)) => {
            // Syncs that finished after the partial write and before its end.
            let before = samples.iter().filter(|(t, ..)| *t <= a).map(|(_, n, _)| *n).max().unwrap_or(0);
            let during = samples.iter().filter(|(t, ..)| *t < b).map(|(_, n, _)| *n).max().unwrap_or(0);
            // Steps that returned in that window with the cursor changed since the step before.
            let cursor_moves = samples.windows(2).filter(|p| p[1].0 > a && p[1].0 < b && p[1].2 != p[0].2).count() as u64;
            Some(MidLine { syncs: during.saturating_sub(before), cursor_moves })
        }
        _ => None,
    };
    Watched { end: end.expect("the turn ended"), seen, changed, syncs: follower.map(|f| f.syncs()).unwrap_or(0), mid_line, stopped }
}

/// Milliseconds from `from` to `to`, signed (negative: `to` came first).
pub fn ms(from: Instant, to: Instant) -> i64 {
    if to >= from { (to - from).as_millis() as i64 } else { -((from - to).as_millis() as i64) }
}

/// Every id in `want` is in `got` exactly once, and `got` holds nothing else
/// (beyond what `extra_ok` allows).
pub fn once_each(got: &[Event], want: &[String], what: &str, extra_ok: impl Fn(&Event) -> bool) {
    assert!(!want.is_empty(), "{what}: the turn made no new events to look for");
    for id in want {
        let n = got.iter().filter(|e| &e.id == id).count();
        assert_eq!(n, 1, "{what}: event {id} is in the ledger {n} times (once each)");
    }
    let extra: Vec<String> = got.iter().filter(|e| !want.contains(&e.id) && !extra_ok(e)).map(|e| format!("{} {:?}", e.id, e.kind)).collect();
    assert!(extra.is_empty(), "{what}: the ledger gained events the turn did not make: {extra:?}");
}

/// The 2 s bound: every id of `want` was appended by the follower, the last
/// of them within [`BOUND`] of the turn's end. Returns that latency (ms).
pub fn within_bound(want: &[String], w: &Watched, what: &str) -> i64 {
    let mut worst = i64::MIN;
    let mut by = None;
    for id in want {
        let (t, trigger) = w.seen.get(id).unwrap_or_else(|| panic!("{what}: event {id} was never appended by the follower"));
        if ms(w.end, *t) > worst {
            worst = ms(w.end, *t);
            by = Some(*trigger);
        }
    }
    eprintln!(
        "sim: {what}: last event in the ledger {worst:+} ms after the turn ended (bound {BOUND:?}), appended by a {}-triggered sync; {} syncs",
        by.map(|t| format!("{t:?}")).unwrap_or_default(),
        w.syncs
    );
    assert!(worst <= BOUND.as_millis() as i64, "{what}: the last event reached the ledger {worst} ms after the turn ended, past the {BOUND:?} bound");
    worst
}

/// Some sync appended events before the turn ended: the follower read the
/// location while the turn was still being written (the reads the next
/// checks are about happened).
pub fn read_mid_turn(w: &Watched, what: &str) {
    let early = w.seen.values().filter(|(t, _)| *t < w.end).count();
    eprintln!("sim: {what}: {early} events appended before the turn ended, by {} syncs that changed the store", w.changed.len());
    assert!(early > 0 && w.changed.len() >= 2, "{what}: the follower never read the location mid-turn");
}

/// No read led to a divergence, up to [`QUIET`] (a poll period and the
/// debounce) after the last wanted event: the follower kept stepping that
/// long, so a sync after the last event is in `w.changed` too.
pub fn no_divergence(all: &[Event], w: &Watched, what: &str) {
    let last = w.seen.values().map(|(t, _)| *t).max().unwrap_or(w.end);
    assert!(
        w.stopped >= last + QUIET,
        "{what}: the follower stopped {} ms after the last event, before {QUIET:?} (a poll period) had passed",
        ms(last, w.stopped)
    );
    let diverged: Vec<&str> = all.iter().filter(|e| e.str("type") == Some("protoledger/diverged")).map(|e| e.id.as_str()).collect();
    assert!(diverged.is_empty(), "{what}: the binding diverged: {diverged:?}");
    assert!(w.changed.iter().all(|r| r.diverged.is_none()), "{what}: a sync found the location diverged");
}

/// No read led to a divergence, and after the follower's first read of the
/// location every read that appended went on from where the last one stopped
/// (no whole re-import): for a format that follows a growing file.
pub fn no_divergence_no_reimport(all: &[Event], w: &Watched, binding: &str, f: Option<&Follower>, what: &str) {
    no_divergence(all, w, what);
    let whole: Vec<usize> = w.changed.iter().enumerate().skip(1).filter(|(_, r)| !r.followed).map(|(i, _)| i).collect();
    assert!(whole.is_empty(), "{what}: syncs {whole:?} of {} re-imported the location whole", w.changed.len());
    if let Some(f) = f {
        assert!(f.incremental().iter().any(|b| b == binding), "{what}: the follower no longer reads the location on from where it stopped");
    }
}
