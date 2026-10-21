//! Claude Code session transcripts (`~/.claude/projects/<cwd-slug>/<session>.jsonl`).
//!
//! See `docs/formats/claude-code.md` for the record → kind mapping, the loss
//! codes, the variants and what has been verified against a real `claude`.
//!
//! - `import` reads a session file, a project directory (sessions plus their
//!   `<session>/subagents/` and `<session>/tool-results/` sidecars, or the
//!   older flat `agent-<id>.jsonl` files), or a whole `projects/` root.
//! - `export` writes every native record back byte for byte, and renders
//!   foreign events into lines Claude Code accepts for `--resume`.

use crate::formats::{Detection, ExportOptions, Exported, Follow, Format, ImportOptions, Imported, Variant};
use crate::ledger::LedgerSet;
use anyhow::Result;
use serde_json::Value;
use std::path::Path;

mod export;
mod import;
#[cfg(test)]
mod tests;

pub const FORMAT: &str = "claude-code";

/// The producer version foreign exports impersonate unless told otherwise:
/// the newest version the export was verified against with `claude --resume`.
pub const DEFAULT_VERSION: &str = "2.1.280";

/// The timestamp written on a line whose time is unknown, read back as unknown.
const UNKNOWN_TS: &str = "1970-01-01T00:00:00.000Z";

pub const V_SIDECAR: &str = "jsonl-sidecar";
pub const V_FLAT: &str = "jsonl-flat-agents";
pub const V_INLINE: &str = "jsonl-inline-sidechain";

static VARIANTS: [Variant; 3] = [
    Variant {
        name: V_SIDECAR,
        versions: ">= 2.1.3 (verified 2.1.3 – 2.1.280)",
        import: true,
        export: true,
        notes: "Subagents in <session>/subagents/agent-<id>.jsonl (+ .meta.json from ~2.1.30), large tool outputs in \
                <session>/tool-results/. Foreign ledgers are always exported in this shape.",
    },
    Variant {
        name: V_FLAT,
        versions: ">= 2.0.30, < 2.1.3 (verified 2.0.70, 2.0.76, 2.1.1)",
        import: true,
        export: true,
        notes: "Subagents (and the startup 'Warmup' agents) as agent-<id>.jsonl beside the session files, lines carry \
                agentId; `summary` title lines. Native records are re-emitted to their original files.",
    },
    Variant {
        name: V_INLINE,
        versions: "< 2.0.30 (verified 1.0.69 – 1.0.83, 2.0.22)",
        import: true,
        export: true,
        notes: "Sidechain (subagent) lines inline in the session file with isSidechain:true and no agentId. Each \
                sidechain becomes its own ledger and is re-interleaved on export.",
    },
];

pub struct ClaudeCode;

impl Format for ClaudeCode {
    fn name(&self) -> &'static str {
        FORMAT
    }
    fn describe(&self) -> &'static str {
        "Claude Code session transcripts (~/.claude/projects/<slug>/<session>.jsonl and sidecars)"
    }
    fn variants(&self) -> &'static [Variant] {
        &VARIANTS
    }
    fn detect(&self, input: &Path) -> Option<Detection> {
        import::detect(input)
    }
    fn import(&self, input: &Path, opts: &ImportOptions) -> Result<Imported> {
        import::import(input, opts)
    }
    fn export(&self, set: &LedgerSet, opts: &ExportOptions) -> Result<Exported> {
        export::export(set, opts)
    }
    fn follow(&self, input: &Path, opts: &ImportOptions) -> Option<Result<(Imported, Box<dyn Follow>)>> {
        import::follow(input, opts)
    }
    fn weight(&self) -> u8 {
        3
    }
}

// ---- helpers shared by import and export ----

fn s<'a>(v: &'a Value, k: &str) -> Option<&'a str> {
    v.get(k).and_then(Value::as_str)
}

/// The project-directory slug Claude Code files a session under: every
/// character of the cwd that is not ASCII alphanumeric becomes `-`.
pub fn slug_for(cwd: &str) -> String {
    cwd.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect()
}

/// Compare dotted versions numerically (`2.1.10` > `2.1.9`); non-numeric parts compare as 0.
fn version_lt(a: &str, b: &str) -> bool {
    let p = |v: &str| v.split(['.', '-']).map(|x| x.parse::<u64>().unwrap_or(0)).collect::<Vec<_>>();
    p(a) < p(b)
}

/// The variant a producer version most likely wrote, for files with no
/// layout evidence of their own.
fn variant_for_version(v: &str) -> &'static str {
    if version_lt(v, "2.0.30") {
        V_INLINE
    } else if version_lt(v, "2.1.3") {
        V_FLAT
    } else {
        V_SIDECAR
    }
}

/// An incoming peer message as the user text SPEC §3.1.6 prescribes for a
/// harness without a peer channel. Absent attributes are omitted.
fn peer_wrap(peer: Option<&str>, network: Option<&str>, text: &str) -> String {
    let mut open = String::from("<peer-message direction=\"in\"");
    if let Some(p) = peer {
        open.push_str(&format!(" peer=\"{p}\""));
    }
    if let Some(n) = network {
        open.push_str(&format!(" network=\"{n}\""));
    }
    format!("{open}>\n{text}\n</peer-message>")
}

/// The inverse of [`peer_wrap`]: `(peer, network, text)` when `t` is exactly
/// one wrapped incoming peer message.
fn peer_unwrap(t: &str) -> Option<(Option<String>, Option<String>, String)> {
    let rest = t.strip_prefix("<peer-message direction=\"in\"")?;
    let (attrs, body) = rest.split_once(">\n")?;
    let text = body.strip_suffix("\n</peer-message>")?;
    let mut peer = None;
    let mut network = None;
    let mut a = attrs;
    while !a.is_empty() {
        let (k, v) = a.strip_prefix(' ')?.split_once("=\"")?;
        let (v, after) = v.split_once('"')?;
        match k {
            "peer" if peer.is_none() && network.is_none() => peer = Some(v.to_string()),
            "network" if network.is_none() => network = Some(v.to_string()),
            _ => return None,
        }
        a = after;
    }
    // Only what peer_wrap itself would have written.
    (peer_wrap(peer.as_deref(), network.as_deref(), text) == t).then(|| (peer, network, text.to_string()))
}

/// A tool result whose call is not in the history, as the user text the
/// exporter writes for it (the API refuses an unanswered `tool_result`).
fn orphan_result_wrap(call: &str, output: &str) -> String {
    format!("[tool result {call}]\n{output}")
}

/// The inverse of [`orphan_result_wrap`]: `(call id, output)`.
fn orphan_result_unwrap(t: &str) -> Option<(String, String)> {
    let rest = t.strip_prefix("[tool result ")?;
    let (call, output) = rest.split_once("]\n")?;
    (!call.contains(['\n', ']'])).then(|| (call.to_string(), output.to_string()))
}
