// usage: node record-session.mjs <dir with node_modules/@google/gemini-cli-core> <outDir>
// Drives gemini-cli-core's real ChatRecordingService through the same call
// sequence GeminiChat uses for one session with thoughts, tokens, tool calls,
// tool responses, a UI info/error notice, a session summary and a subagent.
import path from 'node:path';
import fs from 'node:fs';
const ver = process.argv[2];
const outDir = process.argv[3];
const core = await import(path.resolve(`./${ver}/node_modules/@google/gemini-cli-core/dist/index.js`));
const { ChatRecordingService } = core;
fs.rmSync(outDir, { recursive: true, force: true });
const tools = {
  read_file: { displayName: 'ReadFile', description: 'Reads a file', isOutputMarkdown: true },
  replace: { displayName: 'Edit', description: 'Replaces text in a file', isOutputMarkdown: true },
  codebase_investigator: { displayName: 'Codebase Investigator Agent', description: 'Investigates the codebase', isOutputMarkdown: true },
};
function ctx(promptId, parentSessionId) {
  return {
    promptId, parentSessionId,
    config: {
      getProjectRoot: () => '/home/user/project',
      storage: { getProjectTempDir: () => outDir },
      getWorkspaceContext: () => ({ getDirectories: () => ['/home/user/project'] }),
      getSessionId: () => promptId,
    },
    toolRegistry: { getTool: (n) => tools[n] },
  };
}
const synth = (svc, type, parts) => svc.recordSyntheticMessage ? svc.recordSyntheticMessage(type, parts) : (type === 'gemini' ? svc.recordMessage({ model, type, content: '' }) : null);
const sync = (svc, h) => { if (svc.updateMessagesFromHistory) svc.updateMessagesFromHistory(h); };
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const model = 'gemini-2.5-pro';
const main = new ChatRecordingService(ctx('5b9c3c1e-2f0a-4c43-9a51-6f4f0e0b7a11'));
await main.initialize(undefined, 'main');
const hist = [];
const push = (id, role, parts) => { hist.push({ id, content: { role, parts } }); };
// turn 1
let uid = main.recordMessage({ model, type: 'user', content: [{ text: 'Read README.md and fix the typo in the title.' }] });
push(uid, 'user', [{ text: 'Read README.md and fix the typo in the title.' }]);
await sleep(5);
main.recordThought({ subject: 'Reading the file', description: 'I should read README.md before editing it.' });
main.recordMessageTokens({ promptTokenCount: 1200, candidatesTokenCount: 40, cachedContentTokenCount: 800, thoughtsTokenCount: 25, totalTokenCount: 1265 });
let gid = main.recordMessage({ model, type: 'gemini', content: 'I will read the file first.' });
const call1 = { functionCall: { id: 'read_file_1760000000000_0', name: 'read_file', args: { file_path: 'README.md' } } };
push(gid, 'model', [{ text: '**Reading the file** I should read README.md before editing it.', thought: true, thoughtSignature: 'c2lnLTE=' }, { text: 'I will read the file first.' }, call1]);
await sleep(5);
const resp1 = [{ functionResponse: { id: 'read_file_1760000000000_0', name: 'read_file', response: { output: '# Teh Project\n\nA demo.\n' } } }];
main.recordToolCalls(model, [{ id: 'read_file_1760000000000_0', name: 'read_file', args: { file_path: 'README.md' }, result: resp1, status: 'success', timestamp: new Date().toISOString(), resultDisplay: '', description: 'README.md' }]);
const rid = synth(main, 'user', resp1);
push(rid, 'user', resp1);
sync(main, hist);
await sleep(5);
// turn 1, step 2: edit
main.recordMessageTokens({ promptTokenCount: 1350, candidatesTokenCount: 60, cachedContentTokenCount: 1200, thoughtsTokenCount: 0, totalTokenCount: 1410 });
const g2 = synth(main, 'gemini', []);
const call2 = { functionCall: { id: 'replace_1760000000100_0', name: 'replace', args: { file_path: '/home/user/project/README.md', old_string: '# Teh Project', new_string: '# The Project' } } };
push(g2, 'model', [call2]);
await sleep(5);
const resp2 = [{ functionResponse: { id: 'replace_1760000000100_0', name: 'replace', response: { output: 'Successfully modified file: /home/user/project/README.md (1 replacements).' } } }];
main.recordToolCalls(model, [{ id: 'replace_1760000000100_0', name: 'replace', args: call2.functionCall.args, result: resp2, status: 'success', timestamp: new Date().toISOString(),
  resultDisplay: { fileDiff: 'Index: README.md\n===================================================================\n--- README.md\tCurrent\n+++ README.md\tProposed\n@@ -1,3 +1,3 @@\n-# Teh Project\n+# The Project\n \n A demo.\n', fileName: 'README.md', filePath: '/home/user/project/README.md', originalContent: '# Teh Project\n\nA demo.\n', newContent: '# The Project\n\nA demo.\n', diffStat: { model_added_lines: 1, model_removed_lines: 1, model_added_chars: 13, model_removed_chars: 13, user_added_lines: 0, user_removed_lines: 0, user_added_chars: 0, user_removed_chars: 0 }, isNewFile: false },
  description: 'README.md: # Teh Project => # The Project' }]);
