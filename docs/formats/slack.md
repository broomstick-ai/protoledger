# Slack

`slack` treats a Slack conversation as **conversation input**: people and bots talking in a thread or channel,
imported so it can be handed to an agent (or kept next to one), and exported so a ledger can be posted or archived
as a thread. Slack is not an agent harness, so there are no tool calls, reasoning or usage to map; everything an
agent ledger holds beyond messages and files is a named loss (and carried, see below).

## Variants

| variant | input | ledgers |
|---|---|---|
| `api-replies` (default export) | one JSON object with a `messages` array: a Web API `conversations.replies` or `conversations.history` response (`ok`, `messages`, `has_more`, `response_metadata`), or any wrapper with one, such as letta-ai/trajectory's `{channel, channel_name, users, messages}` | one per file |
| `messages-array` | a bare JSON array of message objects (one day file of an export, or a dump) | one per file |
| `export-dir` | a workspace export directory: `users.json`, `channels.json` (also `groups.json`, `dms.json`, `mpims.json`, `integration_logs.json`, …) and `<channel>/<YYYY-MM-DD>.json` day files | one per channel, plus `slack:workspace` |

A directory that is not an export (no `channels.json`/`users.json` next to day-file folders) is read as a set of
thread files, each imported as `api-replies` or `messages-array`; that is what exporting several ledgers writes.
Slack records no producer version, so `source.version` is unset. Detection: an object whose `messages` look like
Slack messages (string `ts`) is `api-replies` (85 with `ok`, else 70); such an array is `messages-array` (70); an
export directory is 90.

## Ledgers

- **A file** is one ledger. Its id is `slack:<channel>:<thread_ts>` when every message is in one thread,
  `slack:<channel>` otherwise; without a channel (`channel` on the wrapper or on messages) it is
  `slack:<thread_ts>` or `slack:<file stem>`. `source.native_id` is the thread ts (or channel id);
  `meta.slack` holds `channel` and `channel_name`.
- **An export directory** is one ledger per channel folder (`slack:<folder>`, `meta.slack.channel` from
  `channels.json`) with all its day files in date order, each linked `member-of` a `slack:workspace` ledger.
  The workspace ledger has one `native` event per directory-level file (`users.json`, `channels.json`, …), so the
  directory comes back exactly. It is left out of an export only when all four hold: it holds only native or
  app records; another ledger names it `member-of` (every channel ledger does); the export is without carry; and
  the target is neither `slack` nor `protoledger`. It is then counted as `container-not-rendered` (SPEC §2.3)
  instead of being written as an empty conversation. With carry, or exported to `slack` or `protoledger`, it is
  written as before. Threads inside a channel are told apart by `turn` (the thread ts).

## Mapping

