# v2.1.19-sidecar-progress

Claude Code 2.1.19: the sidecar layout (`<session>/subagents/agent-<id>.jsonl`, no meta file yet) with `progress` lines mirroring the subagent in the parent.

## Origin

https://github.com/winksaville/rlibc-x — the owner committed a real `~/.claude/projects/<slug>/`
directory as `.claude/`. Files: .claude/be4b5153-b9c0-4bff-965e-df8e1d3e19a8.jsonl, .claude/be4b5153-b9c0-4bff-965e-df8e1d3e19a8/subagents/agent-a6e6dc6.jsonl. Taken at commit 9f235380b7df5d65a0f311a361201ced6238c06f (HEAD on 2026-10-16), fetched from raw.githubusercontent.com.

## License

Apache-2.0 (the repository's license, per the GitHub license API).

## Sanitisation

Paths `/home/wink` → `/home/user` (and the slug `-home-wink-` → `-home-user-`),
`ls -l` owner ` wink users ` → ` user users `, and the shell prompt
`wink@fwlaptop` → `user@host`. The repository's public GitHub URL is kept as
attribution. Nothing else was changed.
