//! Harbor's Agent Trajectory Interchange Format (ATIF), schema versions
//! `ATIF-v1.0` through `ATIF-v1.8`.
//!
//! A trajectory is one JSON document: a root object (`agent`, `notes`,
//! `final_metrics`, `extra`, ...) with an ordered `steps` array and, since
//! v1.7, embedded `subagent_trajectories`. A Harbor run directory may also
//! hold several documents that reference each other by path (summarisation
//! subagents, `trajectory.cont-1.json` continuations), plus image files.
//!
//! Mapping, carry and losses are documented in `docs/formats/atif.md`.

use crate::formats::{Detection, ExportOptions, Exported, Format, ImportOptions, Imported, Variant};
use crate::ledger::LedgerSet;
use anyhow::Result;
use serde_json::Value;
use std::path::Path;

mod convert;
mod export;
mod import;
pub mod jsonfmt;

pub const FORMAT: &str = "atif";
/// The version written for ledgers that did not come from ATIF.
pub const DEFAULT_EXPORT: Ver = Ver(1, 7);
pub const LATEST: Ver = Ver(1, 8);

/// A schema version, `ATIF-v<major>.<minor>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Ver(pub u32, pub u32);

impl Ver {
    /// Accepts `ATIF-v1.7`, `v1.7` and `1.7`.
    pub fn parse(s: &str) -> Option<Ver> {
        let t = s.trim();
        let t = t.strip_prefix("ATIF-").unwrap_or(t);
        let t = t.strip_prefix('v').or_else(|| t.strip_prefix('V')).unwrap_or(t);
        let (a, b) = t.split_once('.')?;
        Some(Ver(a.parse().ok()?, b.parse().ok()?))
    }
    pub fn name(self) -> String {
        format!("ATIF-v{}.{}", self.0, self.1)
    }
    pub fn at_least(self, major: u32, minor: u32) -> bool {
        self >= Ver(major, minor)
    }
}

static VARIANTS: [Variant; 9] = [
    Variant {
        name: "ATIF-v1.0",
        versions: "schema_version ATIF-v1.0",
        import: true,
        export: true,
        notes: "Initial release: agent, steps (system/user/agent), tool_calls, observation, metrics, final_metrics.",
    },
    Variant { name: "ATIF-v1.1", versions: "schema_version ATIF-v1.1", import: true, export: true, notes: "Adds root-level `extra`." },
    Variant {
        name: "ATIF-v1.2",
        versions: "schema_version ATIF-v1.2 (Harbor goose, hermes, cortex-code converters)",
        import: true,
        export: true,
        notes: "System steps may carry an observation (subagent delegation, context management).",
    },
    Variant { name: "ATIF-v1.3", versions: "schema_version ATIF-v1.3", import: true, export: true, notes: "Adds metrics.completion_token_ids." },
    Variant { name: "ATIF-v1.4", versions: "schema_version ATIF-v1.4", import: true, export: true, notes: "Adds metrics.prompt_token_ids." },
    Variant {
        name: "ATIF-v1.5",
        versions: "schema_version ATIF-v1.5 (Harbor openhands, swe-agent, rovodev converters)",
        import: true,
        export: true,
        notes: "Adds agent.tool_definitions and step.is_copied_context.",
    },
    Variant {
        name: "ATIF-v1.6",
        versions: "schema_version ATIF-v1.6 (Harbor terminus-2, acp, cline, kimi-cli, trae converters)",
        import: true,
        export: true,
        notes: "Multimodal: message and observation content may be ContentPart arrays with image parts (files referenced by path).",
    },
    Variant {
        name: "ATIF-v1.7",
        versions: "schema_version ATIF-v1.7 (most Harbor converters); default export",
        import: true,
        export: true,
        notes: "Adds subagent_trajectories (embedded) and trajectory_id, tool_call/observation-result extra, llm_call_count, the context_management convention; session_id optional; refs resolve by trajectory_id or trajectory_path.",
    },
    Variant {
        name: "ATIF-v1.8",
        versions: "schema_version ATIF-v1.8 (Harbor main, unreleased at the time of writing)",
        import: true,
        export: true,
        notes: "Adds audio content parts. Export with --variant ATIF-v1.8.",
    },
];

/// The static variant name for a version, falling back to the nearest one.
pub fn variant_name(v: Ver) -> &'static str {
    VARIANTS.iter().rev().find(|x| Ver::parse(x.name).is_some_and(|y| y <= v)).map(|x| x.name).unwrap_or(VARIANTS[0].name)
}

pub struct Atif;

