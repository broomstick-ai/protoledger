# v2.0-flat-agent-files

Claude Code 2.0.70: a session with two startup "Warmup" agents stored the 2.0 way, as `agent-<id>.jsonl` beside the session file (lines carry `agentId`).

## Origin

https://github.com/winksaville/rlibc-x — the owner committed a real `~/.claude/projects/<slug>/`
directory as `.claude/`. Files: .claude/6b9c4408-913e-4495-bf33-3df2050cc1a2.jsonl, .claude/agent-a444a46.jsonl, .claude/agent-a6aa908.jsonl. Taken at commit 9f235380b7df5d65a0f311a361201ced6238c06f (HEAD on 2026-10-16), fetched from raw.githubusercontent.com.

## License

Apache-2.0 (the repository's license, per the GitHub license API).

## Sanitisation

Paths `/home/wink` → `/home/user` (and the slug `-home-wink-` → `-home-user-`),
`ls -l` owner ` wink users ` → ` user users `, and the shell prompt
`wink@fwlaptop` → `user@host`. The repository's public GitHub URL is kept as
attribution. Nothing else was changed.
