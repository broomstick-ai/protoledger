import path from 'node:path'; import fs from 'node:fs';
// usage: node save-checkpoint.mjs <dir with node_modules/@google/gemini-cli-core> <outDir> <object|array> <tag>
const ver = process.argv[2], outDir = process.argv[3], form = process.argv[4] || 'object', tag = process.argv[5] || 'demo tag';
const core = await import(path.resolve(`./${ver}/node_modules/@google/gemini-cli-core/dist/index.js`));
fs.rmSync(outDir, { recursive: true, force: true }); fs.mkdirSync(outDir, { recursive: true });
const storage = { initialize: async () => {}, getProjectTempDir: () => outDir, getProjectTempCheckpointsDir: () => outDir };
const logger = new core.Logger('5b9c3c1e-2f0a-4c43-9a51-6f4f0e0b7a11', storage);
await logger.initialize();
const history = [
  { role: 'user', parts: [{ text: 'This is the Gemini CLI. We are setting up the context for our chat.\nToday\'s date is Friday, October 16, 2026.\nMy operating system is: linux\nI\'m currently working in the directory: /home/user/project' }] },
  { role: 'model', parts: [{ text: 'Got it. Thanks for the context!' }] },
  { role: 'user', parts: [{ text: 'List the files and show me Cargo.toml.' }] },
  { role: 'model', parts: [
    { text: 'I need to look at the directory first.', thought: true, thoughtSignature: 'Q2lnLXNpZw==' },
    { text: 'Let me look.' },
    { functionCall: { id: 'list_directory_1760000000300_0', name: 'list_directory', args: { dir_path: '.' } } },
    { functionCall: { id: 'read_file_1760000000300_1', name: 'read_file', args: { file_path: 'Cargo.toml' } } } ] },
  { role: 'user', parts: [
    { functionResponse: { id: 'list_directory_1760000000300_0', name: 'list_directory', response: { output: 'Directory listing for /home/user/project:\n[DIR] src\nCargo.toml' } } },
    { functionResponse: { id: 'read_file_1760000000300_1', name: 'read_file', response: { error: 'File not found: Cargo.toml' } } } ] },
  { role: 'model', parts: [{ text: 'There is a `src` directory but no Cargo.toml.' }] },
  { role: 'user', parts: [{ text: 'What is in this screenshot?' }, { inlineData: { mimeType: 'image/png', data: 'iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==' } }] },
  { role: 'model', parts: [{ text: 'A single transparent pixel.' }] },
];
await logger.saveCheckpoint(form === 'array' ? history.slice(0, 6) : { history, authType: 'oauth-personal' }, tag);
console.log(fs.readdirSync(outDir));
const back = await logger.loadCheckpoint(tag); console.log(Array.isArray(back) ? back.length : back.history.length);