impl Format for Atif {
    fn name(&self) -> &'static str {
        FORMAT
    }
    fn describe(&self) -> &'static str {
        "Harbor ATIF trajectory JSON (Agent Trajectory Interchange Format), v1.0-v1.8"
    }
    fn variants(&self) -> &'static [Variant] {
        &VARIANTS
    }
    fn detect(&self, input: &Path) -> Option<Detection> {
        if input.is_file() {
            let doc = read_doc(input)?;
            let v = doc_version(&doc)?;
            return Some(Detection { variant: variant_name(v), confidence: 95, version: agent_version(&doc) });
        }
        if input.is_dir() {
            let files = import::atif_files(input);
            let main = files.iter().find(|(p, _, _)| p == "trajectory.json").or_else(|| files.first())?;
            let v = doc_version(&main.2)?;
            return Some(Detection { variant: variant_name(v), confidence: 90, version: agent_version(&main.2) });
        }
        None
    }
    fn import(&self, input: &Path, opts: &ImportOptions) -> Result<Imported> {
        // SPEC §3.1.6: user or assistant text holding the peer-message wrapper is that peer message.
        let mut im = import_records(input, opts)?;
        crate::formats::pi_session::unwrap_peers(&mut im.set, self.name());
        Ok(im)
    }
    fn export(&self, set: &LedgerSet, opts: &ExportOptions) -> Result<Exported> {
        export::export(set, opts)
    }
}

/// What [`Format::import`] reads before peer messages are unwrapped.
fn import_records(input: &Path, opts: &ImportOptions) -> Result<Imported> {
    import::import(input, opts)
}

fn read_doc(p: &Path) -> Option<Value> {
    if p.extension().is_some_and(|x| x != "json") {
        return None;
    }
    let text = std::fs::read_to_string(p).ok()?;
    if !text.contains("schema_version") {
        return None;
    }
    let v: Value = serde_json::from_str(&text).ok()?;
    doc_version(&v)?;
    Some(v)
}

/// The document's schema version, if it is an ATIF trajectory.
pub fn doc_version(doc: &Value) -> Option<Ver> {
    let s = doc.get("schema_version")?.as_str()?;
    if !s.starts_with("ATIF-") {
        return None;
    }
    Ver::parse(s)
}

fn agent_version(doc: &Value) -> Option<String> {
    doc.get("agent")?.get("version")?.as_str().map(str::to_string)
}

/// ATIF image media types.
pub const IMAGE_TYPES: [&str; 4] = ["image/jpeg", "image/png", "image/gif", "image/webp"];
/// ATIF audio media types (v1.8), after alias normalisation.
pub const AUDIO_TYPES: [&str; 8] = ["audio/wav", "audio/mpeg", "audio/mp4", "audio/aac", "audio/ogg", "audio/flac", "audio/webm", "audio/aiff"];

pub fn normalise_audio(mt: &str) -> String {
    let m = mt.trim().to_ascii_lowercase();
    match m.as_str() {
        "audio/mp3" | "audio/mpga" | "audio/x-mpeg" => "audio/mpeg".into(),
        "audio/x-wav" | "audio/wave" | "audio/vnd.wave" => "audio/wav".into(),
        "audio/x-m4a" | "audio/m4a" => "audio/mp4".into(),
        "audio/x-aac" => "audio/aac".into(),
        "audio/x-flac" => "audio/flac".into(),
        "audio/x-aiff" => "audio/aiff".into(),
        _ => m,
    }
}

pub fn extension_for(mt: &str) -> &'static str {
    match mt {
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "audio/wav" => "wav",
        "audio/mpeg" => "mp3",
        "audio/mp4" => "m4a",
        "audio/aac" => "aac",
        "audio/ogg" => "ogg",
        "audio/flac" => "flac",
        "audio/webm" => "webm",
        "audio/aiff" => "aiff",
        _ => "bin",
    }
}

/// Is this media `path` a reference to something outside the trajectory's
/// directory (a URL or an absolute path)?
pub fn is_external(path: &str) -> bool {
    path.contains("://") || path.starts_with('/') || path.starts_with("data:")
}

/// Join `rel` onto the directory of `file` (both relative, `/`-separated),
/// resolving `.` and `..`. `None` if it escapes the root or is external.
pub fn join_rel(file: &str, rel: &str) -> Option<String> {
    if is_external(rel) {
        return None;
    }
    let mut parts: Vec<&str> = file.split('/').collect();
    parts.pop();
    for seg in rel.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            s => parts.push(s),
        }
    }
    Some(parts.join("/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_parse_and_order() {
        assert_eq!(Ver::parse("ATIF-v1.7"), Some(Ver(1, 7)));
        assert_eq!(Ver::parse("1.2"), Some(Ver(1, 2)));
        assert_eq!(Ver::parse("v1.10"), Some(Ver(1, 10)));
        assert!(Ver(1, 10) > Ver(1, 8));
        assert_eq!(variant_name(Ver(1, 9)), "ATIF-v1.8");
        assert_eq!(variant_name(Ver(1, 5)), "ATIF-v1.5");
    }

    #[test]
    fn relative_paths() {
        assert_eq!(join_rel("trajectory.json", "images/a.png").as_deref(), Some("images/a.png"));
        assert_eq!(join_rel("sub/t.json", "../x.json").as_deref(), Some("x.json"));
        assert_eq!(join_rel("t.json", "../x.json"), None);
        assert_eq!(join_rel("t.json", "https://x/y.png"), None);
    }
}
