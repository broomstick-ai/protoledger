//! `protoledger serve`, end to end through the binary: the WebSocket stream
//! during a live sync (two clients from different seqs; a reconnect from the
//! last seq seen), every HTTP endpoint against a seeded store, and unbind by
//! the process that holds the writer locks.
//!
//! Every server here listens on a loopback address (127.0.0.1:0 unless the
//! test is about the listen address), is started by its test and is
//! killed by its own `Child` handle (on drop, so a failing test stops it too).
//! Servers run poll-only at 100 ms with no filesystem watcher, so the longest
//! period between a source append and its events on the wire is one poll
//! plus the engine's 20 ms tick; every "nothing more" assertion waits 1 s,
//! ten polls.
//!
//! Every test about a store's contents runs in a plain store (its own name)
//! and in each compressed mode (`<test>::no_dictionary`,
//! `::passed_dictionary`, `::retrained`; SPEC §10.11): the backlog a
//! subscriber is sent comes from sealed blocks and the hot tail, and what
//! the tests read of a ledger they read through the store's reader.
#![cfg(all(feature = "serve", feature = "cli"))]

#[macro_use]
#[path = "modes/mod.rs"]
mod modes;

use modes::Mode;
use protoledger::LedgerSet;
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

const POLL_MS: u64 = 100;
/// Ten polls: how long a "nothing more arrives" assertion waits.
const QUIET: Duration = Duration::from_millis(10 * POLL_MS);
/// How long anything expected may take to arrive.
const PATIENCE: Duration = Duration::from_secs(15);

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_protoledger"))
}

fn fixture(p: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures").join(p)
}

