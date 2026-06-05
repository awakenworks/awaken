import type { Shot } from './manifest-types';
import { shotWindows, type ShotWindow } from './timing';

/**
 * R3 "camera follows cursor" timeline.
 *
 * Background, highlight, and cursor all render inside one CameraRig that applies
 * a single `scale(s) translate(tx,ty)` transform (origin 0,0) in OUTPUT pixels.
 * Because the three layers share the transform, the cursor and the highlight
 * ring stay glued to the element no matter how the camera pushes or pans — the
 * alignment hazard that kept zoom and cursor from ever co-existing in the old
 * flat model simply cannot occur here.
 *
 * Coordinates: source pixels (the manifest's DSF-scaled viewport space) map to
 * output pixels by the uniform base factor `k = outWidth / srcWidth` (capture
 * and output share aspect ratio). Cursor/focus arrive in source px; everything
 * below works in output px after multiplying by k.
 */

export interface CameraState {
  scale: number;
  tx: number;
  ty: number;
}

export interface CursorState {
  x: number;
  y: number;
  opacity: number;
  /** 0→1 ripple progress for a click; 0 when the shot is not a click. */
  clickProgress: number;
}

interface Point {
  x: number;
  y: number;
}

export interface CameraOptions {
  srcWidth: number;
  srcHeight: number;
  outWidth: number;
  outHeight: number;
  fps: number;
  transitionFrames: number;
  /** Hard zoom ceiling — keeps motion calm and avoids over-magnifying. */
  zMax?: number;
  /** Fraction of the frame a focus rect should span when settled. */
  focusFill?: number;
  /** Camera + cursor travel time at the start of each shot. */
  moveSeconds?: number;
  /** Click ripple duration. */
  rippleSeconds?: number;
}

export interface Timeline {
  windows: ShotWindow[];
  targets: CameraState[];
  cursors: (Point | null)[];
  clicks: boolean[];
  moveFrames: number;
  rippleFrames: number;
}

const DEFAULTS = { zMax: 1.6, focusFill: 0.5, moveSeconds: 0.6, rippleSeconds: 0.5 };

function clamp(v: number, lo: number, hi: number): number {
  return Math.min(hi, Math.max(lo, v));
}

function clamp01(v: number): number {
  return clamp(v, 0, 1);
}

/** Symmetric cubic ease-in-out — calm starts and stops, no overshoot. */
export function easeInOut(t: number): number {
  const x = clamp01(t);
  return x < 0.5 ? 4 * x * x * x : 1 - Math.pow(-2 * x + 2, 3) / 2;
}

function lerp(a: number, b: number, t: number): number {
  return a + (b - a) * t;
}

/** The settled camera framing for one shot (no time component). */
export function cameraTargetFor(shot: Shot, opts: CameraOptions): CameraState {
  const { srcWidth, outWidth, outHeight } = opts;
  const zMax = opts.zMax ?? DEFAULTS.zMax;
  const focusFill = opts.focusFill ?? DEFAULTS.focusFill;
  const k = outWidth / srcWidth;

  if (!shot.focus) return { scale: 1, tx: 0, ty: 0 };

  const fw = shot.focus.w * k;
  const fh = shot.focus.h * k;
  const cx = (shot.focus.x + shot.focus.w / 2) * k;
  const cy = (shot.focus.y + shot.focus.h / 2) * k;

  const scale = clamp(
    Math.min((outWidth * focusFill) / fw, (outHeight * focusFill) / fh),
    1,
    zMax,
  );

  // Pan so the focus centre lands on the frame centre: z*(c+t)=center.
  // Then clamp into the cover range so the scaled frame never exposes an edge.
  const txRaw = outWidth / 2 / scale - cx;
  const tyRaw = outHeight / 2 / scale - cy;
  const tx = clamp(txRaw, outWidth * (1 / scale - 1), 0);
  const ty = clamp(tyRaw, outHeight * (1 / scale - 1), 0);
  return { scale, tx, ty };
}

function cursorPointFor(shot: Shot, opts: CameraOptions): Point | null {
  if (!shot.cursor) return null;
  const k = opts.outWidth / opts.srcWidth;
  return { x: shot.cursor.x * k, y: shot.cursor.y * k };
}

export function buildTimeline(shots: Shot[], opts: CameraOptions): Timeline {
  const windows = shotWindows(shots, opts.fps, opts.transitionFrames);
  return {
    windows,
    targets: shots.map((s) => cameraTargetFor(s, opts)),
    cursors: shots.map((s) => cursorPointFor(s, opts)),
    clicks: shots.map((s) => s.click === true),
    moveFrames: Math.max(1, Math.round((opts.moveSeconds ?? DEFAULTS.moveSeconds) * opts.fps)),
    rippleFrames: Math.max(1, Math.round((opts.rippleSeconds ?? DEFAULTS.rippleSeconds) * opts.fps)),
  };
}

/** Index of the shot whose window contains `frame` (clamped to range). */
export function activeShotIndex(tl: Timeline, frame: number): number {
  const last = tl.windows.length - 1;
  if (frame <= tl.windows[0].start) return 0;
  for (let i = 0; i <= last; i++) {
    if (frame < tl.windows[i].end) return i;
  }
  return last;
}

export function cameraAt(tl: Timeline, frame: number): CameraState {
  const i = activeShotIndex(tl, frame);
  const to = tl.targets[i];
  const from = i > 0 ? tl.targets[i - 1] : to;
  const local = frame - tl.windows[i].start;
  const t = easeInOut(clamp01(local / tl.moveFrames));
  return {
    scale: lerp(from.scale, to.scale, t),
    tx: lerp(from.tx, to.tx, t),
    ty: lerp(from.ty, to.ty, t),
  };
}

export function cursorAt(tl: Timeline, frame: number): CursorState {
  const i = activeShotIndex(tl, frame);
  const cur = tl.cursors[i];
  const prev = i > 0 ? tl.cursors[i - 1] : null;
  const local = frame - tl.windows[i].start;
  const moveT = easeInOut(clamp01(local / tl.moveFrames));

  if (!cur) {
    // No target this shot: fade out in place (last known point), stay hidden.
    return { x: prev?.x ?? 0, y: prev?.y ?? 0, opacity: prev ? 1 - moveT : 0, clickProgress: 0 };
  }

  const x = prev ? lerp(prev.x, cur.x, moveT) : cur.x;
  const y = prev ? lerp(prev.y, cur.y, moveT) : cur.y;
  // Already visible -> stay solid while travelling; otherwise fade in on arrival.
  const opacity = prev ? 1 : moveT;

  let clickProgress = 0;
  if (tl.clicks[i]) {
    const rippleLocal = local - tl.moveFrames;
    clickProgress = clamp01(rippleLocal / tl.rippleFrames);
  }
  return { x, y, opacity, clickProgress };
}
