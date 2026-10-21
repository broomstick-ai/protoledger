//! Moving one ATIF document between schema versions.
//!
//! Every field ATIF added after v1.0 is listed here with the version that
//! introduced it. Converting down removes what the target cannot hold and
//! names each removal as a loss; converting up only rewrites what changed
//! meaning (v1.7 made subagent refs resolve by `trajectory_id`).

use super::Ver;
use crate::formats::Losses;
use crate::model::Kind;
use crate::util::OrderedMap;
use serde_json::{Map, Value, json};

/// Rewrite `doc` (not its embedded subagents, which are converted on their
/// own) to schema `to`. `id` is used when a version requires a session_id.
pub fn convert(doc: &mut Value, to: Ver, id: &str, losses: &mut Losses) {
    let Some(o) = doc.as_object_mut() else { return };
    o.insert("schema_version".into(), json!(to.name()));
    if !to.at_least(1, 7) {
        if let Some(t) = o.remove_ordered("trajectory_id")
            && o.get("session_id").is_none_or(Value::is_null)
            && !t.is_null()
        {
            insert_after(o, "schema_version", "session_id", t);
        }
        if o.get("subagent_trajectories").is_some_and(|v| v.as_array().is_none_or(|a| a.is_empty()) || v.is_null()) {
            o.remove_ordered("subagent_trajectories");
        }
        if o.get("session_id").is_none_or(Value::is_null) {
            insert_after(o, "schema_version", "session_id", json!(id));
        }
    }
    if !to.at_least(1, 1)
        && let Some(x) = o.remove_ordered("extra")
        && x.as_object().is_some_and(|m| !m.is_empty())
    {
        losses.add("extra-dropped", None, "root extra needs ATIF-v1.1");
    }
    if !to.at_least(1, 5)
        && let Some(a) = o.get_mut("agent").and_then(Value::as_object_mut)
        && a.remove_ordered("tool_definitions").is_some_and(|t| !t.is_null())
    {
        losses.add("tool-definitions-dropped", Some(Kind::Settings), "agent.tool_definitions needs ATIF-v1.5");
    }
    let Some(steps) = o.get_mut("steps").and_then(Value::as_array_mut) else { return };
    for step in steps {
        let Some(st) = step.as_object_mut() else { continue };
        let system = st.get("source").and_then(Value::as_str) == Some("system");
        if !to.at_least(1, 7) && st.remove_ordered("llm_call_count").is_some_and(|v| !v.is_null()) {
            losses.add("llm-call-count-dropped", Some(Kind::Usage), "step.llm_call_count needs ATIF-v1.7");
        }
        if !to.at_least(1, 5) && st.remove_ordered("is_copied_context").is_some_and(|v| !v.is_null()) {
            losses.add("copied-context-dropped", None, "step.is_copied_context needs ATIF-v1.5");
        }
        if let Some(m) = st.get_mut("metrics").and_then(Value::as_object_mut) {
            if !to.at_least(1, 4) && m.remove_ordered("prompt_token_ids").is_some_and(|v| !v.is_null()) {
                losses.add("token-ids-dropped", Some(Kind::Usage), "metrics.prompt_token_ids needs ATIF-v1.4");
            }
            if !to.at_least(1, 3) && m.remove_ordered("completion_token_ids").is_some_and(|v| !v.is_null()) {
                losses.add("token-ids-dropped", Some(Kind::Usage), "metrics.completion_token_ids needs ATIF-v1.3");
            }
        }
        for tc in st.get_mut("tool_calls").and_then(Value::as_array_mut).into_iter().flatten() {
            if let Some(a) = tc.get_mut("arguments").filter(|a| !a.is_object()) {
                let v = std::mem::take(a);
                *a = if v.is_null() { json!({}) } else { json!({ "input": v }) };
                losses.add("tool-input-wrapped", Some(Kind::ToolCall), "ATIF arguments must be an object; wrapped as {\"input\": …}");
            }
        }
        if !to.at_least(1, 7) {
            for tc in st.get_mut("tool_calls").and_then(Value::as_array_mut).into_iter().flatten() {
                if let Some(t) = tc.as_object_mut()
                    && t.remove_ordered("extra").is_some_and(|v| v.as_object().is_some_and(|m| !m.is_empty()))
                {
                    losses.add("tool-call-extra-dropped", Some(Kind::ToolCall), "tool_call.extra needs ATIF-v1.7");
                }
            }
        }
        if let Some(m) = st.get_mut("message") {
            convert_content(m, to, losses, true);
        }
        if !to.at_least(1, 2) && system && st.remove_ordered("observation").is_some_and(|v| !v.is_null()) {
            losses.add("system-observation-dropped", None, "an observation on a system step needs ATIF-v1.2");
        }
        let results = st.get_mut("observation").and_then(|o| o.get_mut("results")).and_then(Value::as_array_mut);
        for r in results.into_iter().flatten() {
            let Some(r) = r.as_object_mut() else { continue };
            if !to.at_least(1, 7) && r.remove_ordered("extra").is_some_and(|v| v.as_object().is_some_and(|m| !m.is_empty())) {
                losses.add("observation-extra-dropped", Some(Kind::ToolResult), "observation result extra needs ATIF-v1.7");
            }
            if let Some(c) = r.get_mut("content") {
                convert_content(c, to, losses, false);
            }
            for sref in r.get_mut("subagent_trajectory_ref").and_then(Value::as_array_mut).into_iter().flatten() {
                let Some(sr) = sref.as_object_mut() else { continue };
                let has_path = sr.get("trajectory_path").is_some_and(|v| !v.is_null());
                if !to.at_least(1, 7) {
                    let tid = sr.remove_ordered("trajectory_id");
                    if sr.get("session_id").is_none_or(Value::is_null) {
                        let sid = tid.clone().filter(|v| !v.is_null()).unwrap_or_else(|| json!(id));
                        sr.insert("session_id".into(), sid);
                    }
                    if !has_path {
                        losses.add("subagent-ref-unresolvable", Some(Kind::Subagent), "a pre-v1.7 ref resolves only by trajectory_path");
                    }
                } else if !has_path && sr.get("trajectory_id").is_none_or(Value::is_null) {
                    // v1.6 refs could resolve by session_id alone; v1.7 cannot.
                    let sid = sr.get("session_id").cloned().unwrap_or(json!(id));
                    sr.insert("trajectory_id".into(), sid);
                    losses.add("subagent-ref-unresolvable", Some(Kind::Subagent), "a session_id-only ref is not resolvable in ATIF-v1.7");
                }
            }
        }
    }
}

