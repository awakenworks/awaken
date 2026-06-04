import type { Shot } from './manifest-types';

/** Scenes that make the 60-90s highlight cut, in play order. */
export const HIGHLIGHT_SCENES = [
  '02-providers',
  '04-assistant-create-agent',
  '06-mcp',
  '09-sandbox',
  '12-eval',
];

export function selectHighlight(shots: Shot[], scenes: string[] = HIGHLIGHT_SCENES): Shot[] {
  const set = new Set(scenes);
  return shots.filter((s) => set.has(s.scene));
}

/**
 * One readable "payoff" beat per highlight scene for the GIF: the last
 * caption-bearing image shot of each scene (the settled, narrated result),
 * held long enough to read. Far fewer, slower beats than the full highlight —
 * so a looping GIF conveys each scene's intent instead of flashing past.
 */
export function selectGifShots(
  shots: Shot[],
  holdSeconds = 2.4,
  scenes: string[] = HIGHLIGHT_SCENES,
): Shot[] {
  const out: Shot[] = [];
  for (const scene of scenes) {
    const inScene = shots.filter((s) => s.scene === scene && s.image);
    if (inScene.length === 0) continue;
    const captioned = inScene.filter((s) => s.caption);
    const pool = captioned.length > 0 ? captioned : inScene;
    const pick = pool[pool.length - 1];
    out.push({ ...pick, hold: holdSeconds, transition: 'fade', cursor: undefined, click: false });
  }
  return out;
}