const rid2 = synth(main, 'user', resp2);
push(rid2, 'user', resp2);
sync(main, hist);
await sleep(5);
main.recordMessageTokens({ promptTokenCount: 1450, candidatesTokenCount: 12, cachedContentTokenCount: 1300, thoughtsTokenCount: 0, totalTokenCount: 1462 });
const g3 = main.recordMessage({ model, type: 'gemini', content: 'Fixed the typo: the title now reads "The Project".' });
push(g3, 'model', [{ text: 'Fixed the typo: the title now reads "The Project".' }]);
sync(main, hist);
main.recordMessage({ model: undefined, type: 'info', content: 'Request cancelled.' });
main.saveSummary('Fix README title typo');
// turn 2: subagent
await sleep(5);
const u2 = main.recordMessage({ model, type: 'user', content: [{ text: 'Where is the entry point?' }], displayContent: [{ text: 'Where is the entry point?' }] });
push(u2, 'user', [{ text: 'Where is the entry point?' }]);
const subId = 'b0c1d2e3-0000-4000-8000-00000000c0de';
const sub = new ChatRecordingService(ctx(subId, '5b9c3c1e-2f0a-4c43-9a51-6f4f0e0b7a11'));
await sub.initialize(undefined, 'subagent');
sub.recordMessage({ model: 'gemini-2.5-flash', type: 'user', content: [{ text: 'Find the program entry point.' }] });
sub.recordMessageTokens({ promptTokenCount: 300, candidatesTokenCount: 20, cachedContentTokenCount: 0, thoughtsTokenCount: 0, totalTokenCount: 320 });
sub.recordMessage({ model: 'gemini-2.5-flash', type: 'gemini', content: 'The entry point is src/main.rs.' });
await sleep(5);
main.recordMessageTokens({ promptTokenCount: 1500, candidatesTokenCount: 30, cachedContentTokenCount: 1400, thoughtsTokenCount: 0, totalTokenCount: 1530 });
const g4 = synth(main, 'gemini', []);
const call3 = { functionCall: { id: 'codebase_investigator_1760000000200_0', name: 'codebase_investigator', args: { objective: 'Find the program entry point.' } } };
push(g4, 'model', [call3]);
const resp3 = [{ functionResponse: { id: 'codebase_investigator_1760000000200_0', name: 'codebase_investigator', response: { output: 'The entry point is src/main.rs.' } } }];
main.recordToolCalls(model, [{ id: 'codebase_investigator_1760000000200_0', name: 'codebase_investigator', args: call3.functionCall.args, result: resp3, status: 'success', timestamp: new Date().toISOString(), agentId: subId,
  resultDisplay: { isSubagentProgress: true, agentName: 'codebase_investigator', recentActivity: [], state: 'completed', result: 'The entry point is src/main.rs.' }, description: 'Find the program entry point.' }]);
const rid3 = synth(main, 'user', resp3);
push(rid3, 'user', resp3);
sync(main, hist);
const g5 = main.recordMessage({ model, type: 'gemini', content: 'The entry point is `src/main.rs`.' });
push(g5, 'model', [{ text: 'The entry point is `src/main.rs`.' }]);
sync(main, hist);
main.recordMessage({ model: undefined, type: 'error', content: 'Quota exceeded for quota metric. Please retry later.' });
// turn 3 rewound
const u3 = main.recordMessage({ model, type: 'user', content: [{ text: 'Delete everything.' }] });
if (main.rewindTo) main.rewindTo(u3);
console.log(main.getConversationFilePath());
