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
