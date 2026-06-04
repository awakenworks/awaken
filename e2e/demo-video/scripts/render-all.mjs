import { execSync } from 'node:child_process';
import { existsSync, mkdirSync } from 'node:fs';
import { resolve } from 'node:path';

const FRAMES = resolve('../target/demo-frames');
const OUT = resolve('out');
const ENTRY = 'src/index.ts';

const LOCALES = [
  { key: 'en', dir: 'en', manifest: 'manifest-en.json' },
  { key: 'zh', dir: 'zh', manifest: 'manifest-zh.json' },
];

mkdirSync(OUT, { recursive: true });

const run = (cmd) => {
  console.log('+', cmd);
  execSync(cmd, { stdio: 'inherit' });
};

let rendered = 0;
for (const l of LOCALES) {
  const pub = `${FRAMES}/${l.dir}`;
  const props = `${pub}/${l.manifest}`;
  if (!existsSync(props)) {
    console.warn(`skip ${l.key}: ${props} not found`);
    continue;
  }
  const common = `--public-dir="${pub}" --props="${props}"`;
  run(`npx remotion render ${ENTRY} DemoLong "${OUT}/awaken-demo-${l.key}.mp4" ${common}`);
  run(`npx remotion render ${ENTRY} DemoHighlight "${OUT}/awaken-demo-${l.key}-highlight.mp4" ${common}`);
  run(`npx remotion render ${ENTRY} DemoHighlight "${OUT}/awaken-demo-${l.key}.gif" ${common} --codec=gif --every-nth-frame=2`);
  rendered += 1;
}

if (rendered === 0) {
  console.error('No manifests found — run the capture stage first.');
  process.exit(1);
}
console.log(`Done: rendered ${rendered} locale(s) -> ${OUT}`);
