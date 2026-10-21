// Deterministic scripted OpenAI-compatible chat-completions server, used only
// because OpenCode Zen's free tier refuses requests from opencode <= 1.2.x.
// The opencode binary under test is real; only the model replies are scripted.
// usage: node mock-llm.mjs <port> <logfile>
import http from "node:http"
import fs from "node:fs"

const port = Number(process.argv[2] || 47300)
const logFile = process.argv[3]
const log = (o) => logFile && fs.appendFileSync(logFile, JSON.stringify(o) + "\n")

function textOf(content) {
  if (typeof content === "string") return content
  if (Array.isArray(content)) return content.map((p) => p.text ?? "").join("\n")
  return ""
}

let callSeq = 0
function decide(body) {
  const msgs = body.messages || []
  const system = msgs.filter((m) => m.role === "system").map((m) => textOf(m.content)).join("\n")
  const hasTools = Array.isArray(body.tools) && body.tools.length > 0
  const toolNames = new Set((body.tools || []).map((t) => t.function?.name))
  const wd = (system.match(/Working directory: (\S+)/) || [])[1] || process.cwd()
  // last real user message index
  let lastUser = -1
  for (let i = msgs.length - 1; i >= 0; i--) if (msgs[i].role === "user") { lastUser = i; break }
  const userText = lastUser >= 0 ? textOf(msgs[lastUser].content) : ""
  const firstUserText = textOf((msgs.find((m) => m.role === "user") || {}).content)
  const steps = msgs.slice(lastUser + 1).filter((m) => m.role === "assistant" && m.tool_calls?.length).length
  const lastTool = [...msgs].reverse().find((m) => m.role === "tool")

  if (!hasTools && /title/i.test(system + userText) && !/Provide a detailed prompt for continuing/i.test(userText)) {
    // title generator
    if (/hello fixture/.test(userText)) return { text: "Hello fixture reply" }
    if (/input\.txt/.test(userText)) return { text: "Read input and list files" }
    if (/exit 7/.test(userText)) return { text: "Failing shell command" }
    if (/subagent/.test(userText)) return { text: "Subagent plan summary" }
    if (/out\.txt/.test(userText)) return { text: "Create notes/out.txt" }
    return { text: "Fixture session" }
  }
  if (/Provide a detailed prompt for continuing|summar(y|ize) .*conversation/i.test(userText) && !/Follow-up/.test(userText)) {
    return {
      text:
        "## Summary\nThe user asked to read input.txt (contents: alpha, beta, gamma; 3 lines) and list files with glob " +
        "(input.txt, notes/plan.md). A follow-up asked which word is longest; the answer was alpha (tied with gamma at 5 letters).\n" +
        "## Next steps\nNone pending.",
    }
  }
  if (!hasTools) return { text: "ok" }

  const call = (name, args) => ({ tool: { name, args } })
  if (/Follow-up:/.test(userText)) return { text: "alpha" }
  if (/Continue if you have next steps/.test(userText)) return { text: "Nothing further to do." }
  if (/hello fixture/.test(userText)) return { text: "hello fixture" }
  if (/Read input\.txt using the read tool/.test(userText)) {
    if (steps === 0) return call("read", { filePath: `${wd}/input.txt` })
    if (steps === 1) return call("glob", { pattern: "**/*" })
    return { text: "The three words are alpha, beta and gamma; input.txt has 3 lines." }
  }
  if (/exit 7/.test(userText)) {
    if (steps === 0) return call("bash", { command: "printf 'x\\n'; exit 7", description: "Run command that exits with code 7" })
    return { text: "The command printed `x` and exited with code 7." }
  }
  if (/launch a general subagent/.test(userText) && toolNames.has("task")) {
    if (steps === 0)
      return call("task", {
        description: "Summarize notes/plan.md",
        prompt: "Read notes/plan.md and summarize it in one line.",
        subagent_type: "general",
      })
    return { text: "Summary: a three-step plan to read input.txt, count its lines, and write a short report." }
  }
  if (/Read notes\/plan\.md and summarize/.test(userText)) {
    if (steps === 0) return call("read", { filePath: `${wd}/notes/plan.md` })
    return { text: "A three-step plan: read input.txt, count the lines, write a short report." }
  }
  if (/notes\/out\.txt/.test(userText)) {
    if (steps === 0) return call("write", { filePath: `${wd}/notes/out.txt`, content: "done\n" })
    return { text: "ok" }
  }
  return { text: "ok" }
}

function usage() {
  return { prompt_tokens: 120, completion_tokens: 12, total_tokens: 132 }
}

http
  .createServer((req, res) => {
    let raw = ""
    req.on("data", (c) => (raw += c))
    req.on("end", () => {
      if (req.method === "GET") {
        res.writeHead(200, { "content-type": "application/json" })
        return res.end(JSON.stringify({ object: "list", data: [{ id: "fixture-model", object: "model" }] }))
      }
      let body = {}
      try { body = JSON.parse(raw || "{}") } catch {}
      const d = decide(body)
      log({ url: req.url, decision: d, nmsg: body.messages?.length, tools: (body.tools || []).map((t) => t.function?.name) })
      const id = "chatcmpl-fixture-" + ++callSeq
      const created = 1760000000 + callSeq
      const model = body.model || "fixture-model"
      const toolCall = d.tool && { id: "call_fixture_" + callSeq, type: "function", function: { name: d.tool.name, arguments: JSON.stringify(d.tool.args) } }
      if (!body.stream) {
        res.writeHead(200, { "content-type": "application/json" })
        return res.end(JSON.stringify({
          id, object: "chat.completion", created, model,
          choices: [{ index: 0, finish_reason: toolCall ? "tool_calls" : "stop",
            message: { role: "assistant", content: d.text ?? null, ...(toolCall ? { tool_calls: [toolCall] } : {}) } }],
          usage: usage(),
        }))
      }
      res.writeHead(200, { "content-type": "text/event-stream", "cache-control": "no-cache", connection: "keep-alive" })
      const send = (o) => res.write("data: " + JSON.stringify({ id, object: "chat.completion.chunk", created, model, ...o }) + "\n\n")
      send({ choices: [{ index: 0, delta: { role: "assistant", content: "" }, finish_reason: null }] })
      if (d.text) for (const piece of d.text.match(/.{1,24}/gs)) send({ choices: [{ index: 0, delta: { content: piece }, finish_reason: null }] })
      if (toolCall) send({ choices: [{ index: 0, delta: { tool_calls: [{ index: 0, ...toolCall }] }, finish_reason: null }] })
      send({ choices: [{ index: 0, delta: {}, finish_reason: toolCall ? "tool_calls" : "stop" }] })
      send({ choices: [], usage: usage() })
      res.end("data: [DONE]\n\n")
    })
  })
  .listen(port, "127.0.0.1", () => console.log("mock listening " + port))
