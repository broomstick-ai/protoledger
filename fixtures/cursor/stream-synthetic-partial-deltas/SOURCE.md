# stream-synthetic-partial-deltas

**Origin:** `packages/harness-cursor/fixtures/stream/text-deltas.jsonl` in https://github.com/razzant/claudexor at commit `8de3318c1ed6b320982bb64024b3f9add0e7d071`, copied byte for byte.

Synthetic (claudexor's manifest marks it `source: synthetic`), written over the recorded frame shapes.

**License:** MIT.

**What it exercises:** `--stream-partial-output`: assistant deltas followed by the buffered flush and the final flush that repeat the text (imported as one text event), a thinking delta run.
