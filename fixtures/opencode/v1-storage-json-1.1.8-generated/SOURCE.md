# v1-storage-json-1.1.8-generated

- **Generated** by running opencode-ai@1.1.8 (the npm package's native darwin-arm64 binary)
  on 2026-10-16/17 (UTC) in a throwaway environment: `HOME`, `XDG_DATA_HOME`,
  `XDG_CONFIG_HOME`, `XDG_STATE_HOME`, `XDG_CACHE_HOME` and `TMPDIR` pointed into a
  scratch directory, `OPENCODE_DISABLE_AUTOUPDATE/SHARE/LSP_DOWNLOAD` and the
  Claude-Code/plugin discovery switches set, permissions `edit`/`bash` allowed.
- **Model:** a **local scripted OpenAI-compatible endpoint** (`_tools/mock-llm.mjs`, provider `fixture/fixture-model` via `@ai-sdk/openai-compatible`): OpenCode Zen refuses free-tier requests from this version. The opencode binary, its tools, storage and export are real; only the assistant's words and tool-call choices are scripted.
- **Project:** a throwaway git repo (`input.txt` = `alpha\nbeta\ngamma\n`,
  `notes/plan.md`), now `/home/user/project-a`.
- **Native data:** the whole `$XDG_DATA_HOME/opencode/storage` tree (`project/`, `session/`, `message/`, `part/`, `session_diff/`, `migration`).
- **Prompts** (`opencode run --format json --model …`, neutral): a plain reply;
  read `input.txt` and glob; run `printf 'x\n'; exit 7` with the shell tool; use the
  task/subagent tool to summarise `notes/plan.md`; a follow-up turn on the tools
  session (`--session`); create `notes/out.txt`; then a manual compaction of the
  tools session through the server API.

```
Scenario → session id:
  s1_simple     ses_eb8d6e895ffelnosNEd7BusjvE   (1 simple reply, no tools)
  s2_tools      ses_eb8d6b6c3ffe7PBN5Mvaul4Hw8   (2 read + glob tool calls)
  s3_bash_fail  ses_eb8d65eacffeypcj0au0M9u834   (3 failing shell command (exit 7))
  s4_subagent   ses_eb8d6226effetkLNpC2gL32PTJ   (4 subagent/task tool (parent session))
  s5_continue   ses_eb8d6b6c3ffe7PBN5Mvaul4Hw8   (5 multi-turn: follow-up turn appended to the s2 session via --session)
  s6_edit       ses_eb8d5b11effeOoY3wQX3mr7ogV   (6 file edit: create notes/out.txt containing done)
  s5b_continue  ses_eb8d6b6c3ffe7PBN5Mvaul4Hw8   (5b second follow-up turn on the s2 session)
  s7_compact    ses_eb8d6b6c3ffe7PBN5Mvaul4Hw8   (6 manual compaction applied to the s2 session (after its follow-up))
  s4_child      ses_eb8d62215ffeMFyurHd6LNaC4g   (4 subagent child session, parentID = s4 session)
```

- **Sanitised** (in the copy only): the project path and the scratch home/XDG
  paths rewritten to `/home/user/…`, the local user name to `user`, in every JSON
  file and every text value of every table (then `VACUUM`); account, credential
  and share tables are empty; no `auth.json` or config was copied. Timestamps,
  ids, token counts and git hashes (the project id) are unchanged. The export
  files were rewritten by the sanitiser in the same `JSON.stringify(x, null, 2)`
  layout.
- **Notes:** `opencode run` stores the prompt wrapped in double quotes (and 1.x
  appends a newline), because it re-joins argv.
- **License:** the session data is ours to publish (generated for this purpose,
  neutral content), MIT OR Apache-2.0 like this repository.