fn run(args: &[&str]) -> String {
    let out = bin().args(args).output().unwrap();
    assert!(out.status.success(), "protoledger {args:?} failed: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8(out.stdout).unwrap()
}

// ---- the server ----

struct Serve {
    child: Child,
    addr: String,
}

impl Serve {
    fn start(store: &Path) -> Serve {
        let s = Serve::start_on(store, "127.0.0.1:0", &[]);
        assert!(s.addr.starts_with("127.0.0.1:"), "{}", s.addr);
        s
    }

    /// Start serve listening on `listen` (with `extra` args); panics unless
    /// it says where it listens.
    fn start_on(store: &Path, listen: &str, extra: &[&str]) -> Serve {
        let mut child = bin()
            .args(["serve", "--store", store.to_str().unwrap(), "--listen", listen, "--no-watcher"])
            .args(["--poll-ms", &POLL_MS.to_string(), "--debounce-ms", "50"])
            .args(extra)
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            let mut line = String::new();
            let _ = BufReader::new(stdout).read_line(&mut line);
            let _ = tx.send(line);
        });
        let line = match rx.recv_timeout(PATIENCE) {
            Ok(l) => l,
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("serve --listen {listen} {extra:?} did not say where it listens");
            }
        };
        let Some(addr) = line.trim().strip_prefix("listening on ") else {
            let _ = child.kill();
            let _ = child.wait();
            panic!("serve --listen {listen} {extra:?}: its first line: {line:?}");
        };
        let addr = addr.to_string();
        let s = Serve { child, addr };
        assert!(!s.addr.ends_with(":0"), "{}", s.addr);
        s
    }

    fn get(&self, path: &str) -> (u16, Value) {
        http(&self.addr, "GET", path, None)
    }

    fn post(&self, path: &str, body: Value) -> (u16, Value) {
        http(&self.addr, "POST", path, Some(&body.to_string()))
    }

    fn ws(&self, from: &str) -> Ws {
        Ws::connect(&self.addr, from)
    }

    /// An agent's indexed last seq, as serve reports it.
    fn last_seq(&self, agent: &str) -> u64 {
        let (st, a) = self.get(&format!("/v1/agents/{agent}"));
        assert_eq!(st, 200, "{a}");
        a["last_seq"].as_u64().unwrap()
    }

    /// Wait until serve's index says `agent` has reached `seq`.
    fn wait_for_seq(&self, agent: &str, seq: u64) {
        let end = Instant::now() + PATIENCE;
        while self.last_seq(agent) < seq {
            assert!(Instant::now() < end, "{agent} never reached seq {seq} (at {})", self.last_seq(agent));
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for Serve {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// One HTTP/1.1 request on a fresh connection; the status and the JSON body.
fn http(addr: &str, method: &str, path: &str, body: Option<&str>) -> (u16, Value) {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(PATIENCE)).unwrap();
    let body = body.unwrap_or("");
    write!(
        s,
        "{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    let mut raw = Vec::new();
    s.read_to_end(&mut raw).unwrap();
    let text = String::from_utf8(raw).unwrap();
    let (head, rest) = text.split_once("\r\n\r\n").unwrap_or_else(|| panic!("no header end: {text:?}"));
    let status: u16 = head.split(' ').nth(1).unwrap().parse().unwrap();
    let chunked = head.lines().any(|l| l.to_ascii_lowercase().starts_with("transfer-encoding: chunked"));
    let body = if chunked {
        let mut out = String::new();
        let mut r = rest;
        loop {
            let (n, tail) = r.split_once("\r\n").unwrap();
            let n = usize::from_str_radix(n.trim(), 16).unwrap();
            if n == 0 {
                break;
            }
            out.push_str(&tail[..n]);
            r = &tail[n + 2..];
        }
        out
    } else {
        rest.to_string()
    };
    let v = serde_json::from_str(&body).unwrap_or_else(|e| panic!("{method} {path} → {status}: not JSON ({e}): {body:?}"));
    (status, v)
}

// ---- a WebSocket client ----

struct Ws {
    rx: Receiver<Value>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Ws {
    fn connect(addr: &str, from: &str) -> Ws {
        let (mut ws, resp) = tungstenite::connect(format!("ws://{addr}/v1/subscribe?from={from}")).unwrap();
        assert_eq!(resp.status().as_u16(), 101);
        if let tungstenite::stream::MaybeTlsStream::Plain(s) = ws.get_mut() {
            s.set_read_timeout(Some(Duration::from_millis(50))).unwrap();
        }
        let (tx, rx) = channel();
        let stop = Arc::new(AtomicBool::new(false));
        let stop2 = stop.clone();
        let thread = std::thread::spawn(move || {
            loop {
                if stop2.load(Ordering::SeqCst) {
                    let _ = ws.close(None);
                    let _ = ws.flush();
                    return;
                }
                match ws.read() {
                    Ok(tungstenite::Message::Text(t)) => {
                        let v: Value = serde_json::from_str(t.as_str()).unwrap();
                        if tx.send(v).is_err() {
                            return;
                        }
                    }
                    Ok(_) => {}
                    Err(tungstenite::Error::Io(e)) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {}
                    Err(_) => return,
                }
            }
        });
        Ws { rx, stop, thread: Some(thread) }
    }

    /// The next `n` messages; panics if they do not all come in time.
    fn take(&self, n: usize) -> Vec<Value> {
        let end = Instant::now() + PATIENCE;
        let mut out = Vec::new();
        while out.len() < n {
            match self.rx.recv_timeout(end.saturating_duration_since(Instant::now())) {
                Ok(v) => out.push(v),
                Err(_) => panic!("{} of {n} messages came: {:?}", out.len(), summary(&out)),
            }
        }
        out
    }

    /// Everything that arrives within `d`.
    fn quiet(&self, d: Duration) -> Vec<Value> {
        let end = Instant::now() + d;
        let mut out = Vec::new();
        while let Ok(v) = self.rx.recv_timeout(end.saturating_duration_since(Instant::now())) {
            out.push(v);
        }
        out
    }

    fn close(mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            t.join().unwrap();
        }
    }
}

impl Drop for Ws {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

fn summary(msgs: &[Value]) -> Vec<String> {
    msgs.iter().map(|m| format!("{}#{} {}", m["agent"].as_str().unwrap_or("?"), m["seq"], m["event"]["id"].as_str().unwrap_or("?"))).collect()
}

fn seqs(msgs: &[Value]) -> Vec<u64> {
    msgs.iter().map(|m| m["seq"].as_u64().unwrap()).collect()
}

fn ids(msgs: &[Value]) -> Vec<String> {
    msgs.iter().map(|m| m["event"]["id"].as_str().unwrap().to_string()).collect()
}

/// Each message is well formed for `agent`: its seq is its event's seq.
fn check_shape(msgs: &[Value], agent: &str) {
    for m in msgs {
        assert_eq!(m["agent"].as_str(), Some(agent), "{m}");
        assert_eq!(m["seq"], m["event"]["seq"], "{m}");
        assert!(m["offset"].as_u64().is_some(), "{m}");
    }
}

// ---- a Codex rollout written in parts ----

const ROLLOUT: &str = "codex/legacy-0.142-compaction-real/native/rollout-2026-06-26T23-11-51-019f0634-3175-7702-b55c-e256f2966840.jsonl";
const HEAD_LINES: usize = 12;

/// The ids the Codex importer alone reads from `text` (no store, no serve).
fn import_ids(dir: &Path, text: &str) -> Vec<String> {
    let p = dir.join(Path::new(ROLLOUT).file_name().unwrap());
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(&p, text).unwrap();
    let f = protoledger::formats::by_name("codex").unwrap();
    let imp = f.import(&p, &protoledger::formats::ImportOptions::standard()).unwrap();
    imp.set.iter().next().unwrap().events.iter().map(|e| e.id.clone()).collect()
}

/// A rollout cut into a head (attached) and two later parts, with the event
/// ids each part makes, by the importer alone. The cut between the parts is
/// one at which the importer reads the head and part one exactly as the
/// whole file's prefix.
struct Rollout {
    loc: PathBuf,
    head: String,
    parts: [String; 2],
    part_ids: [Vec<String>; 2],
}

fn rollout(root: &Path) -> Rollout {
    let text = std::fs::read_to_string(fixture(ROLLOUT)).unwrap();
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let (head, rest) = lines.split_at(HEAD_LINES);
    let scratch = root.join("scratch");
    let whole = import_ids(&scratch, &text);
    let head_ids = import_ids(&scratch, &head.concat());
    assert_eq!(whole[..head_ids.len()], head_ids[..], "the head reads the same alone");
    // The clean cut nearest the middle of the rest.
    let mut cuts: Vec<usize> = (1..rest.len()).collect();
    cuts.sort_by_key(|k| k.abs_diff(rest.len() / 2));
    let (k, mid_ids) = cuts
        .into_iter()
        .find_map(|k| {
            let ids = import_ids(&scratch, &(head.concat() + &rest[..k].concat()));
            (ids.len() > head_ids.len() + 1 && ids.len() + 1 < whole.len() && whole[..ids.len()] == ids[..]).then_some((k, ids))
        })
        .expect("a clean cut");
    let loc_dir = root.join("codex");
    std::fs::create_dir_all(&loc_dir).unwrap();
    let loc = loc_dir.join(Path::new(ROLLOUT).file_name().unwrap());
    std::fs::write(&loc, head.concat()).unwrap();
    Rollout {
        loc,
        head: head.concat(),
        parts: [rest[..k].concat(), rest[k..].concat()],
        part_ids: [mid_ids[head_ids.len()..].to_vec(), whole[mid_ids.len()..].to_vec()],
    }
}

fn append(path: &Path, text: &str) {
    let mut f = std::fs::OpenOptions::new().append(true).open(path).unwrap();
    f.write_all(text.as_bytes()).unwrap();
    f.sync_all().unwrap();
}

/// Attach the rollout's head through serve; the binding id and the agent.
fn attach(s: &Serve, r: &Rollout) -> (String, String) {
    assert!(!r.head.is_empty());
    let (st, v) = s.post("/v1/attach", json!({ "format": "codex", "location": r.loc }));
    assert_eq!(st, 200, "{v}");
    let b = &v["binding"];
    assert_eq!((b["mode"].as_str(), b["active"].as_bool()), (Some("attachment"), Some(true)), "{v}");
    (b["binding"].as_str().unwrap().to_string(), b["agent"].as_str().unwrap().to_string())
}

/// Every event id of a ledger in the store, read from its files (in either
/// form) by the store's reader, not through serve.
fn ledger_ids(store: &Path, agent: &str) -> Vec<String> {
    let set = modes::read_set(store);
    set.get(agent).unwrap().events.iter().map(|e| e.id.clone()).collect()
}

/// Start serve on `store` (in `m`) with the rollout's head attached. In the
/// retrained mode the head is attached through the library and the store
/// retrained before serve starts, since a recompression needs the writer
/// locks serve then holds; otherwise it is attached through serve.
fn start_attached(m: Mode, store: &Path, r: &Rollout) -> (Serve, String, String) {
    m.prepare(store);
    if m == Mode::Retrained {
        let mut st = protoledger::Store::open(store).unwrap();
        let (row, _) = protoledger::sync::attach(&mut st, "codex", &r.loc).unwrap();
        m.retrain(&mut st);
        drop(st);
        let s = Serve::start(store);
        return (s, row.binding, row.agent);
    }
    let s = Serve::start(store);
    let (binding, agent) = attach(&s, r);
    (s, binding, agent)
}

// ---- (a) ----

/// Two clients, one subscribed below the live seq and one at it, while
/// serve's sync engine follows a Codex rollout that gains lines: each gets
/// every event once, in seq order, the first its backlog then the live ones;
/// and nothing more arrives in the ten polls after.
#[test]
fn two_clients_from_different_seqs_get_every_live_sync_event_once_in_order_quiet_past_ten_100ms_polls() {
    two_clients_from_different_seqs(Mode::Plain);
}

fn two_clients_from_different_seqs(m: Mode) {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("store");
    let r = rollout(dir.path());
    let (s, _binding, agent) = start_attached(m, &store, &r);
    let tip = s.last_seq(&agent);
    assert!(tip > 6, "{tip}");
    let before = ledger_ids(&store, &agent);
    assert_eq!(before.len() as u64, tip);
    let low = 3u64;

    let a = s.ws(&format!("{agent}:{low}"));
    let b = s.ws(&format!("{agent}:{tip}"));
    append(&r.loc, &r.parts[0]);
    append(&r.loc, &r.parts[1]);
    let live: Vec<String> = r.part_ids.concat();
    assert!(live.len() > 4, "{live:?}");
    let backlog = (tip - low) as usize;

    let got_a = a.take(backlog + live.len());
    let got_b = b.take(live.len());
    let extra_a = a.quiet(QUIET);
    let extra_b = b.quiet(Duration::ZERO);
    assert!(extra_a.is_empty() && extra_b.is_empty(), "more came: {:?} / {:?}", summary(&extra_a), summary(&extra_b));

    check_shape(&got_a, &agent);
    check_shape(&got_b, &agent);
    let last = tip + live.len() as u64;
    assert_eq!(seqs(&got_a), (low + 1..=last).collect::<Vec<_>>(), "client a: {:?}", summary(&got_a));
    assert_eq!(seqs(&got_b), (tip + 1..=last).collect::<Vec<_>>(), "client b: {:?}", summary(&got_b));
    assert_eq!(ids(&got_a)[..backlog], before[low as usize..], "client a's backlog is the ledger's");
    assert_eq!(ids(&got_a)[backlog..], live[..], "client a's live events are what the appended lines make");
    assert_eq!(ids(&got_b), live, "client b's live events are what the appended lines make");
    // And the ledger agrees.
    assert_eq!(ledger_ids(&store, &agent), [before, live].concat());
    a.close();
    b.close();
    drop(s);
    m.check(&store);
}

// ---- (b) ----

/// A client takes part of a live sync, disconnects; more is appended; it
/// reconnects from the last seq it saw and gets exactly the events it missed,
/// none twice, and nothing more in the ten polls after.
#[test]
fn reconnect_from_last_seq_gets_exactly_the_missed_events_quiet_past_ten_100ms_polls() {
    reconnect_from_last_seq(Mode::Plain);
}

fn reconnect_from_last_seq(m: Mode) {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("store");
    let r = rollout(dir.path());
    let (s, _binding, agent) = start_attached(m, &store, &r);
    let tip = s.last_seq(&agent);

    let c = s.ws(&format!("{agent}:{tip}"));
    append(&r.loc, &r.parts[0]);
    let first = c.take(r.part_ids[0].len());
    check_shape(&first, &agent);
    let n0 = r.part_ids[0].len() as u64;
    assert_eq!(seqs(&first), (tip + 1..=tip + n0).collect::<Vec<_>>(), "first session: {:?}", summary(&first));
    assert_eq!(ids(&first), r.part_ids[0]);
    let extra = c.quiet(QUIET);
    assert!(extra.is_empty(), "first session, more came: {:?}", summary(&extra));
    c.close();
    let seen = *seqs(&first).last().unwrap();

    append(&r.loc, &r.parts[1]);
    let missed = r.part_ids[1].len() as u64;
    s.wait_for_seq(&agent, seen + missed);

    let d = s.ws(&format!("{agent}:{seen}"));
    let second = d.take(missed as usize);
    check_shape(&second, &agent);
    assert_eq!(seqs(&second), (seen + 1..=seen + missed).collect::<Vec<_>>(), "after reconnecting: {:?}", summary(&second));
    let extra = d.quiet(QUIET);
    assert!(extra.is_empty(), "after reconnecting, more came: {:?}", summary(&extra));
    assert_eq!(ids(&second), r.part_ids[1], "exactly the missed events");
    let firsts: std::collections::HashSet<String> = ids(&first).into_iter().collect();
    assert!(ids(&second).iter().all(|i| !firsts.contains(i)), "an event came twice");
    d.close();
    drop(s);
    m.check(&store);
}

// ---- (c) ----

/// A store seeded through the CLI (a parent with a subagent, a second agent
/// projected into Claude Code): every query endpoint answers what the ledger
/// files say, and an unknown agent is 404 on each agent endpoint. Then an
/// import through serve shows up in the next answer.
#[test]
fn http_endpoints_answer_a_seeded_store_and_an_unknown_agent_is_404() {
    http_endpoints(Mode::Plain);
}

fn http_endpoints(m: Mode) {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("store");
    m.prepare(&store);
    let st = store.to_str().unwrap();
    run(&["store", "import", st, "protoledger", fixture("protoledger/native-subagent-compaction/native").to_str().unwrap()]);
    run(&["store", "import", st, "protoledger", fixture("protoledger/minimal/native").to_str().unwrap()]);
    let cc = dir.path().join("cc");
    let projected = run(&["project", st, "hello-1", "claude-code", cc.to_str().unwrap()]).trim().to_string();
    assert_eq!(projected.len(), 36, "{projected:?}");
    m.retrain_at(&store);
    m.check(&store);
    // What the ledgers say, read from the files.
    let set = modes::read_set(&store);
    let mut want: Vec<(String, u64)> = set.iter().map(|l| (l.id().to_string(), l.last_seq())).collect();
    want.sort();
    assert_eq!(want.iter().map(|(a, _)| a.as_str()).collect::<Vec<_>>(), ["hello-1", "main-1", "main-1.sub-1"]);

    let s = Serve::start(&store);

    // /v1/agents
    let (code, v) = s.get("/v1/agents");
    assert_eq!(code, 200, "{v}");
    let mut got: Vec<(String, u64)> = v.as_array().unwrap().iter().map(|a| (a["id"].as_str().unwrap().to_string(), a["last_seq"].as_u64().unwrap())).collect();
    got.sort();
    assert_eq!(got, want);
    let sub = v.as_array().unwrap().iter().find(|a| a["id"] == "main-1.sub-1").unwrap();
    assert_eq!(sub["parent"]["agent"], "main-1", "{sub}");
    assert_eq!(sub["path"], "ledgers/main-1.sub-1.jsonl", "{sub}");

    // /v1/agents/{id}
    let main = set.get("main-1").unwrap();
    let (code, v) = s.get("/v1/agents/main-1");
    assert_eq!(code, 200, "{v}");
    assert_eq!((v["id"].as_str(), v["last_seq"].as_u64()), (Some("main-1"), Some(main.last_seq())), "{v}");
    assert_eq!(v["created_at"], json!(main.agent().created_at), "{v}");

    // /v1/agents/{id}/events: from is the last seq seen, limit the page size.
    assert!(main.events.len() >= 6, "{}", main.events.len());
    let (code, v) = s.get("/v1/agents/main-1/events?from=2&limit=3");
    assert_eq!(code, 200, "{v}");
    let page = v.as_array().unwrap();
    assert_eq!(seqs(page), [3, 4, 5]);
    for (m, e) in page.iter().zip(&main.events[2..5]) {
        assert_eq!(m["agent"], "main-1");
        assert_eq!(m["event"], serde_json::to_value(e).unwrap(), "seq {}", e.seq);
    }
    let (code, v) = s.get("/v1/agents/main-1/events");
    assert_eq!(code, 200);
    assert_eq!(ids(v.as_array().unwrap()), main.events.iter().map(|e| e.id.clone()).collect::<Vec<_>>());
    let (code, v) = s.get(&format!("/v1/agents/main-1/events?from={}", main.last_seq()));
    assert_eq!((code, v), (200, json!([])));
    let (code, v) = s.get("/v1/agents/main-1/events?from=x");
    assert_eq!(code, 400, "{v}");
    // Paging, each page's last seq the next `from` (the resume rule): the
    // whole ledger, nothing twice, nothing missed.
    let mut paged: Vec<Value> = Vec::new();
    let mut from = 0u64;
    for _ in 0..=main.events.len() {
        let (code, v) = s.get(&format!("/v1/agents/main-1/events?from={from}&limit=2"));
        assert_eq!(code, 200, "{v}");
        let page = v.as_array().unwrap().clone();
        let Some(last) = page.last() else { break };
        from = last["seq"].as_u64().unwrap();
        paged.extend(page);
    }
    assert_eq!(seqs(&paged), (1..=main.last_seq()).collect::<Vec<_>>(), "paging /events from each page's last seq: {:?}", summary(&paged));
    assert_eq!(ids(&paged), main.events.iter().map(|e| e.id.clone()).collect::<Vec<_>>());

    // /v1/agents/{id}/children
    let (code, v) = s.get("/v1/agents/main-1/children");
    assert_eq!(code, 200, "{v}");
    assert_eq!(v.as_array().unwrap().iter().map(|a| a["id"].as_str().unwrap()).collect::<Vec<_>>(), ["main-1.sub-1"]);
    let (code, v) = s.get("/v1/agents/hello-1/children");
    assert_eq!((code, v), (200, json!([])));

    // /v1/bindings
    let (code, v) = s.get("/v1/bindings");
    assert_eq!(code, 200, "{v}");
    let bs = v.as_array().unwrap();
    assert_eq!(bs.len(), 1, "{v}");
    assert_eq!(
        (bs[0]["binding"].as_str(), bs[0]["agent"].as_str(), bs[0]["format"].as_str(), bs[0]["mode"].as_str(), bs[0]["active"].as_bool()),
        (Some(projected.as_str()), Some("hello-1"), Some("claude-code"), Some("projection"), Some(true)),
        "{v}"
    );

    // Unknown agents are 404 on every agent endpoint.
    for p in ["/v1/agents/nobody", "/v1/agents/nobody/events", "/v1/agents/nobody/children"] {
        let (code, v) = s.get(p);
        assert_eq!(code, 404, "{p}: {v}");
        assert!(v["error"].as_str().unwrap().contains("nobody"), "{p}: {v}");
    }

    // POST /v1/import, then the agents are there.
    let (code, v) = s.post("/v1/import", json!({ "format": "protoledger", "path": fixture("protoledger/orchestrated-peers/native") }));
    assert_eq!(code, 200, "{v}");
    let created: Vec<&str> = v["created"].as_array().unwrap().iter().map(|a| a.as_str().unwrap()).collect();
    assert!(created.contains(&"coord-1") && created.contains(&"peer-a"), "{v}");
    let (_, v) = s.get("/v1/agents");
    assert_eq!(v.as_array().unwrap().len(), 3 + created.len());
    let (code, v) = s.post("/v1/import", json!({ "format": "protoledger" }));
    assert_eq!(code, 400, "{v}");
    drop(s);
    m.check(&store);
}

/// `from=all` streams every agent from its first event; a malformed cursor
/// is refused before the upgrade.
#[test]
fn subscribe_all_streams_every_agent_from_seq_one_and_a_bad_cursor_is_400() {
    subscribe_all(Mode::Plain);
}

fn subscribe_all(m: Mode) {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("store");
    m.prepare(&store);
    let st = store.to_str().unwrap();
    run(&["store", "import", st, "protoledger", fixture("protoledger/native-subagent-compaction/native").to_str().unwrap()]);
    m.retrain_at(&store);
    m.check(&store);
    let set = modes::read_set(&store);
    let total: usize = set.iter().map(|l| l.events.len()).sum();
    let s = Serve::start(&store);
    let w = s.ws("all");
    let got = w.take(total);
    assert!(w.quiet(QUIET).is_empty());
    for l in set.iter() {
        let mine: Vec<Value> = got.iter().filter(|m| m["agent"] == l.id()).cloned().collect();
        check_shape(&mine, l.id());
        assert_eq!(ids(&mine), l.events.iter().map(|e| e.id.clone()).collect::<Vec<_>>(), "{}", l.id());
    }
    w.close();
    for bad in ["", "main-1", "main-1:x", "main-1:1,main-1:2"] {
        let (code, v) = http(&s.addr, "GET", &format!("/v1/subscribe?from={bad}"), None);
        assert_eq!(code, 400, "from={bad:?}: {v}");
    }
}

// ---- (d) ----

/// While serve follows a binding (and so holds its ledger's writer lock),
/// `protoledger unbind` from another process is refused as locked, and the
/// same unbind through serve succeeds: the binding is inactive, the ledger
/// ends in its `protoledger/unbound` event, and what the location gains
/// afterwards is not appended (ten polls).
#[test]
fn unbind_through_serve_succeeds_while_serve_follows_where_cli_unbind_is_refused_locked() {
    unbind_through_serve(Mode::Plain);
}

fn unbind_through_serve(m: Mode) {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("store");
    let r = rollout(dir.path());
    let (s, binding, agent) = start_attached(m, &store, &r);
    let tip = s.last_seq(&agent);
    // It is being followed.
    append(&r.loc, &r.parts[0]);
    s.wait_for_seq(&agent, tip + r.part_ids[0].len() as u64);

    let out = bin().args(["unbind", store.to_str().unwrap(), &binding]).output().unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success() && err.contains("locked"), "the CLI's unbind: {:?} {err}", out.status);

    let (code, v) = s.post("/v1/unbind", json!({ "binding": binding }));
    assert_eq!(code, 200, "{v}");
    assert_eq!((v["active"].as_bool(), v["ended"].as_str()), (Some(false), Some("unbound")), "{v}");
    let (_, bs) = s.get("/v1/bindings");
    assert_eq!(bs.as_array().unwrap().iter().filter(|b| b["binding"] == binding.as_str() && b["active"] == false).count(), 1, "{bs}");
    let ids_now = ledger_ids(&store, &agent);
    assert_eq!(ids_now.last().unwrap(), &format!("protoledger/unbound:{binding}"));
    let after_unbind = s.last_seq(&agent);
    assert_eq!(after_unbind, ids_now.len() as u64);

    append(&r.loc, &r.parts[1]);
    std::thread::sleep(QUIET);
    assert_eq!(s.last_seq(&agent), after_unbind, "an unbound location was still followed");

    let (code, v) = s.post("/v1/unbind", json!({ "binding": binding }));
    assert_eq!(code, 409, "{v}");
    let (code, v) = s.post("/v1/unbind", json!({ "binding": "no-such-binding" }));
    assert_eq!(code, 404, "{v}");

    // With serve stopped the lock is gone: the same CLI call now reaches the
    // binding and is refused for what it is, inactive, not as locked.
    drop(s);
    let out = bin().args(["unbind", store.to_str().unwrap(), &binding]).output().unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success() && err.contains("not active") && !err.contains("locked"), "{:?} {err}", out.status);
    m.check(&store);
}

// ---- the seam: a new agent written just before a subscribe ----

const TS: &str = "2026-10-16T10:30:00.000Z";

fn header_line(id: &str) -> String {
    format!("{{\"protoledger\":\"0.1\",\"agent\":{{\"id\":\"{id}\",\"created_at\":\"{TS}\"}}}}\n")
}

fn event_line(id: &str, seq: u64) -> String {
    format!("{{\"id\":\"{id}:t:{seq}\",\"seq\":{seq},\"ts\":\"{TS}\",\"kind\":\"text\",\"body\":{{\"role\":\"user\",\"text\":\"{seq}\"}}}}\n")
}

/// Put a whole ledger of `n` events into the store's ledgers directory, as
/// another program would: written aside, then renamed in (so no reader ever
/// sees half of it).
fn drop_ledger(store: &Path, aside: &Path, id: &str, n: u64) {
    let mut text = header_line(id);
    for k in 1..=n {
        text += &event_line(id, k);
    }
    let tmp = aside.join(format!("{id}.jsonl"));
    std::fs::write(&tmp, text).unwrap();
    std::fs::rename(&tmp, store.join("ledgers").join(format!("{id}.jsonl"))).unwrap();
}

/// Up to `n` messages, waiting no later than `end`; never panics.
fn take_up_to(w: &Ws, n: usize, end: Instant) -> Vec<Value> {
    let mut out = Vec::new();
    while out.len() < n {
        match w.rx.recv_timeout(end.saturating_duration_since(Instant::now())) {
            Ok(v) => out.push(v),
            Err(_) => break,
        }
    }
    out
}

/// A stress probe: 100 clients subscribe `all`, each right after
/// another program wrote a new agent (pcN, a wait of 0 to 110 ms, i.e. up to
/// one 100 ms poll and a bit, then pdN), while a third agent's ledger is
/// appended to every 10 ms throughout. A ledger first indexed by the store's
/// catch-up inside the engine's own re-subscribe is backfilled to that client
/// AND covered by the hub from 0 later; only the per-agent live floor
/// `max(from, snap)` keeps it from arriving twice. Every client gets every
/// agent's events exactly once, in seq order, and nothing more arrives in
/// the ten 100 ms polls after (the longest period on the path: poll 100 ms,
/// engine tick 20 ms, no watcher). 100 and not the probe's 40: with the
/// floor removed the race hits from 2 to 11 clients in 40 on this machine,
/// so 40 can come out clean by luck.
#[test]
fn a_hundred_clients_subscribing_all_right_after_a_new_agent_get_every_event_once_quiet_past_ten_100ms_polls() {
    const CLIENTS: u64 = 100;
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("store");
    let st = store.to_str().unwrap();
    run(&["store", "import", st, "protoledger", fixture("protoledger/minimal/native").to_str().unwrap()]);
    let aside = dir.path().join("aside");
    std::fs::create_dir_all(&aside).unwrap();
    let s = Serve::start(&store);

    // Appends streaming: another program extends stream-1 every 10 ms.
    drop_ledger(&store, &aside, "stream-1", 1);
    let stop = Arc::new(AtomicBool::new(false));
    let streamer = {
        let stop = stop.clone();
        let path = store.join("ledgers").join("stream-1.jsonl");
        std::thread::spawn(move || {
            let mut seq = 1u64;
            while !stop.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(10));
                seq += 1;
                append(&path, &event_line("stream-1", seq));
            }
            seq
        })
    };

    let mut clients = Vec::new();
    for n in 0..CLIENTS {
        drop_ledger(&store, &aside, &format!("pc{n}"), 4);
        std::thread::sleep(Duration::from_millis((n * 37) % 111));
        drop_ledger(&store, &aside, &format!("pd{n}"), 8);
        clients.push(s.ws("all"));
    }
    stop.store(true, Ordering::SeqCst);
    let stream_last = streamer.join().unwrap();
    s.wait_for_seq("stream-1", stream_last);
    s.wait_for_seq(&format!("pd{}", CLIENTS - 1), 8);

    // What every client is owed, read from the ledger files.
    let set = LedgerSet::read_path(&store.join("ledgers")).unwrap();
    let want: std::collections::BTreeMap<String, Vec<String>> =
        set.iter().map(|l| (l.id().to_string(), l.events.iter().map(|e| e.id.clone()).collect())).collect();
    assert_eq!(want.len() as u64, 2 + 2 * CLIENTS, "{:?}", want.keys().collect::<Vec<_>>());
    assert_eq!(want["stream-1"].len() as u64, stream_last);
    let total: usize = want.values().map(Vec::len).sum();

    // One deadline for all of them: a client short of events costs the wait once.
    let end = Instant::now() + PATIENCE;
    let mut got: Vec<Vec<Value>> = clients.iter().map(|c| take_up_to(c, total, end)).collect();
    std::thread::sleep(QUIET);
    for (g, c) in got.iter_mut().zip(&clients) {
        g.extend(c.quiet(Duration::ZERO));
    }

    let mut dup_clients = Vec::new();
    let mut gap_clients = Vec::new();
    for (i, g) in got.iter().enumerate() {
        let mut per: std::collections::BTreeMap<String, Vec<Value>> = std::collections::BTreeMap::new();
        for m in g {
            per.entry(m["agent"].as_str().unwrap().to_string()).or_default().push(m.clone());
        }
        let mut dups = Vec::new();
        let mut gaps = Vec::new();
        for (agent, ids_want) in &want {
            let mine = per.remove(agent).unwrap_or_default();
            check_shape(&mine, agent);
            let mut seen = std::collections::HashSet::new();
            let d: Vec<u64> = seqs(&mine).into_iter().filter(|q| !seen.insert(*q)).collect();
            if !d.is_empty() {
                dups.push(format!("{agent} again: {d:?}"));
            }
            if ids(&mine) != *ids_want && d.is_empty() {
                gaps.push(format!("{agent}: got {:?} of {}", seqs(&mine), ids_want.len()));
            }
        }
        assert!(per.is_empty(), "client {i}: agents no ledger holds: {:?}", per.keys().collect::<Vec<_>>());
        if !dups.is_empty() {
            dup_clients.push(format!("client {i}: {}", dups.join("; ")));
        }
        if !gaps.is_empty() {
            gap_clients.push(format!("client {i}: {}", gaps.join("; ")));
        }
    }
    assert!(dup_clients.is_empty(), "{}/{CLIENTS} clients got duplicates: {dup_clients:#?}", dup_clients.len());
    assert!(gap_clients.is_empty(), "{}/{CLIENTS} clients have gaps: {gap_clients:#?}", gap_clients.len());
    for c in clients {
        c.close();
    }
}

