# v2.1.11-tool-results

Claude Code 2.1.11: a persisted tool output named by its tool_use id (`tool-results/toolu_….txt`), referenced only from the tool_result text ("Full output saved to: …"), not from `toolUseResult`.

## Origin

https://github.com/winksaville/rlibc-x — the owner committed a real `~/.claude/projects/<slug>/`
directory as `.claude/`. Files: .claude/4693a7cf-a43a-4568-a19c-9ece7613e294.jsonl, .claude/4693a7cf-a43a-4568-a19c-9ece7613e294/tool-results/toolu_012bCG7Ee9NzkAEdH1532deK.txt. Taken at commit 9f235380b7df5d65a0f311a361201ced6238c06f (HEAD on 2026-10-16), fetched from raw.githubusercontent.com.

## License

Apache-2.0 (the repository's license, per the GitHub license API).

## Sanitisation

Paths `/home/wink` → `/home/user` (and the slug `-home-wink-` → `-home-user-`),
`ls -l` owner ` wink users ` → ` user users `, and the shell prompt
`wink@fwlaptop` → `user@host`. The repository's public GitHub URL is kept as
attribution. Nothing else was changed.
