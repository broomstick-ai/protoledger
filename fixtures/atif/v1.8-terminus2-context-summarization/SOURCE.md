# ATIF-v1.8, terminus-2 run with context summarisation (four linked files)

- **Origin:** `tests/golden/terminus_2/hello-world-context-summarization.trajectory*.json` in [harbor-framework/harbor](https://github.com/harbor-framework/harbor) at commit `62db0c1fd003807ec2c64cea91f474f9bacc67d6`. The main
  trajectory's system step 5 is a `context_management` compaction whose observation references three summarisation
  subagent trajectories by `trajectory_path`.
- **License:** Apache-2.0 (Harbor's `LICENSE`).
- **Sanitisation:** files renamed from the golden names (`hello-world-context-summarization.trajectory.<x>.json`) to the
  names Harbor writes in a real run directory (`trajectory.json`, `trajectory.<x>.json`), which are the names the
  `trajectory_path` refs already use. Contents unchanged.