// ---- /v1/project over HTTP ----

/// `POST /v1/project` through serve: the projection is written and bound
/// (mode projection, active, at a file in the dir), its `bound` event is
/// published to a subscriber like any append, serve then holds it as a
/// binding it can unbind; an unknown agent is 404, an unknown format 422, a
/// missing field 400, and a second projection into the same dir 422.
#[test]
fn project_over_http_binds_publishes_its_bound_event_and_refuses_404_422_400() {
    project_over_http(Mode::Plain);
}

fn project_over_http(m: Mode) {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("store");
    m.prepare(&store);
    let st = store.to_str().unwrap();
    run(&["store", "import", st, "protoledger", fixture("protoledger/minimal/native").to_str().unwrap()]);
    m.retrain_at(&store);
    let s = Serve::start(&store);
    let tip = s.last_seq("hello-1");
    let w = s.ws(&format!("hello-1:{tip}"));
    let cc = dir.path().join("cc");

    let (code, v) = s.post("/v1/project", json!({ "agent": "hello-1", "format": "claude-code", "dir": cc }));
    assert_eq!(code, 200, "{v}");
    let binding = v["binding"].as_str().unwrap().to_string();
    assert_eq!(
        (v["agent"].as_str(), v["format"].as_str(), v["mode"].as_str(), v["active"].as_bool()),
        (Some("hello-1"), Some("claude-code"), Some("projection"), Some(true)),
        "{v}"
    );
    let loc = PathBuf::from(v["location"].as_str().unwrap());
    assert!(loc.is_file() && loc.starts_with(&cc), "{v}");
    let (_, bs) = s.get("/v1/bindings");
    assert_eq!(bs.as_array().unwrap().iter().filter(|b| b["binding"] == binding.as_str() && b["active"] == true).count(), 1, "{bs}");

    // The ledger gained the binding's events (`bound`, then the
    // `protoledger/projected` fact naming the file written, SPEC §11.2), and
    // the subscriber got exactly them.
    let ledger = ledger_ids(&store, "hello-1");
    assert_eq!(ledger.len() as u64, tip + 2, "{ledger:?}");
    assert_eq!(ledger[tip as usize], format!("protoledger/bound:{binding}"), "{ledger:?}");
    assert!(ledger.last().unwrap().starts_with("protoledger/projected:"), "{ledger:?}");
    let got = w.take(2);
    check_shape(&got, "hello-1");
    assert_eq!(ids(&got), ledger[tip as usize..]);
    assert!(w.quiet(QUIET).is_empty());
    w.close();

    // Refusals.
    let (code, v) = s.post("/v1/project", json!({ "agent": "nobody", "format": "claude-code", "dir": dir.path().join("x") }));
    assert_eq!(code, 404, "{v}");
    let (code, v) = s.post("/v1/project", json!({ "agent": "hello-1", "format": "no-such-format", "dir": dir.path().join("y") }));
    assert_eq!(code, 422, "{v}");
    let (code, v) = s.post("/v1/project", json!({ "agent": "hello-1", "format": "claude-code" }));
    assert_eq!(code, 400, "{v}");
    let (code, v) = s.post("/v1/project", json!({ "agent": "hello-1", "format": "claude-code", "dir": cc }));
    assert_eq!(code, 422, "a second projection into the same dir: {v}");

    // Serve holds the projection as a binding of its own.
    let (code, v) = s.post("/v1/unbind", json!({ "binding": binding }));
    assert_eq!(code, 200, "{v}");
    assert_eq!((v["active"].as_bool(), v["ended"].as_str()), (Some(false), Some("unbound")), "{v}");
    drop(s);
    m.check(&store);
}

