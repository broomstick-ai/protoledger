# ATIF-v1.8 with every optional field written as an explicit `null`

- **Origin:** the trajectory `tests/unit/agents/installed/test_claude_code_load_trajectory.py::test_load_atif_renders_session_and_resumes`
  writes (Harbor's `Trajectory.model_dump_json()` without `exclude_none`) in [harbor-framework/harbor](https://github.com/harbor-framework/harbor) at commit `62db0c1fd003807ec2c64cea91f474f9bacc67d6`, captured from disk.
- **License:** Apache-2.0 (Harbor's `LICENSE`).
- **Sanitisation:** none; synthetic test content.
