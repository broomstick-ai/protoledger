# v2-sqlite-2.0.31-generated

- **Generated** by running @opencode/cli@2.0.31 (the npm package's native darwin-arm64 binary)
  on 2026-10-16/17 (UTC) in a throwaway environment: `HOME`, `XDG_DATA_HOME`,
  `XDG_CONFIG_HOME`, `XDG_STATE_HOME`, `XDG_CACHE_HOME` and `TMPDIR` pointed into a
  scratch directory, `OPENCODE_DISABLE_AUTOUPDATE/SHARE/LSP_DOWNLOAD` and the
  Claude-Code/plugin discovery switches set, permissions `edit`/`bash` allowed.
- **Model:** **opencode/big-pickle**, the free OpenCode Zen model, anonymous — real model output.
- **Project:** a throwaway git repo (`input.txt` = `alpha\nbeta\ngamma\n`,
  `notes/plan.md`), now `/home/user/project-a`.
- **Native data:** `opencode.db` (2.x schema; WAL checkpointed, `.backup`, `journal_mode=DELETE`, `VACUUM`; the cached `models-dev:catalog` kv row deleted).
- **Prompts** (`opencode run --format json --model …`, neutral): a plain reply;
  read `input.txt` and glob; run `printf 'x\n'; exit 7` with the shell tool; use the
  task/subagent tool to summarise `notes/plan.md`; a follow-up turn on the tools
  session (`--session`); create `notes/out.txt`; then a manual compaction of the
  tools session through the server API.

```
Scenario → session id:
  s1_simple     ses_eb8d577daffemT7plHSFKatVSE   (1 simple reply, no tools)
  s2_tools      ses_eb8d458b3ffeZ1K0WtB1fzo7r8   (2 read + glob tool calls)
  s3_bash_fail  ses_eb8d18896ffeTiyOVNOvNBtsGu   (3 failing shell command (exit 7))
  s4_subagent   ses_eb8d04a5affen0kcF35RycWqNi   (4 subagent/task tool (parent session))
  s5_continue   ses_eb8d458b3ffeZ1K0WtB1fzo7r8   (5 multi-turn: follow-up turn appended to the s2 session via --session)
  s6_edit       ses_eb8c84066ffeJdlXfUdayokmeA   (6 file edit: create notes/out.txt containing done)
  s7_compact    ses_eb8d458b3ffeZ1K0WtB1fzo7r8   (6 manual compaction applied to the s2 session (after its follow-up))
  s4_child      ses_eb8c9fda6ffe8K98POiCUxPxeJ   (4 subagent child session, parentID = s4 session)
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
