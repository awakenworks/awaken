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
  const long = `${OUT}/awaken-demo-${l.key}.mp4`;
  const highlight = `${OUT}/awaken-demo-${l.key}-highlight.mp4`;
  const gif = `${OUT}/awaken-demo-${l.key}.gif`;
  const palette = `${OUT}/_palette-${l.key}.png`;

  run(`npx remotion render ${ENTRY} DemoLong "${long}" ${common}`);
  run(`npx remotion render ${ENTRY} DemoHighlight "${highlight}" ${common}`);

  // GIF via ffmpeg palettegen from the highlight MP4 — far smaller and sharper
  // than the raw gif codec. Sped up 3x + 560px/10fps/128-color to land a
  // compact, README-friendly teaser (~7 MB) instead of a 50s full-size loop.
  const filt = 'setpts=PTS/3.0,fps=10,scale=560:-1:flags=lanczos';
  run(`ffmpeg -y -loglevel error -i "${highlight}" -vf "${filt},palettegen=max_colors=128:stats_mode=diff" "${palette}"`);
  run(`ffmpeg -y -loglevel error -i "${highlight}" -i "${palette}" -lavfi "${filt}[x];[x][1:v]paletteuse=dither=bayer:bayer_scale=4" "${gif}"`);

  rendered += 1;
}

if (rendered === 0) {
  console.error('No manifests found — run the capture stage first.');
  process.exit(1);
}
console.log(`Done: rendered ${rendered} locale(s) -> ${OUT}`);