// ---- "not followed after unbind", the sync layer alone ----

/// The layer under serve's follower: `sync::sync` of an unbound binding
/// appends nothing though its location grew (it is deferred as not
/// followed). Through serve this layer is never reached while the follower
/// drops inactive bindings (see the report), so it is tested here, against
/// the library, where nothing else stands in front of it. Opposed: the same
/// call on the same binding while it is bound appends what the location
/// gained.
#[test]
fn sync_of_an_unbound_binding_appends_nothing_though_its_location_grew() {
    sync_of_an_unbound_binding(Mode::Plain);
}

fn sync_of_an_unbound_binding(m: Mode) {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("store");
    let r = rollout(dir.path());
    m.prepare(&store);
    let mut st = protoledger::Store::open(&store).unwrap();
    let (row, _) = protoledger::sync::attach(&mut st, "codex", &r.loc).unwrap();
    m.retrain(&mut st);

    append(&r.loc, &r.parts[0]);
    let bound = protoledger::sync::sync(&mut st, &row.binding).unwrap();
    assert_eq!(bound.appended.iter().map(|p| p.event.id.clone()).collect::<Vec<_>>(), r.part_ids[0], "while bound");

    protoledger::sync::unbind(&mut st, &row.binding).unwrap();
    let before = st.agent(&row.agent).unwrap().unwrap().last_seq;
    append(&r.loc, &r.parts[1]);
    let unbound = protoledger::sync::sync(&mut st, &row.binding).unwrap();
    assert!(
        unbound.appended.is_empty() && unbound.created.is_empty() && unbound.diverged.is_none(),
        "an unbound binding was synced: {} appended {:?}",
        unbound.appended.len(),
        unbound.deferred
    );
    assert!(unbound.deferred.as_deref().is_some_and(|d| d.contains("not followed")), "{:?}", unbound.deferred);
    assert_eq!(st.agent(&row.agent).unwrap().unwrap().last_seq, before);
    drop(st);
    assert_eq!(ledger_ids(&store, &row.agent).len() as u64, before);
    m.check(&store);
}

