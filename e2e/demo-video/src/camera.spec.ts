import { describe, it, expect } from 'vitest';
import {
  buildTimeline,
  cameraAt,
  cameraTargetFor,
  cursorAt,
  type CameraOptions,
} from './camera';
import type { Shot } from './manifest-types';

const OPTS: CameraOptions = {
  srcWidth: 3200,
  srcHeight: 2000,
  outWidth: 1920,
  outHeight: 1200,
  fps: 30,
  transitionFrames: 12,
  zMax: 1.6,
  focusFill: 0.5,
  moveSeconds: 0.6, // -> 18 move frames
};

// k = outW/srcW = 0.6. A 100x100 src focus centered at (1600,1000).
const centeredFocus = (): Shot => ({
  scene: 'x',
  index: 0,
  hold: 3,
  image: 'a.png',
  focus: { x: 1550, y: 950, w: 100, h: 100 },
});

describe('cameraTargetFor', () => {
  it('is identity when the shot has no focus', () => {
    const t = cameraTargetFor({ scene: 'x', index: 0, hold: 2, image: 'a.png' }, OPTS);
    expect(t).toEqual({ scale: 1, tx: 0, ty: 0 });
  });

  it('zooms (clamped to zMax) and centers a small focus rect on the frame', () => {
    const t = cameraTargetFor(centeredFocus(), OPTS);
    expect(t.scale).toBeCloseTo(1.6, 5);
    // z*(center+t) must land on the output frame center (960,600).
    expect(t.scale * (960 + t.tx)).toBeCloseTo(960, 3);
    expect(t.scale * (600 + t.ty)).toBeCloseTo(600, 3);
  });

  it('clamps translate so the scaled frame always covers the viewport (no black edges)', () => {
    // Focus hard against the top-left corner: ideal pan would expose edges; the
    // translate must clamp into [outW(1/z-1), 0] x [outH(1/z-1), 0].
    const corner: Shot = {
      scene: 'x',
      index: 0,
      hold: 3,
      image: 'a.png',
      focus: { x: 0, y: 0, w: 100, h: 100 },
    };
    const t = cameraTargetFor(corner, OPTS);
    expect(t.tx).toBeLessThanOrEqual(0);
    expect(t.ty).toBeLessThanOrEqual(0);
    expect(t.tx).toBeGreaterThanOrEqual(OPTS.outWidth * (1 / t.scale - 1) - 1e-6);
    expect(t.ty).toBeGreaterThanOrEqual(OPTS.outHeight * (1 / t.scale - 1) - 1e-6);
  });
});

describe('cameraAt', () => {
  const shots = [
    { scene: 'a', index: 0, hold: 3, image: 'a.png' } as Shot, // no focus -> identity
    centeredFocus(), // focus -> zoom
  ];
  const tl = buildTimeline(shots, OPTS);

  it('holds the first shot at its own target (no prior to move from)', () => {
    expect(cameraAt(tl, 0)).toEqual({ scale: 1, tx: 0, ty: 0 });
  });

  it('begins shot 1 at the previous target and eases to the new one', () => {
    const w1 = tl.windows[1];
    const atStart = cameraAt(tl, w1.start);
    expect(atStart.scale).toBeCloseTo(1, 5); // still at shot 0's identity
    const settled = cameraAt(tl, w1.start + tl.moveFrames + 2);
    expect(settled.scale).toBeCloseTo(1.6, 5); // fully arrived at shot 1
  });
});

describe('cursorAt', () => {
  const withCursor = (cx: number, cy: number, click = false): Shot => ({
    scene: 'x',
    index: 0,
    hold: 3,
    image: 'a.png',
    cursor: { x: cx, y: cy },
    click,
  });

  it('hides the cursor on shots that have none', () => {
    const tl = buildTimeline([{ scene: 'x', index: 0, hold: 2, image: 'a.png' } as Shot], OPTS);
    expect(cursorAt(tl, 0).opacity).toBe(0);
  });

  it('travels from the previous cursor to the current one (output px)', () => {
    const shots = [withCursor(0, 0), withCursor(1600, 1000)];
    const tl = buildTimeline(shots, OPTS);
    const w1 = tl.windows[1];
    const arriving = cursorAt(tl, w1.start); // start of move == prev point (0,0)
    expect(arriving.x).toBeCloseTo(0, 3);
    expect(arriving.opacity).toBe(1);
    const settled = cursorAt(tl, w1.start + tl.moveFrames + 2); // out px = src*0.6
    expect(settled.x).toBeCloseTo(960, 1);
    expect(settled.y).toBeCloseTo(600, 1);
  });

  it('fades the cursor in when it appears with no prior position', () => {
    const shots = [
      { scene: 'x', index: 0, hold: 2, image: 'a.png' } as Shot, // no cursor
      withCursor(1600, 1000),
    ];
    const tl = buildTimeline(shots, OPTS);
    const w1 = tl.windows[1];
    expect(cursorAt(tl, w1.start).opacity).toBeLessThan(0.5);
    expect(cursorAt(tl, w1.start + tl.moveFrames + 2).opacity).toBe(1);
  });

  it('plays a click ripple after the cursor lands', () => {
    const shots = [withCursor(100, 100), withCursor(1600, 1000, true)];
    const tl = buildTimeline(shots, OPTS);
    const w1 = tl.windows[1];
    expect(cursorAt(tl, w1.start + tl.moveFrames).clickProgress).toBeCloseTo(0, 2); // just landed
    expect(cursorAt(tl, w1.start + tl.moveFrames + tl.rippleFrames).clickProgress).toBeCloseTo(1, 2);
    // a non-click shot never ripples
    const tl2 = buildTimeline([withCursor(100, 100), withCursor(1600, 1000, false)], OPTS);
    expect(cursorAt(tl2, tl2.windows[1].start + tl2.moveFrames + 5).clickProgress).toBe(0);
  });
});
