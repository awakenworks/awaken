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
 * The GIF beats: a faithful, condensed cut of the highlight that keeps the
 * REAL video captions (so the GIF corresponds to the video) and shows several
 * beats per step (intro + payoff) so each step's story is complete — not a
 * single disconnected snapshot. Per scene we keep the narrated beats (image +
 * caption), drop transient cursor-only frames and consecutive duplicate
 * captions, and trim to the first beat plus the final payoff beats. Beats are
 * held long enough to read and zoomed a touch so the UI is legible.
 */
export function selectGifShots(
  shots: Shot[],
  holdSeconds = 1.5,
  zoom?: number,
  maxPerScene = 3,
  scenes: string[] = HIGHLIGHT_SCENES,
): Shot[] {
  const out: Shot[] = [];
  for (const scene of scenes) {
    // Narration beats only: scene-title / caption / payoff shots. Exclude
    // `click` frames (nav + button presses) — those are transient interactions
    // whose caption is the persisted one from the previous beat/scene, so they
    // would show a caption that doesn't match the frame.
    const beats = shots.filter((s) => s.scene === scene && s.image && s.caption && !s.click);
    // Drop consecutive duplicate captions (e.g. "reply…" then "reply").
    const dedup = beats.filter((b, i) => i === 0 || b.caption !== beats[i - 1].caption);
    if (dedup.length === 0) continue;
    // Intro beat + the final payoff beats, so each step reads intent -> result.
    const chosen =
      dedup.length <= maxPerScene
        ? dedup
        : [dedup[0], ...dedup.slice(dedup.length - (maxPerScene - 1))];
    for (const b of chosen) {
      out.push({
        ...b,
        hold: holdSeconds,
        transition: 'fade',
        cursor: undefined,
        click: false,
        zoom: zoom ?? b.zoom,
      });
    }
  }
  return out;
}