/// ContentPart arrays (v1.6) and audio parts (v1.8) on the way down.
fn convert_content(c: &mut Value, to: Ver, losses: &mut Losses, is_message: bool) {
    let Value::Array(parts) = c else { return };
    if !to.at_least(1, 8) {
        let before = parts.len();
        parts.retain(|p| p.get("type").and_then(Value::as_str) != Some("audio"));
        losses.add_n("media-dropped", Some(Kind::Media), (before - parts.len()) as u64, "audio content needs ATIF-v1.8");
    }
    if !to.at_least(1, 6) {
        let mut texts = Vec::new();
        let mut dropped = 0;
        for p in parts.iter() {
            match p.get("type").and_then(Value::as_str) {
                Some("text") => texts.push(p.get("text").and_then(Value::as_str).unwrap_or("").to_string()),
                _ => dropped += 1,
            }
        }
        losses.add_n("media-dropped", Some(Kind::Media), dropped, "content parts need ATIF-v1.6");
        if texts.len() > 1 {
            losses.add("content-parts-joined", Some(Kind::Text), "several text parts joined into one string before ATIF-v1.6");
        }
        *c = json!(texts.join("\n\n"));
        return;
    }
    if parts.is_empty() && is_message {
        *c = json!("");
    }
}

fn insert_after(o: &mut Map<String, Value>, after: &str, key: &str, v: Value) {
    let idx = o.keys().position(|k| k == after).map(|i| i + 1).unwrap_or(0);
    o.insert_at(idx, key.to_string(), v);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn down_to_v1_0_strips_later_fields() {
        let mut doc = json!({
            "schema_version": "ATIF-v1.7", "trajectory_id": "t1",
            "agent": {"name": "a", "version": "1", "tool_definitions": [{"type": "function"}]},
            "steps": [
                {"step_id": 1, "source": "user", "message": [{"type": "text", "text": "a"}, {"type": "image", "source": {"media_type": "image/png", "path": "x.png"}}]},
                {"step_id": 2, "source": "agent", "message": "", "llm_call_count": 2,
                 "tool_calls": [{"tool_call_id": "c", "function_name": "f", "arguments": {}, "extra": {"k": 1}}],
                 "metrics": {"prompt_token_ids": [1], "completion_token_ids": [2]}},
                {"step_id": 3, "source": "system", "message": "m", "observation": {"results": [{"content": "x"}]}}
            ],
            "extra": {"k": 1}
        });
        let mut l = Losses::default();
        convert(&mut doc, Ver(1, 0), "id", &mut l);
        assert_eq!(doc["session_id"], json!("t1"));
        assert!(doc.get("trajectory_id").is_none() && doc.get("extra").is_none());
        assert!(doc["agent"].get("tool_definitions").is_none());
        assert_eq!(doc["steps"][0]["message"], json!("a"));
        assert!(doc["steps"][1].get("llm_call_count").is_none());
        assert!(doc["steps"][1]["metrics"].as_object().unwrap().is_empty());
        assert!(doc["steps"][2].get("observation").is_none());
        let codes: Vec<&str> = l.0.iter().map(|x| x.code.as_str()).collect();
        for c in [
            "extra-dropped",
            "tool-definitions-dropped",
            "media-dropped",
            "llm-call-count-dropped",
            "token-ids-dropped",
            "tool-call-extra-dropped",
            "system-observation-dropped",
        ] {
            assert!(codes.contains(&c), "{c} missing from {codes:?}");
        }
    }

    #[test]
    fn up_to_v1_7_needs_resolvable_refs() {
        let mut doc = json!({"schema_version": "ATIF-v1.6", "session_id": "s", "agent": {"name": "a", "version": "1"},
            "steps": [{"step_id": 1, "source": "system", "message": "",
              "observation": {"results": [{"subagent_trajectory_ref": [{"session_id": "kid"}]}]}}]});
        let mut l = Losses::default();
        convert(&mut doc, Ver(1, 7), "s", &mut l);
        assert_eq!(doc["steps"][0]["observation"]["results"][0]["subagent_trajectory_ref"][0]["trajectory_id"], json!("kid"));
        assert_eq!(l.0[0].code, "subagent-ref-unresolvable");
    }
}
