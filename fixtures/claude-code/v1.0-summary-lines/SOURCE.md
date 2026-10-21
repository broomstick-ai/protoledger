# v1.0-summary-lines

Claude Code 1.0.69 – 1.0.83 lines: `summary` title lines (with `leafUuid`),
synthetic `<synthetic>` assistant lines (usage-limit and API-error notices),
`isMeta` and command lines. Not one session: it is the set of real lines the
ccexport tool skipped while its author ran it on his own sessions, unwrapped
from ccexport's `{"line","reason","data"}` envelope in file order and written
as one file named after the first `sessionId`. The lines themselves are
verbatim 1.0.x records (re-serialised as compact JSON).

## Origin and license

https://github.com/marcheiligers/ccexport, `VIBE_full_skipped.jsonl` at commit
304c7c410018f885d7ace7b274b5f12fcb1fbfcc. License: MIT.

## Sanitisation

`/Users/marcheiligers` → `/home/user` (and the slug likewise).
