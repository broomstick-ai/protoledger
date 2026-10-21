//! `protoledger store init|ls|show|reindex|import`, end to end through the binary.
#![cfg(all(feature = "store", feature = "cli"))]

use std::path::Path;
use std::process::{Command, Output};

fn run(args: &[&str]) -> Output {
    let out = Command::new(env!("CARGO_BIN_EXE_protoledger")).args(args).output().unwrap();
    assert!(out.status.success(), "protoledger {args:?} failed: {}", String::from_utf8_lossy(&out.stderr));
    out
}

fn stdout(args: &[&str]) -> String {
    String::from_utf8(run(args).stdout).unwrap()
}

#[test]
fn store_cli_init_import_ls_show_reindex() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("s");
    let st = store.to_str().unwrap();
    run(&["store", "init", st]);
    assert!(store.join("ledgers").is_dir() && store.join("index.db").is_file());
    assert_eq!(stdout(&["store", "ls", st]), "");

    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/protoledger/orchestrated-peers/native");
    run(&["store", "import", st, "protoledger", src.to_str().unwrap()]);
    let ls = stdout(&["store", "ls", st]);
    let ids: Vec<_> = ls.lines().map(|l| l.split('\t').next().unwrap()).collect();
    assert_eq!(ids, ["coord-1", "peer-a", "peer-b"]);

    // `show` prints the agent's events exactly as its ledger holds them.
    let ledger = std::fs::read_to_string(src.join("peer-a.jsonl")).unwrap();
    let events: String = ledger.lines().skip(1).map(|l| format!("{l}\n")).collect();
    assert!(!events.is_empty());
    assert_eq!(stdout(&["store", "show", st, "peer-a"]), events);
    let second = ledger.lines().nth(2).unwrap();
    assert_eq!(stdout(&["store", "show", st, "peer-a", "--from", "2", "--limit", "1"]), format!("{second}\n"));
    let missing = Command::new(env!("CARGO_BIN_EXE_protoledger")).args(["store", "show", st, "nobody"]).output().unwrap();
    assert!(!missing.status.success());

    // Delete index.db, reindex, and ls/show answer as before.
    for n in ["index.db", "index.db-wal", "index.db-shm"] {
        let _ = std::fs::remove_file(store.join(n));
    }
    assert!(!store.join("index.db").exists());
    run(&["store", "reindex", st]);
    assert_eq!(stdout(&["store", "ls", st]), ls);
    assert_eq!(stdout(&["store", "show", st, "peer-a"]), events);
}

/// A ledger another program wrote: spaced JSON a re-serialization would
/// change, raw non-ASCII, and one line longer than 64 KiB.
fn planted_ledger() -> String {
    let long = "y".repeat(150_000);
    format!(
        "{{\"protoledger\": \"0.1\", \"agent\": {{\"id\": \"odd\"}}}}\n\
         {{\"id\": \"odd:1\", \"seq\": 1, \"kind\": \"text\", \"body\": {{\"role\": \"user\", \"text\": \"naïve café — 日本語 🦀\"}}}}\n\
         {{\"seq\":2,\"id\":\"odd:2\",\"kind\":\"text\",\"body\":{{\"text\":\"{long}\",\"role\":\"assistant\"}}}}\n\
         {{\"id\": \"odd:3\", \"seq\": 3, \"kind\": \"text\", \"body\": {{\"role\": \"user\", \"text\": \"\\u00e9 escaped\"}}}}\n"
    )
}

#[test]
fn cat_writes_each_ledger_byte_for_byte() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("s");
    let st = store.to_str().unwrap();
    for case in ["orchestrated-peers", "branch-rewind", "native-subagent-compaction"] {
        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/protoledger").join(case).join("native");
        run(&["store", "import", st, "protoledger", src.to_str().unwrap()]);
    }
    std::fs::write(store.join("ledgers/odd.jsonl"), planted_ledger()).unwrap();
    let ls = stdout(&["store", "ls", st]);
    let ids: Vec<&str> = ls.lines().map(|l| l.split('\t').next().unwrap()).collect();
    assert!(ids.len() >= 7 && ids.contains(&"odd"), "{ids:?}");
    for id in ids {
        let file = std::fs::read(store.join("ledgers").join(format!("{id}.jsonl"))).unwrap();
        assert!(file.ends_with(b"\n"));
        let out = run(&["cat", st, id]).stdout;
        assert!(out == file, "cat {id}: {} bytes, the ledger file holds {}", out.len(), file.len());
    }
    let odd = run(&["cat", st, "odd"]).stdout;
    assert!(odd == planted_ledger().into_bytes(), "cat odd: {} bytes", odd.len());
    assert!(odd.len() > 150_000 && String::from_utf8(odd).unwrap().contains("日本語 🦀"));
}

#[test]
fn cat_leaves_out_a_torn_final_line() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("s");
    let st = store.to_str().unwrap();
    run(&["store", "init", st]);
    let torn = "{\"id\": \"odd:4\", \"seq\": 4, \"ki";
    std::fs::write(store.join("ledgers/odd.jsonl"), format!("{}{torn}", planted_ledger())).unwrap();
    let out = run(&["cat", st, "odd"]).stdout;
    assert!(out == planted_ledger().into_bytes(), "cat odd: {} bytes, its complete lines hold {}", out.len(), planted_ledger().len());
}

#[test]
fn cat_of_an_agent_the_store_does_not_hold_fails_naming_it() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("s");
    let st = store.to_str().unwrap();
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/protoledger/minimal/native");
    run(&["store", "import", st, "protoledger", src.to_str().unwrap()]);
    let out = Command::new(env!("CARGO_BIN_EXE_protoledger")).args(["cat", st, "nobody-here"]).output().unwrap();
    assert!(!out.status.success());
    assert!(out.stdout.is_empty());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("nobody-here"), "{err}");
}
