//! `protoledger project|attach|unbind|watch`, end to end through the binary.
#![cfg(all(feature = "sync", feature = "cli"))]

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::Duration;

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_protoledger"))
}

fn run(args: &[&str]) -> Output {
    let out = bin().args(args).output().unwrap();
    assert!(out.status.success(), "protoledger {args:?} failed: {}", String::from_utf8_lossy(&out.stderr));
    out
}

fn fixture(p: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures").join(p)
}

/// `watch`'s poll period in this test.
const POLL_MS: u64 = 100;
/// After the expected lines, how long the test keeps reading for more: ten
/// poll periods, so a duplicate printed by any later poll is seen.
const QUIET_MS: u64 = 10 * POLL_MS;

#[test]
fn project_attach_watch_unbind_then_nothing_more_for_10_polls_of_100ms() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("s");
    let st = store.to_str().unwrap();
    run(&["store", "import", st, "protoledger", fixture("protoledger/minimal/native").to_str().unwrap()]);

    // project prints the binding id; projecting onto the same files again is refused.
    let cc = dir.path().join("cc");
    let out = run(&["project", st, "hello-1", "claude-code", cc.to_str().unwrap()]);
    let projected = String::from_utf8(out.stdout).unwrap().trim().to_string();
    assert_eq!(projected.len(), 36, "{projected:?}");
    assert!(!bin().args(["project", st, "hello-1", "claude-code", cc.to_str().unwrap()]).output().unwrap().status.success());

    // attach a codex rollout with its tail held back.
    let src = fixture("codex/legacy-0.142-compaction-real/native/rollout-2026-06-26T23-11-51-019f0634-3175-7702-b55c-e256f2966840.jsonl");
    let text = std::fs::read_to_string(&src).unwrap();
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let (head, rest) = lines.split_at(12);
    std::fs::create_dir_all(dir.path().join("codex")).unwrap();
    let loc = dir.path().join("codex").join(src.file_name().unwrap());
    std::fs::write(&loc, head.concat()).unwrap();
    // What the tail makes, by the importer alone: the whole file's ids past the head's.
    let f = protoledger::formats::by_name("codex").unwrap();
    let opts = protoledger::formats::ImportOptions::standard();
    let ids = |p: &Path| -> Vec<String> { f.import(p, &opts).unwrap().set.iter().next().unwrap().events.iter().map(|e| e.id.clone()).collect() };
    let head_ids = ids(&loc);
    let whole = ids(&src);
    assert_eq!(whole[..head_ids.len()], head_ids[..]);
    let expected = whole[head_ids.len()..].to_vec();
    assert!(!expected.is_empty());
    let out = run(&["attach", st, "codex", loc.to_str().unwrap()]);
    let attached = String::from_utf8(out.stdout).unwrap().trim().to_string();
    assert_ne!(attached, projected);

    // watch prints each appended event as a JSON line; poll only.
    let poll = POLL_MS.to_string();
    let mut child = bin().args(["watch", st, "--no-watcher", "--poll-ms", poll.as_str()]).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    let stdout = child.stdout.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            if tx.send(line.unwrap()).is_err() {
                break;
            }
        }
    });
    // Ready when it says so: `watch` writes "watching N bindings" to stderr
    // once its catch-up sync is done and its follower is set up, so an append
    // after that is the poll's to read.
    let stderr = child.stderr.take().unwrap();
    let (etx, erx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines() {
            if etx.send(line.unwrap()).is_err() {
                break;
            }
        }
    });
    let ready = loop {
        match erx.recv_timeout(Duration::from_secs(30)) {
            Ok(l) if l.starts_with("watching ") => break l,
            Ok(_) => continue,
            Err(e) => panic!("watch never said it was ready: {e}"),
        }
    };
    assert_eq!(ready.split(' ').nth(1), Some("2"), "{ready}");
    std::fs::OpenOptions::new().append(true).open(&loc).unwrap().write_all(rest.concat().as_bytes()).unwrap();
    let mut got = Vec::new();
    while got.len() < expected.len() {
        let Ok(line) = rx.recv_timeout(Duration::from_secs(10)) else { break };
        got.push(line);
    }
    // Keep reading past the count: anything more within ten polls is a duplicate.
    while let Ok(line) = rx.recv_timeout(Duration::from_millis(QUIET_MS)) {
        got.push(line);
    }
    child.kill().unwrap();
    child.wait().unwrap();
    let mut ids = Vec::new();
    for line in &got {
        let v: serde_json::Value = serde_json::from_str(line).unwrap();
        assert_eq!((v["binding"].as_str(), v["trigger"].as_str()), (Some(attached.as_str()), Some("poll")), "{line}");
        ids.push(v["event"]["id"].as_str().unwrap().to_string());
    }
    assert_eq!(ids, expected, "the tail's events, once each, in order, and nothing more for {QUIET_MS} ms");

    // unbind; unbinding again is refused.
    run(&["unbind", st, &attached]);
    assert!(!bin().args(["unbind", st, &attached]).output().unwrap().status.success());
}