| Slack | protoledger |
|---|---|
| message (no subtype, or `bot_message`, `thread_broadcast`, `reply_broadcast`, `file_share`, `me_message`) | `text` {`role`, `text`}: role `assistant` when the message is a bot's (`bot_id`, `bot_message`, `bot_profile`), `user` otherwise. The text is Slack's raw mrkdwn (`<@U…>` mentions, `<url\|label>` links) |
| each entry of `files` with a URL | `media` {`role`, `media_type` ← `mimetype`, `url` ← `url_private` (else `permalink`), `name`}, part of the same message |
| other subtypes (`channel_join`, `channel_topic`, `pinned_item`, edit/delete events, …), non-message events, empty messages | one `native` event |
| `ts` | event id (native id; `ts~2` for a repeated copy) and `ts` as RFC 3339 with microseconds |
| `thread_ts` | `turn` |
| one message | `group` = its `ts` |
| `user`, `bot_id`, `subtype`, speaker name | `meta.slack` {`user`, `bot_id`, `subtype`, `name`}; the name comes from `users` (export `users.json` or a wrapper's `users`), then `user_profile`, `bot_profile.name`, `username` |
| `blocks`, `attachments`, `reactions`, `edited`, `reply_count`, `client_msg_id`, `team`, a broadcast's nested `root`, … | native only |
| response wrapper (`ok`, `has_more`, `response_metadata`, wrapper `users`, …) | a leading `native` event `response` holding the wrapper record |

Who said a message is attached (`meta`), not prepended to the text: `body.text` stays what was posted. An exporter
that flattens a multi-person thread into one user role should read `meta.slack.name` if it needs speakers.

## Exact self trip

Each message object is the native record of its events; the wrapper and the workspace files are records too.
The layout of every file is detected with the same detector the ATIF adapter uses
(`crate::formats::atif::jsonfmt`) and kept in `source.extra.json` (single files) or `source.extra.files`
(per file in an export). Slack's own layout is covered: 4-space indent with `\/` escaped slashes and `\uXXXX`
non-ASCII in exports, compact JSON with escaped slashes from the API. A file no style reproduces keeps its raw
text (on the wrapper record, or `source.extra.raw`). An empty day file is kept as a marker event. All 7 fixtures
come back byte for byte.

## Export of other ledgers

- `text` with role `user` → a message from `meta.slack.user` or `U0PROTOLEDGER`; role `assistant` → a
  `bot_message` from `meta.slack.bot_id` or `B0PROTOLEDGER` with `username` from `meta.slack.name` (else
  `assistant`). Consecutive events of one `group` join one message; `peer_message` is written as a message
  (out = bot) holding the SPEC §3.1.6 wrapper, which the importer turns back into a `peer_message`.
- `media` with a `url` → an entry in that message's `files` (`name`, `mimetype`, `url_private`); a message with
  only files gets subtype `file_share`.
- `ts` is minted from the event time (microseconds kept), made unique and increasing; every message gets
  `thread_ts` = the first message's `ts`, so a ledger becomes one thread.
- `api-replies` (default) writes `<ledger id>.json` as `{"ok": true, "messages": [...], "has_more": false}`;
  `messages-array` writes the bare array; `export-dir` writes one folder per ledger with day files by UTC date,
  plus `users.json` and `channels.json` for the two synthetic speakers (in Slack's export layout).


## Branches (SPEC §2.3)

A Slack thread is replies in `ts` order: no branch can be expressed. Decision: **not expressible** — the exporter renders the active
path, names `abandoned-branch-not-rendered`, and carries the abandoned events on
the next message. Done for every variant (one writer). The comparator now reads `after` (`<kind>.branch` costs what dropping the event costs, `<kind>.after` a wrong predecessor), so writing the abandoned branch as the conversation is no longer free: `protoledger/branch-rewind` via slack was 13 with the branch written inline (where the comparator could not see it) and is 17 with the active path rendered, its 4 abandoned events (13 facts) missing; written inline it would score 20 under the new comparator (its tool call and result were already missing). With carry the trip still loses nothing (carried score 0).

## Carry

Message objects tolerate extra keys, so each rendered message gets `protoledger = {v, h, events}` (SPEC §6) with
the events it was made from and every event Slack cannot hold that followed it (events before the first message
ride on the first). The wrapper (`api-replies`) gets `protoledger.agent` with the ledger id, `created_at`, `meta`,
`links` and `parent`. On import, an unedited message restores its events in their original order; an edited one
is imported fresh. With carry, every fixture and reference ledger sent through Slack comes back with no neutral
loss. `messages-array` and `export-dir` have no wrapper, so the ledger header is not carried there.

## Loss codes

Import: `file-without-url` (a shared file with no URL; native only), `file-not-imported` (a non-JSON or
non-day file in an export).

Export: `system-message-dropped` (system/developer text), `media-dropped` (media with no URL), `reasoning-dropped`,
`tool-call-dropped`, `tool-result-dropped`, `subagent-dropped`, `compaction-dropped`, `usage-dropped`,
`settings-dropped`, `title-dropped`, `turn-dropped`, `error-dropped`, `file-change-dropped`, `app-dropped`,
`native-dropped`, `peer-message-fields-dropped` (peer messages are written as SPEC §3.1.6 wrapped text (user when incoming, assistant when outgoing), which the importer turns back into a `peer_message`; this code names the fields beyond direction, peer, network and text that the wrapper cannot hold), `wrapper-dropped` (an API wrapper written as a bare array),
`empty-thread-padded` (a ledger with nothing Slack can show gets one empty message to hold the carrier),
`channel-not-listed` (a rendered channel added to a native export is not added to its `channels.json`),
`exact-text-fallback` (an unchanged document (every record its own) could not be rebuilt from its records' own texts (a piece has no text, or the rebuilt text does not parse to the document's value) and is written from the parsed value, so its layout, number spelling or escapes may differ from the source. No fixture reaches it; a unit test does (`an_unchanged_document_not_rebuilt_from_its_texts_names_the_fallback`)).

## Fixtures

7 cases: four letta-ai/trajectory Slack fixtures (Apache-2.0; a rich thread with a users directory and blocks, a
relay bot thread with Unicode, a channel dump with a missing root and a file without URL, and a cleanup case with
duplicate copies, a broadcast with a nested root and a join), and three labelled synthetics modelled on Slack's
documented shapes: a `conversations.replies` response for a person-and-bot-agent thread, a bare day file, and a
two-channel workspace export.

## Verified and unverified

- Verified: byte-exact self trips; carried trips clean through Slack for all ATIF and Slack fixtures and the
  reference ledgers; Slack ledgers exported to ATIF v1.0–v1.8 validate with Harbor's validator (45/45).
- Unverified: the exact byte layout of live Slack API responses and real exports (the synthetic fixtures follow
  the documented and commonly observed layout; any other layout falls back to raw text, so self trips stay
  exact); Enterprise Grid exports, Slack Connect and huddle/canvas files (read as plain directory-level files).
  No real Slack data is included: none with a publishable license was found.

## Prefix stability (SPEC §9.3)

Not tested for prefix stability: every variant is a JSON document (or a directory of day documents), with no record boundary to cut at.
