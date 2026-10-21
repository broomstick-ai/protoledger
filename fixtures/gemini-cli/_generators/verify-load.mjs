// usage: node verify-load.mjs <dir with node_modules/@google/gemini-cli-core> <file> [--resume]
// Loads a session file (JSONL or JSON) or a /chat save checkpoint with
// gemini-cli-core's own loaders, rebuilds the API history Gemini CLI would
// resume with, and checks it is well formed: alternating user/model turns,
// every functionCall answered by a functionResponse with the same id.
import path from 'node:path'; import fs from 'node:fs'; import os from 'node:os';
const [coreDir, file, flag] = process.argv.slice(2);
const core = await import(path.resolve(coreDir, 'node_modules/@google/gemini-cli-core/dist/index.js'));
let history;
const base = path.basename(file);
if (base.startsWith('checkpoint-')) {
  const tag = decodeURIComponent(base.slice('checkpoint-'.length, -'.json'.length));
  // Logger.initialize writes logs.json next to the checkpoints: use a copy.
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'gcv-'));
  fs.copyFileSync(file, path.join(dir, base));
  const logger = new core.Logger('verify', { initialize: async () => {}, getProjectTempDir: () => dir, getProjectTempCheckpointsDir: () => dir });
  await logger.initialize();
  const ck = await logger.loadCheckpoint(tag);
  history = (Array.isArray(ck) ? ck : ck.history).map((c, i) => ({ id: String(i), content: c }));
} else {
  const rec = core.loadConversationRecord ? await core.loadConversationRecord(file) : JSON.parse(fs.readFileSync(file, 'utf8'));
  if (!rec) throw new Error('loadConversationRecord returned null');
  history = core.convertSessionToClientHistory(rec.messages);
  if (flag === '--resume') {
    const tmp = fs.mkdtempSync(path.join(os.tmpdir(), 'gcv-'));
    const copy = path.join(tmp, base);
    fs.copyFileSync(file, copy);
    const svc = new core.ChatRecordingService({ promptId: rec.sessionId, config: { getProjectRoot: () => '/', storage: { getProjectTempDir: () => tmp }, getWorkspaceContext: () => ({ getDirectories: () => [] }) }, toolRegistry: { getTool: () => undefined } });
    await svc.initialize({ conversation: rec, filePath: copy }, rec.kind);
    const conv = svc.getConversation();
    if (!conv || conv.messages.length !== rec.messages.length) throw new Error('resume lost messages');
  }
}
const turns = history.map((h) => h.content ?? h);
const problems = [];
const notes = [];
const pending = new Map();
turns.forEach((c, i) => {
  if (i > 0 && turns[i - 1].role === c.role) notes.push(`turn ${i}: two ${c.role} turns in a row`);
  for (const p of c.parts ?? []) {
    if (p.functionCall) pending.set(p.functionCall.id, i);
    if (p.functionResponse) {
      if (!pending.has(p.functionResponse.id)) problems.push(`turn ${i}: functionResponse ${p.functionResponse.id} answers no call`);
      pending.delete(p.functionResponse.id);
    }
  }
});
if (turns.length === 0) problems.push('nothing loaded');
for (const [id, i] of pending) problems.push(`turn ${i}: functionCall ${id} never answered`);
console.log(JSON.stringify({ file: base, turns: turns.length, roles: turns.map((c) => c.role[0]).join(''), problems, notes }));
if (problems.length) process.exitCode = 1;
