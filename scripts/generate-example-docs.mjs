// Project the runnable backend into README. No second handwritten SDK example.
import { readFileSync, writeFileSync } from 'node:fs';

if (process.argv.length > 3 || (process.argv[2] && process.argv[2] !== '--check')) {
  console.error('Usage: node scripts/generate-example-docs.mjs [--check]'); process.exit(2);
}
const readmeURL = new URL('../README.md', import.meta.url);
const readme = readFileSync(readmeURL, 'utf8');
const source = readFileSync(new URL('../examples/application/managed.mjs', import.meta.url), 'utf8').trimEnd();
const start = '<!-- managed-example:start -->', end = '<!-- managed-example:end -->';
if (readme.split(start).length !== 2 || readme.split(end).length !== 2
  || readme.indexOf(start) >= readme.indexOf(end)) throw new Error('README needs exactly one ordered managed-example marker pair');
const generated = `${start}\n<details>\n<summary>View the exact runnable backend example</summary>\n\nThis entry is shared with Console. Run it using the commands above; keep\n[its support file](examples/application/support.mjs) beside it when copying.\n\n\`\`\`js\n${source}\n\`\`\`\n</details>\n${end}`;
const output = readme.replace(new RegExp(`${start}[\\s\\S]*?${end}`), generated);
if (process.argv[2] === '--check') {
  if (readme !== output) { console.error('README example is stale; run pnpm examples:docs'); process.exit(1); }
  console.log('README example projection is current.');
} else if (readme !== output) {
  writeFileSync(readmeURL, output);
  console.log('README example refreshed from its runnable source.');
}
