# snapshot-only

A session file holding nothing but `file-history-snapshot` lines (no conversation, no version).

## Origin

https://github.com/winksaville/rlibc-x — the owner committed a real `~/.claude/projects/<slug>/`
directory as `.claude/`. Files: .claude/7b243c46-eec0-4af9-bb4e-a4d9911740f4.jsonl. Taken at commit 9f235380b7df5d65a0f311a361201ced6238c06f (HEAD on 2026-10-16), fetched from raw.githubusercontent.com.

## License

Apache-2.0 (the repository's license, per the GitHub license API).

## Sanitisation

Paths `/home/wink` → `/home/user` (and the slug `-home-wink-` → `-home-user-`),
`ls -l` owner ` wink users ` → ` user users `, and the shell prompt
`wink@fwlaptop` → `user@host`. The repository's public GitHub URL is kept as
attribution. Nothing else was changed.