modes_tests!(
    two_clients_from_different_seqs,
    reconnect_from_last_seq,
    http_endpoints,
    subscribe_all,
    unbind_through_serve,
    project_over_http,
    sync_of_an_unbound_binding,
);

// ---- loopback only by default ----

/// The process's exit status and stderr when serve refuses to start.
fn serve_refused(listen: &str) -> (std::process::ExitStatus, String) {
    let dir = tempfile::tempdir().unwrap();
    let mut child = bin()
        .args(["serve", "--store", dir.path().join("store").to_str().unwrap(), "--listen", listen, "--no-watcher"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let end = Instant::now() + PATIENCE;
    let status = loop {
        if let Some(st) = child.try_wait().unwrap() {
            break st;
        }
        if Instant::now() >= end {
            let _ = child.kill();
            let _ = child.wait();
            panic!("serve --listen {listen} did not refuse: it is still running");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let mut out = String::new();
    child.stdout.take().unwrap().read_to_string(&mut out).unwrap();
    assert!(!out.contains("listening on"), "{out}");
    // Refused before the store is opened: its directory was never created.
    assert!(dir.path().is_dir() && !dir.path().join("store").exists(), "a refused serve --listen {listen} created the store directory");
    let mut err = String::new();
    child.stderr.take().unwrap().read_to_string(&mut err).unwrap();
    (status, err)
}

/// A non-loopback address is refused, with the reason (no authentication)
/// and the way out (--allow-remote); 0.0.0.0 and :: alike.
#[test]
fn serve_refuses_a_non_loopback_listen_address_without_allow_remote() {
    for listen in ["0.0.0.0:0", "[::]:0"] {
        let (st, err) = serve_refused(listen);
        assert!(!st.success(), "{listen}: {st:?} {err}");
        assert!(err.contains("not a loopback address") && err.contains("no authentication") && err.contains("--allow-remote"), "{listen}: {err}");
    }
}

/// Opposed: loopback (127.0.0.1, [::1], localhost) is served without the
/// flag, and 0.0.0.0 is served with --allow-remote; each answers a request.
#[test]
fn serve_listens_on_loopback_without_the_flag_and_anywhere_with_allow_remote() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("store");
    for (listen, extra, prefix) in [
        ("127.0.0.1:0", &[][..], "127.0.0.1:"),
        ("[::1]:0", &[][..], "[::1]:"),
        ("localhost:0", &[][..], ""),
        ("0.0.0.0:0", &["--allow-remote"][..], "0.0.0.0:"),
    ] {
        let s = Serve::start_on(&store, listen, extra);
        assert!(s.addr.starts_with(prefix), "{listen}: {}", s.addr);
        // Opposes serve_refused's check: a server that starts creates its store.
        assert!(store.is_dir(), "{listen}: an accepted serve has no store directory");
        let a: std::net::SocketAddr = s.addr.parse().unwrap();
        if listen.starts_with("localhost") {
            assert!(a.ip().is_loopback(), "{listen}: {a}");
        }
        // Ask over loopback (0.0.0.0 is reached through 127.0.0.1).
        let ask = if a.ip().is_unspecified() { format!("127.0.0.1:{}", a.port()) } else { s.addr.clone() };
        let (code, v) = http(&ask, "GET", "/v1/agents", None);
        assert_eq!(code, 200, "{listen}: {v}");
        drop(s);
    }
}

// ---- a damaged ledger in the store ----

/// A store holding the rollout's head attached (agent and binding returned)
/// and the minimal protoledger fixture (`hello-1`); with `damaged`, the
/// attached agent's ledger is then damaged in place (a line that does not
/// parse), after the index read it.
fn store_with_a_damaged_attachment(dir: &Path, damaged: bool) -> (PathBuf, Rollout, String, String) {
    let store = dir.join("store");
    let r = rollout(dir);
    let mut st = protoledger::Store::open(&store).unwrap();
    let (row, _) = protoledger::sync::attach(&mut st, "codex", &r.loc).unwrap();
    st.import("protoledger", &fixture("protoledger/minimal/native")).unwrap();
    let path = store.join(&st.agent(&row.agent).unwrap().unwrap().path);
    drop(st);
    if damaged {
        let text = String::from_utf8(std::fs::read(&path).unwrap()).unwrap();
        let mut lines: Vec<&str> = text.lines().collect();
        lines[3] = "{this line is not json";
        std::fs::write(&path, lines.join("\n") + "\n").unwrap();
    }
    (store, r, row.binding, row.agent)
}

/// serve with a damaged ledger: it says so on stderr, once; a subscription
/// to every agent gets every other ledger's events and none of its; one
/// naming it is refused with 409, naming it, and so is every HTTP request
/// naming it (its row, its events, its children), with the same body; the
/// agent listing marks its row damaged and carries none of the counts the
/// index read before the damage. Opposed: the healthy agent beside it is
/// served (200, 101), requests that fail for any other reason keep their
/// own status (unknown agent 404, a bad `from` 400, an agent not in the
/// store yet 101); and healthy, the attached agent is served everywhere and
/// listed unmarked.
#[test]
fn serve_names_a_damaged_ledger_leaves_it_out_of_the_all_agents_feed_and_refuses_a_subscription_to_it() {
    for damaged in [true, false] {
        let dir = tempfile::tempdir().unwrap();
        let (store, r, _binding, agent) = store_with_a_damaged_attachment(dir.path(), damaged);
        // stderr: started, then stopped once it says where it listens.
        let mut child = bin()
            .args(["serve", "--store", store.to_str().unwrap(), "--listen", "127.0.0.1:0", "--no-watcher"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut line = String::new();
        BufReader::new(child.stdout.take().unwrap()).read_line(&mut line).unwrap();
        assert!(line.starts_with("listening on "), "{line:?}");
        let _ = child.kill();
        let _ = child.wait();
        let mut err = String::new();
        child.stderr.take().unwrap().read_to_string(&mut err).unwrap();
        let named = err.lines().filter(|l| l.contains("is damaged and was skipped") && l.contains(&format!("(agent {agent})"))).count();
        assert_eq!(named, usize::from(damaged), "damaged: {damaged}: {err}");

        let s = Serve::start(&store);
        let w = s.ws("all");
        let got = w.quiet(QUIET);
        let agents: std::collections::BTreeSet<&str> = got.iter().filter_map(|m| m["agent"].as_str()).collect();
        if damaged {
            assert_eq!(agents.into_iter().collect::<Vec<_>>(), ["hello-1"], "{:?}", summary(&got));
            let refused: Value = match tungstenite::connect(format!("ws://{}/v1/subscribe?from={agent}:0", s.addr)) {
                Ok(_) => panic!("a subscription to the damaged ledger was served"),
                Err(tungstenite::Error::Http(resp)) => {
                    let body = String::from_utf8_lossy(resp.body().as_deref().unwrap_or_default()).into_owned();
                    assert!(body.contains("damaged") && body.contains(&agent), "{} {body}", resp.status());
                    assert_eq!(resp.status().as_u16(), 409, "{body}");
                    serde_json::from_str(&body).unwrap()
                }
                Err(e) => panic!("{e}"),
            };
            // Every request naming it: 409, the subscription's body.
            for path in [format!("/v1/agents/{agent}"), format!("/v1/agents/{agent}/events"), format!("/v1/agents/{agent}/children")] {
                let (st, body) = s.get(&path);
                assert_eq!((st, &body), (409, &refused), "GET {path}");
            }
            let (st, body) = s.post("/v1/project", json!({"agent": agent, "format": "codex", "dir": dir.path().join("proj")}));
            assert_eq!((st, &body), (409, &refused), "POST /v1/project");
            // Import and attach that reach it: 409, naming it; any other refusal keeps 422.
            // The attach is of a copy of its rollout (with more of it) at a location not bound yet.
            let copy = dir.path().join("copy").join(r.loc.file_name().unwrap());
            std::fs::create_dir_all(copy.parent().unwrap()).unwrap();
            std::fs::write(&copy, r.head.clone() + &r.parts[0]).unwrap();
            for (path, req) in [("/v1/import", json!({"format": "codex", "path": r.loc})), ("/v1/attach", json!({"format": "codex", "location": copy}))] {
                let (st, body) = s.post(path, req);
                let msg = body["error"].as_str().unwrap_or_default();
                assert!(st == 409 && msg.contains("is damaged") && msg.contains(&agent), "POST {path}: {st} {body}");
            }
            let (st, body) = s.post("/v1/import", json!({"format": "no-such-format", "path": r.loc}));
            assert_eq!(st, 422, "{body}");
            let (st, body) = s.post("/v1/attach", json!({"format": "codex", "location": r.loc}));
            assert!(st == 422 && body["error"].as_str().unwrap_or_default().contains("already bound"), "{st} {body}");
            // The listing marks it, without the stale counts.
            let (st, list) = s.get("/v1/agents");
            assert_eq!(st, 200, "{list}");
            let row = list.as_array().unwrap().iter().find(|a| a["id"] == agent.as_str()).unwrap_or_else(|| panic!("not listed: {list}"));
            let mark = row["damaged"].as_str().unwrap_or_else(|| panic!("not marked: {row}"));
            assert!(mark.contains("damaged") && mark.contains(&agent), "{row}");
            assert!(row.get("last_seq").is_none() && row.get("byte_len").is_none() && row.get("size").is_none(), "stale counts: {row}");
            let hello = list.as_array().unwrap().iter().find(|a| a["id"] == "hello-1").unwrap();
            assert!(hello.get("damaged").is_none() && hello["last_seq"] == 2, "{hello}");
            // Beside it: the healthy agent is served, and other failures keep their own status.
            assert_eq!(s.get("/v1/agents/hello-1").0, 200);
            let (st, evs) = s.get("/v1/agents/hello-1/events");
            assert_eq!((st, evs.as_array().map(Vec::len)), (200, Some(2)), "{evs}");
            assert_eq!(s.get("/v1/agents/nobody").0, 404);
            assert_eq!(s.get("/v1/agents/nobody/events").0, 404);
            match tungstenite::connect(format!("ws://{}/v1/subscribe?from={agent}", s.addr)) {
                Err(tungstenite::Error::Http(resp)) => assert_eq!(resp.status().as_u16(), 400),
                other => panic!("a bad from: {:?}", other.map(|(_, r)| r.status())),
            }
            let one = s.ws("hello-1:0");
            assert_eq!(seqs(&one.take(2)), [1, 2]);
            one.close();
            s.ws("nobody:0").close();
        } else {
            assert!(agents.contains("hello-1") && agents.contains(agent.as_str()), "{:?}", summary(&got));
            let one = s.ws(&format!("{agent}:0"));
            assert!(!one.take(1).is_empty());
            one.close();
            let (st, a) = s.get(&format!("/v1/agents/{agent}"));
            assert!(st == 200 && a.get("damaged").is_none() && a["last_seq"].as_u64() > Some(0), "{st} {a}");
            let (st, evs) = s.get(&format!("/v1/agents/{agent}/events"));
            assert!(st == 200 && !evs.as_array().unwrap().is_empty(), "{st} {evs}");
            assert_eq!(s.get(&format!("/v1/agents/{agent}/children")).0, 200);
            let (_, list) = s.get("/v1/agents");
            assert!(list.as_array().unwrap().iter().all(|a| a.get("damaged").is_none() && a["last_seq"].as_u64().is_some()), "{list}");
        }
        w.close();
    }
}

/// Sync with a damaged ledger bound: the binding is not read (deferred,
/// naming the ledger) and nothing fails; the ledger is left as it was.
/// Opposed: healthy, the same sync appends what the location gained.
#[test]
fn sync_of_a_binding_whose_ledger_is_damaged_defers_it_naming_it_and_stops_nothing() {
    for damaged in [true, false] {
        let dir = tempfile::tempdir().unwrap();
        let (store, r, binding, agent) = store_with_a_damaged_attachment(dir.path(), damaged);
        let mut st = protoledger::Store::open(&store).unwrap();
        let path = store.join(&st.agent(&agent).unwrap().unwrap().path);
        let before = std::fs::read(&path).unwrap();
        append(&r.loc, &r.parts[0]);
        let all = protoledger::sync::sync_all(&mut st).unwrap_or_else(|e| panic!("damaged: {damaged}: sync_all failed: {e:#}"));
        let s = all.iter().find(|s| s.binding == binding).expect("the binding was synced");
        if damaged {
            let d = s.deferred.as_deref().unwrap_or_else(|| panic!("not deferred: {s:?}"));
            assert!(d.contains("damaged") && d.contains(path.file_name().unwrap().to_str().unwrap()), "{d}");
            assert!(s.appended.is_empty(), "{s:?}");
            drop(st);
            assert!(std::fs::read(&path).unwrap() == before, "the damaged ledger changed");
        } else {
            assert!(s.deferred.is_none(), "{s:?}");
            let ids: Vec<String> = s.appended.iter().map(|p| p.event.id.clone()).collect();
            assert_eq!(ids, r.part_ids[0], "{s:?}");
        }
    }
}

/// An import or attach that reaches a damaged ledger is refused naming the
/// damage, whether the index still holds the ledger's rows or not (`store
/// reindex` drops them; the file and its readable header are still there):
/// the CLI's error says so, and serve answers 409. Opposed: a new agent is
/// still created beside it (200); creating an agent a healthy ledger already
/// holds keeps its own refusal; and healthy, the same import and attach
/// succeed.
#[test]
fn import_and_attach_reaching_a_damaged_ledger_are_refused_by_name_indexed_or_not() {
    for (damaged, reindexed) in [(true, false), (true, true), (false, false)] {
        let case = format!("damaged: {damaged}, reindexed: {reindexed}");
        let dir = tempfile::tempdir().unwrap();
        let (store, r, _binding, agent) = store_with_a_damaged_attachment(dir.path(), damaged);
        let st = store.to_str().unwrap();
        if reindexed {
            run(&["store", "reindex", st]);
            let s = protoledger::Store::open(&store).unwrap();
            assert!(s.agent(&agent).unwrap().is_none(), "{case}: the reindex kept its row");
            assert!(s.damaged_agent(&agent).is_some(), "{case}: not named damaged");
        }
        // A copy of its rollout, with more of it, at a location not bound yet.
        let copy = dir.path().join("copy").join(r.loc.file_name().unwrap());
        std::fs::create_dir_all(copy.parent().unwrap()).unwrap();
        std::fs::write(&copy, r.head.clone() + &r.parts[0]).unwrap();
        let copy2 = dir.path().join("copy2").join(r.loc.file_name().unwrap());
        std::fs::create_dir_all(copy2.parent().unwrap()).unwrap();
        std::fs::write(&copy2, r.head.clone() + &r.parts[0]).unwrap();

        // The CLI: its error (not the line naming what the store skipped) names the damage.
        let out = bin().args(["store", "import", st, "codex", copy.to_str().unwrap()]).output().unwrap();
        let err = String::from_utf8_lossy(&out.stderr).into_owned();
        let error = err.split_once("Error:").map(|(_, e)| e.to_string()).unwrap_or_default();
        if damaged {
            assert!(!out.status.success(), "{case}: store import succeeded");
            assert!(error.contains("is damaged") && error.contains(&agent) && !error.contains("already exists"), "{case}: store import: {err}");
        } else {
            assert!(out.status.success(), "{case}: store import: {err}");
        }

        // Library: an agent a healthy ledger holds keeps its own refusal.
        {
            let mut s = protoledger::Store::open(&store).unwrap();
            let id = "hello-1";
            let e = s
                .create_agent(protoledger::model::Agent {
                    id: id.into(),
                    created_at: None,
                    source: None,
                    parent: None,
                    links: vec![],
                    meta: Default::default(),
                })
                .unwrap_err();
            assert!(e.downcast_ref::<protoledger::store::DamagedLedger>().is_none() && format!("{e:#}").contains("already in the store"), "{case}: {e:#}");
        }

        let s = Serve::start(&store);
        for (path, req) in [("/v1/import", json!({"format": "codex", "path": copy})), ("/v1/attach", json!({"format": "codex", "location": copy2}))] {
            let (status, body) = s.post(path, req);
            let msg = body["error"].as_str().unwrap_or_default();
            if damaged {
                assert!(status == 409 && msg.contains("is damaged") && msg.contains(&agent), "{case}: POST {path}: {status} {body}");
            } else {
                assert_eq!(status, 200, "{case}: POST {path}: {body}");
            }
        }
        // A new agent beside it is created.
        let (status, body) = s.post("/v1/import", json!({"format": "protoledger", "path": fixture("protoledger/native-subagent-compaction/native")}));
        assert_eq!(status, 200, "{case}: {body}");
    }
}
