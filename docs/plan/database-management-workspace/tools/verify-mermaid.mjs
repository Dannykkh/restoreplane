import fs from 'node:fs/promises';
import path from 'node:path';
import { pathToFileURL, fileURLToPath } from 'node:url';

const modulePath = process.argv[2];
if (!modulePath) throw new Error('Pass an installed mermaid core module path; no automatic global install.');
const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
// Mermaid's label sanitizer requires a DOM even when only parsing syntax.
// Use the project's already installed jsdom; no app dependencies are changed.
const { JSDOM } = await import(pathToFileURL(path.resolve(root, '../../../apps/console/node_modules/jsdom/lib/api.js')));
const dom = new JSDOM('<!doctype html><html><body></body></html>');
globalThis.window = dom.window;
globalThis.document = dom.window.document;
const { default: mermaid } = await import(pathToFileURL(modulePath));
mermaid.initialize({ startOnLoad: false, securityLevel: 'strict' });
const dir = path.join(root, 'flow-diagrams');
const errors = [];
const files = (await fs.readdir(dir)).filter(x => x.endsWith('.mmd')).sort();
for (const name of files) {
  try {
    await mermaid.parse(await fs.readFile(path.join(dir, name), 'utf8'));
  } catch (error) {
    errors.push({ name, error: String(error.message ?? error) });
  }
}
console.log(JSON.stringify({ scope: 'Mermaid syntax only; no application flow match claim', files: files.length, errors }, null, 2));
process.exitCode = errors.length ? 1 : 0;
