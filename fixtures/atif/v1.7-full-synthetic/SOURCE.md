# Synthetic (v1.7-full-synthetic)

- **Origin:** synthetic, written for protoledger by a generator script (Python 3.12 `json.dumps`) from the ATIF RFC
  (`rfcs/0001-trajectory-format.md`) and Harbor's pydantic models (`src/harbor/models/trajectories/`) at
  harbor-framework/harbor commit `62db0c1fd003807ec2c64cea91f474f9bacc67d6`. Validated with Harbor's `Trajectory` model.
- **License:** MIT OR Apache-2.0 (protoledger's own).
- **Sanitisation:** none needed; neutral prompts, made-up ids, token ids and costs. Any image/audio file is a 1-pixel PNG
  or an empty WAV header.
- **Exercises:** embedded `subagent_trajectories` resolved by `trajectory_id`, a `context_management` compaction
  system step, `llm_call_count` (2 and 0), tool_call and observation-result `extra`, `tool_definitions`,
  reasoning_effort, a model change, an image file next to the trajectory, notes, root extra.
