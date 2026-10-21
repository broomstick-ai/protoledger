# dir-real-vscode-0.54-f46082a6

**Origin:** `docs/hooks/vs-copilot-f46082a6-transcript.jsonl` in
https://github.com/griddynamics/rosetta at commit
`a95d97991ef22e20d979011e010c4eabc6b76e04`, copied as `events.jsonl`.

A real capture from the VS Code Copilot agent host, which writes the same
event format (`producer: copilot-agent`, `copilotVersion` 0.54.0 = the VS Code
extension, plus `vscodeVersion`).

**License:** Apache-2.0.

**Sanitisation:** the author's home directory `/Users/isolomatov` was replaced
with `/home/user` everywhere (string contents only; nothing else changed).

**What it exercises:** a second producer of the format, hook events, tool
calls and results from the VS Code agent.
