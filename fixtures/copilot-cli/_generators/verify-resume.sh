#!/bin/bash
# usage: verify-resume.sh <copilot binary> <session dir with events.jsonl> [more dirs...]
#
# Asks a real Copilot CLI binary to resume each session, offline: an isolated
# COPILOT_HOME, a fake token, outbound network denied (macOS sandbox-exec).
# Copilot loads and validates events.jsonl before it authenticates, so a
# session that loads stops at the auth error ("LOADED"), and one that does
# not is reported as "Session file is corrupted ..." ("FAILED").
# The binary is GitHub's (proprietary); get it with `npm pack @github/copilot`
# and the platform package, and do not commit it.
set -u
bin="$1"; shift
for dir in "$@"; do
  home=$(mktemp -d)
  id=$(head -1 "$dir/events.jsonl" | sed -E 's/.*"sessionId":"([^"]+)".*/\1/')
  mkdir -p "$home/.copilot/session-state/$id" "$home/work"
  cp -R "$dir"/. "$home/.copilot/session-state/$id/"
  out=$(cd "$home/work" && env -i GH_TOKEN=gho_fake000000000000000000000000000000 HOME="$home" COPILOT_HOME="$home/.copilot" PATH=/usr/bin:/bin TERM=dumb \
    sandbox-exec -p '(version 1)(allow default)(deny network-outbound (remote ip))(deny network-outbound (remote unix-socket (path-regex #"^/private/var/run/mDNSResponder")))' \
    "$bin" --resume "$id" -p hi 2>&1 | tr '\n' ' ' | cut -c1-300)
  if echo "$out" | grep -q "could not be loaded\|corrupted\|no longer supported"; then
    echo "FAILED $dir :: $out"; status=1
  elif echo "$out" | grep -q -i "auth\|token"; then
    echo "LOADED $dir"
  else
    echo "UNKNOWN $dir :: $out"; status=1
  fi
  rm -rf "$home"
done
exit ${status:-0}
