import type { Shot } from './manifest-types';

/** Scenes that make the 60-90s highlight cut, in play order. */
export const HIGHLIGHT_SCENES = [
  '02-providers',
  '04-assistant-create-agent',
  '05b-create-agent',
  '06-mcp',
  '09-sandbox',
  '12-eval',
];

/** Tight teaser cut for the README GIF: a 3-beat arc that shows the focus-moving
 *  R3 camera at its best, kept short so the GIF stays small and legible. The full
 *  highlight story lives in the MP4 (DemoHighlight / DemoLong). */
export const GIF_SCENES = ['02-providers', '05b-create-agent', '12-eval'];

export function selectHighlight(shots: Shot[], scenes: string[] = HIGHLIGHT_SCENES): Shot[] {
  const set = new Set(scenes);
  return shots.filter((s) => set.has(s.scene));
}

export interface GifBeatOptions {
  /** Hold for a beat the viewer reads (caption / payoff). */
  holdSeconds?: number;
  /** Hold for a transient interaction beat (cursor travel / button click). */
  actionHoldSeconds?: number;
  scenes?: string[];
}

/**
 * The GIF cut: the COMPLETE story of each highlight scene, in capture order, so
 * the loop reads as a continuous flow rather than disconnected snapshots. Unlike
 * the old payoff-only slideshow, this keeps the interaction beats — sidebar
 * navigation, button clicks, the cursor resting on a control — with their
 * `cursor` / `click` / `focus` intact, because under the R3 camera those are
 * exactly what drives the visible mouse travel, the click ripple, and the
 * highlight ring. The camera supplies the zoom, so no static `zoom` is baked in.
 *
 * Per scene we keep every image-bearing beat in order, only collapsing runs of
 * dead-duplicate beats (same caption, no cursor, no click) so the loop never
 * stalls on a frame that says nothing new. Interaction beats are held briefly
 * (the action should feel live); caption/payoff beats are held long enough to
 * read.
 */
export function selectGifShots(shots: Shot[], opts: GifBeatOptions = {}): Shot[] {
  const holdSeconds = opts.holdSeconds ?? 1.6;
  const actionHold = opts.actionHoldSeconds ?? 0.9;
  const scenes = opts.scenes ?? HIGHLIGHT_SCENES;

  const out: Shot[] = [];
  for (const scene of scenes) {
    const beats = shots.filter((s) => s.scene === scene && s.image);
    // Collapse only adjacent beats that add nothing: identical caption AND both
    // static (no cursor/click). Interaction beats are always kept.
    const kept = beats.filter((b, i) => {
      if (i === 0) return true;
      const p = beats[i - 1];
      const dead =
        !b.cursor && !b.click && !p.cursor && !p.click && b.caption === p.caption;
      return !dead;
    });
    for (const b of kept) {
      const isAction = b.click === true || b.cursor != null;
      out.push({
        ...b,
        hold: isAction ? actionHold : holdSeconds,
        transition: b.transition ?? 'fade',
      });
    }
  }
  return out;
}
