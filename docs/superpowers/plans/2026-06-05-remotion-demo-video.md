# Remotion Demo Video Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Re-produce the awaken-admin-console promo (MP4 + GIF, EN/ZH) with sharp text/lines by capturing crisp 2× screenshots from a real-Gemini E2E run and compositing them with Remotion.

**Architecture:** Two stages. ① The existing Playwright demo spec re-runs the real flow (real Gemini, all scenes, QA instrumentation intact) and, at each beat, writes a 2× lossless viewport PNG plus a manifest entry. ② A standalone Remotion project (`e2e/demo-video/`) reads the manifest + PNGs and renders `DemoLong` and `DemoHighlight` compositions per locale, drawing cursor/caption/focus-zoom/transitions/title-cards as vectors. No lossy continuous encode sits between the source pixels and the output.

**Tech Stack:** Playwright (capture), Remotion 4 + React 18 (compose), Vitest (unit tests for pure logic), ffmpeg/Remotion GIF codec (export).

**Dimensions (locked):** viewport `1600×1000`, `deviceScaleFactor: 2` → source PNGs `3200×2000`; Remotion composition `1920×1200` @ 30fps (all 16:10).

---

## File Structure

Compose stage — new, isolated package `e2e/demo-video/`:
- `package.json`, `tsconfig.json`, `remotion.config.ts` — project scaffold.
- `src/manifest-types.ts` — manifest types + `parseManifest` validator (pure, tested).
- `src/timing.ts` — `holdFrames`, `totalDurationInFrames` (pure, tested).
- `src/highlight.ts` — `selectHighlight` scene filter (pure, tested).
- `src/components/Caption.tsx`, `Cursor.tsx`, `TitleCard.tsx`, `Shot.tsx` — vector overlays + image/Ken-Burns.
- `src/Demo.tsx` — builds a `TransitionSeries` from shots.
- `src/Root.tsx`, `src/index.ts` — composition registration + metadata.
- `scripts/render-all.mjs` — renders 4 MP4 + 2 GIF from the two manifests.
- `src/*.spec.ts` — Vitest unit tests.

Capture stage — modify existing files:
- `e2e/playwright.demo.config.ts` — video off (frames are the source now).
- `e2e/tests/demo-helpers.ts` — remove on-page cursor/caption injection; add the shot machinery (`shot`, `initCapture`, `setCurrentScene`, `writeManifest`, `targetOf`) and re-point `caption`/`scene`/`point` at it.
- `e2e/tests/admin-demo.spec.ts` — wire capture lifecycle; make `clickByName`/`goSidebar`/`clickTab`/`titleCard` emit cursor/title shots; add "money-shot" captures after live-LLM replies.
- `e2e/package.json` — add `capture:demo` script.

---

## Task 1: Scaffold the Remotion project

**Files:**
- Create: `e2e/demo-video/package.json`
- Create: `e2e/demo-video/tsconfig.json`
- Create: `e2e/demo-video/remotion.config.ts`

- [ ] **Step 1: Create `e2e/demo-video/package.json`**

```json
{
  "name": "awaken-demo-video",
  "private": true,
  "version": "0.0.0",
  "type": "module",
  "scripts": {
    "studio": "remotion studio src/index.ts",
    "test": "vitest run",
    "render:all": "node scripts/render-all.mjs"
  }
}
```

- [ ] **Step 2: Install pinned, version-matched deps**

Remotion requires `remotion` and every `@remotion/*` package to share the exact same version. Install with `@latest` so npm writes one consistent set into `package.json`:

Run:
```bash
cd e2e/demo-video
npm install --save react@^18.3.1 react-dom@^18.3.1
npm install --save remotion@latest @remotion/cli@latest @remotion/transitions@latest
npm install --save-dev @types/react@^18.3.0 @types/react-dom@^18.3.0 typescript@^5.4.0 vitest@^2.0.0
```
Expected: `node_modules/` created; `package.json` now lists matching `remotion` / `@remotion/*` versions.

- [ ] **Step 3: Create `e2e/demo-video/tsconfig.json`**

```json
{
  "compilerOptions": {
    "target": "ES2020",
    "module": "ESNext",
    "moduleResolution": "Bundler",
    "jsx": "react-jsx",
    "strict": true,
    "esModuleInterop": true,
    "skipLibCheck": true,
    "lib": ["ES2020", "DOM"],
    "types": ["react", "node"]
  },
  "include": ["src", "scripts"]
}
```

- [ ] **Step 4: Create `e2e/demo-video/remotion.config.ts`**

```ts
import { Config } from '@remotion/cli/config';

Config.setVideoImageFormat('png');
Config.setConcurrency(4);
Config.setChromiumDisableWebSecurity(true);
```

- [ ] **Step 5: Add a gitignore so frames/renders never get staged**

Create `e2e/demo-video/.gitignore`:
```
node_modules/
out/
```

- [ ] **Step 6: Commit**

```bash
cd /home/chaizhenhua/Codes/awaken
git add e2e/demo-video/package.json e2e/demo-video/package-lock.json e2e/demo-video/tsconfig.json e2e/demo-video/remotion.config.ts e2e/demo-video/.gitignore
git commit -m "🎬 chore(demo-video): scaffold Remotion compositor package"
```

---

## Task 2: Manifest types + validator (TDD)

**Files:**
- Create: `e2e/demo-video/src/manifest-types.ts`
- Test: `e2e/demo-video/src/manifest-types.spec.ts`

- [ ] **Step 1: Write the failing test**

`e2e/demo-video/src/manifest-types.spec.ts`:
```ts
import { describe, it, expect } from 'vitest';
import { parseManifest } from './manifest-types';

const valid = {
  locale: 'en',
  width: 3200,
  height: 2000,
  shots: [
    { scene: '01-intro', hold: 2.6, title: 'Awaken', subtitle: 'x' },
    { scene: '02-providers', hold: 2.2, image: '02-providers-01.png', caption: 'Providers', cursor: { x: 10, y: 20 }, click: true },
  ],
};

describe('parseManifest', () => {
  it('accepts a valid manifest and defaults transition to fade', () => {
    const m = parseManifest(valid);
    expect(m.shots).toHaveLength(2);
    expect(m.shots[1].transition).toBe('fade');
    expect(m.shots[1].click).toBe(true);
    expect(m.shots[0].image).toBeUndefined();
  });

  it('rejects empty shots', () => {
    expect(() => parseManifest({ ...valid, shots: [] })).toThrow(/no shots/);
  });

  it('rejects a bad locale', () => {
    expect(() => parseManifest({ ...valid, locale: 'fr' })).toThrow(/locale/);
  });

  it('rejects a shot with neither image nor title', () => {
    expect(() => parseManifest({ ...valid, shots: [{ scene: 'x', hold: 1 }] })).toThrow(/image or title/);
  });
});
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cd e2e/demo-video && npx vitest run src/manifest-types.spec.ts`
Expected: FAIL — `Failed to resolve import './manifest-types'`.

- [ ] **Step 3: Write minimal implementation**

`e2e/demo-video/src/manifest-types.ts`:
```ts
export type Transition = 'fade' | 'slide' | 'cut';
export interface Point { x: number; y: number }
export interface Rect { x: number; y: number; w: number; h: number }

export interface Shot {
  scene: string;
  index: number;
  image?: string;
  caption?: string;
  hold: number;
  cursor?: Point;
  click?: boolean;
  focus?: Rect;
  transition?: Transition;
  title?: string;
  subtitle?: string;
}

export interface Manifest {
  locale: 'en' | 'zh-CN';
  width: number;
  height: number;
  shots: Shot[];
}

function isPoint(v: any): v is Point {
  return v && typeof v.x === 'number' && typeof v.y === 'number';
}
function isRect(v: any): v is Rect {
  return v && typeof v.x === 'number' && typeof v.y === 'number'
    && typeof v.w === 'number' && typeof v.h === 'number';
}

export function parseManifest(raw: unknown): Manifest {
  const m = raw as any;
  if (!m || typeof m !== 'object') throw new Error('manifest: not an object');
  if (m.locale !== 'en' && m.locale !== 'zh-CN') throw new Error('manifest: bad locale');
  if (typeof m.width !== 'number' || typeof m.height !== 'number') {
    throw new Error('manifest: bad dimensions');
  }
  if (!Array.isArray(m.shots) || m.shots.length === 0) throw new Error('manifest: no shots');

  const shots: Shot[] = m.shots.map((s: any, i: number) => {
    if (typeof s.scene !== 'string') throw new Error(`shot ${i}: missing scene`);
    if (typeof s.hold !== 'number' || s.hold <= 0) throw new Error(`shot ${i}: bad hold`);
    if (s.image == null && s.title == null) throw new Error(`shot ${i}: needs image or title`);
    if (s.cursor != null && !isPoint(s.cursor)) throw new Error(`shot ${i}: bad cursor`);
    if (s.focus != null && !isRect(s.focus)) throw new Error(`shot ${i}: bad focus`);
    return {
      scene: s.scene,
      index: i,
      image: s.image ?? undefined,
      caption: s.caption ?? undefined,
      hold: s.hold,
      cursor: s.cursor ?? undefined,
      click: s.click === true,
      focus: s.focus ?? undefined,
      transition: (s.transition as Transition) ?? 'fade',
      title: s.title ?? undefined,
      subtitle: s.subtitle ?? undefined,
    };
  });
  return { locale: m.locale, width: m.width, height: m.height, shots };
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cd e2e/demo-video && npx vitest run src/manifest-types.spec.ts`
Expected: PASS (4 tests).

- [ ] **Step 5: Commit**

```bash
cd /home/chaizhenhua/Codes/awaken
git add e2e/demo-video/src/manifest-types.ts e2e/demo-video/src/manifest-types.spec.ts
git commit -m "🎬 feat(demo-video): manifest types + validator"
```

---

## Task 3: Timing math (TDD)

**Files:**
- Create: `e2e/demo-video/src/timing.ts`
- Test: `e2e/demo-video/src/timing.spec.ts`

- [ ] **Step 1: Write the failing test**

`e2e/demo-video/src/timing.spec.ts`:
```ts
import { describe, it, expect } from 'vitest';
import { holdFrames, totalDurationInFrames } from './timing';
import type { Shot } from './manifest-types';

const s = (hold: number, transition: Shot['transition']): Shot =>
  ({ scene: 'x', index: 0, hold, transition, image: 'a.png' });

describe('timing', () => {
  it('holdFrames rounds seconds to frames (min 1)', () => {
    expect(holdFrames(s(2, 'fade'), 30)).toBe(60);
    expect(holdFrames(s(0.01, 'fade'), 30)).toBe(1);
  });

  it('totalDurationInFrames subtracts one overlap per non-cut transition', () => {
    const shots = [s(2, 'fade'), s(3, 'fade'), s(1, 'cut')];
    // 60 + 90 + 30 = 180 holds; one fade overlap (index 1) of 12 → 168
    expect(totalDurationInFrames(shots, 30, 12)).toBe(168);
  });

  it('never returns less than 1', () => {
    expect(totalDurationInFrames([s(0.01, 'fade')], 30, 12)).toBe(1);
  });
});
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cd e2e/demo-video && npx vitest run src/timing.spec.ts`
Expected: FAIL — cannot resolve `./timing`.

- [ ] **Step 3: Write minimal implementation**

`e2e/demo-video/src/timing.ts`:
```ts
import type { Shot } from './manifest-types';

export function holdFrames(shot: Shot, fps: number): number {
  return Math.max(1, Math.round(shot.hold * fps));
}

export function totalDurationInFrames(
  shots: Shot[],
  fps: number,
  transitionFrames: number,
): number {
  const holds = shots.reduce((acc, s) => acc + holdFrames(s, fps), 0);
  const overlaps = shots.slice(1).filter((s) => (s.transition ?? 'fade') !== 'cut').length;
  return Math.max(1, holds - overlaps * transitionFrames);
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cd e2e/demo-video && npx vitest run src/timing.spec.ts`
Expected: PASS (3 tests).

- [ ] **Step 5: Commit**

```bash
cd /home/chaizhenhua/Codes/awaken
git add e2e/demo-video/src/timing.ts e2e/demo-video/src/timing.spec.ts
git commit -m "🎬 feat(demo-video): composition timing math"
```

---

## Task 4: Highlight selection (TDD)

**Files:**
- Create: `e2e/demo-video/src/highlight.ts`
- Test: `e2e/demo-video/src/highlight.spec.ts`

- [ ] **Step 1: Write the failing test**

`e2e/demo-video/src/highlight.spec.ts`:
```ts
import { describe, it, expect } from 'vitest';
import { selectHighlight, HIGHLIGHT_SCENES } from './highlight';
import type { Shot } from './manifest-types';

const s = (scene: string): Shot => ({ scene, index: 0, hold: 2, image: 'a.png' });

describe('selectHighlight', () => {
  it('keeps only highlight scenes, preserving order', () => {
    const shots = [s('01-intro'), s('02-providers'), s('05-agents-list'), s('09-sandbox'), s('12-eval')];
    const out = selectHighlight(shots);
    expect(out.map((x) => x.scene)).toEqual(['02-providers', '09-sandbox', '12-eval']);
  });

  it('exposes the default highlight scene list', () => {
    expect(HIGHLIGHT_SCENES).toContain('04-assistant-create-agent');
  });
});
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cd e2e/demo-video && npx vitest run src/highlight.spec.ts`
Expected: FAIL — cannot resolve `./highlight`.

- [ ] **Step 3: Write minimal implementation**

`e2e/demo-video/src/highlight.ts`:
```ts
import type { Shot } from './manifest-types';

/** Scenes that make the 60–90s highlight cut, in play order. */
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
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cd e2e/demo-video && npx vitest run src/highlight.spec.ts`
Expected: PASS (2 tests).

- [ ] **Step 5: Commit**

```bash
cd /home/chaizhenhua/Codes/awaken
git add e2e/demo-video/src/highlight.ts e2e/demo-video/src/highlight.spec.ts
git commit -m "🎬 feat(demo-video): highlight scene selection"
```

---

## Task 5: Vector overlay components

These render visually and are verified later via `remotion still` (Task 9), not unit tests.

**Files:**
- Create: `e2e/demo-video/src/components/Caption.tsx`
- Create: `e2e/demo-video/src/components/Cursor.tsx`
- Create: `e2e/demo-video/src/components/TitleCard.tsx`

- [ ] **Step 1: Create `Caption.tsx`**

```tsx
import React from 'react';
import { useCurrentFrame, useVideoConfig, spring, interpolate } from 'remotion';

export const Caption: React.FC<{ text: string }> = ({ text }) => {
  const frame = useCurrentFrame();
  const { fps, durationInFrames } = useVideoConfig();
  const enter = spring({ frame, fps, config: { damping: 200 }, durationInFrames: 10 });
  const exit = interpolate(
    frame,
    [durationInFrames - 8, durationInFrames],
    [1, 0],
    { extrapolateLeft: 'clamp', extrapolateRight: 'clamp' },
  );
  const opacity = Math.min(enter, exit);
  const y = interpolate(enter, [0, 1], [24, 0]);
  return (
    <div
      style={{
        position: 'absolute',
        left: '50%',
        bottom: 48,
        transform: `translateX(-50%) translateY(${y}px)`,
        opacity,
        maxWidth: '80%',
        padding: '16px 30px',
        borderRadius: 18,
        background: 'rgba(2,6,23,0.86)',
        color: '#e2e8f0',
        font: '600 30px/1.4 system-ui,-apple-system,sans-serif',
        letterSpacing: 0.3,
        border: '1px solid rgba(56,189,248,0.45)',
        boxShadow: '0 14px 50px rgba(0,0,0,0.5)',
        textAlign: 'center',
        backdropFilter: 'blur(8px)',
      }}
    >
      {text}
    </div>
  );
};
```

- [ ] **Step 2: Create `Cursor.tsx`**

The cursor coordinates are already in composition pixels (scaled by the caller). Each shot is a still, so the cursor pops in with a small spring; a click adds an expanding ripple.

```tsx
import React from 'react';
import { useCurrentFrame, useVideoConfig, spring, interpolate } from 'remotion';

export const Cursor: React.FC<{ x: number; y: number; click: boolean }> = ({ x, y, click }) => {
  const frame = useCurrentFrame();
  const { fps } = useVideoConfig();
  const appear = spring({ frame, fps, config: { damping: 200 }, durationInFrames: 12 });
  const size = 26;
  const ripple = click ? interpolate(frame, [4, 22], [0, 1], { extrapolateRight: 'clamp' }) : 0;
  return (
    <>
      {click ? (
        <div
          style={{
            position: 'absolute',
            left: x - 30,
            top: y - 30,
            width: 60,
            height: 60,
            borderRadius: '50%',
            border: '2px solid rgba(56,189,248,0.8)',
            transform: `scale(${ripple})`,
            opacity: 1 - ripple,
          }}
        />
      ) : null}
      <div
        style={{
          position: 'absolute',
          left: x - size / 2,
          top: y - size / 2,
          width: size,
          height: size,
          borderRadius: '50%',
          background: 'rgba(56,189,248,0.45)',
          border: '2px solid rgba(56,189,248,0.95)',
          boxShadow: '0 0 16px 4px rgba(56,189,248,0.55)',
          transform: `scale(${appear})`,
        }}
      />
    </>
  );
};
```

- [ ] **Step 3: Create `TitleCard.tsx`**

```tsx
import React from 'react';
import { AbsoluteFill, useCurrentFrame, useVideoConfig, spring, interpolate } from 'remotion';

export const TitleCard: React.FC<{ title: string; subtitle: string }> = ({ title, subtitle }) => {
  const frame = useCurrentFrame();
  const { fps, durationInFrames } = useVideoConfig();
  const enter = spring({ frame, fps, config: { damping: 200 }, durationInFrames: 18 });
  const exit = interpolate(
    frame,
    [durationInFrames - 12, durationInFrames],
    [1, 0],
    { extrapolateLeft: 'clamp', extrapolateRight: 'clamp' },
  );
  const opacity = Math.min(enter, exit);
  return (
    <AbsoluteFill
      style={{
        background: 'radial-gradient(ellipse at center,#0b1220,#020617)',
        alignItems: 'center',
        justifyContent: 'center',
        gap: 18,
        opacity,
      }}
    >
      <div
        style={{
          fontSize: 84,
          fontWeight: 800,
          letterSpacing: -1,
          fontFamily: 'system-ui,-apple-system,sans-serif',
          background: 'linear-gradient(90deg,#38bdf8,#a78bfa)',
          WebkitBackgroundClip: 'text',
          backgroundClip: 'text',
          color: 'transparent',
          transform: `scale(${interpolate(enter, [0, 1], [0.96, 1])})`,
        }}
      >
        {title}
      </div>
      <div
        style={{
          fontSize: 34,
          color: '#94a3b8',
          fontWeight: 500,
          maxWidth: '70%',
          textAlign: 'center',
          fontFamily: 'system-ui,-apple-system,sans-serif',
        }}
      >
        {subtitle}
      </div>
    </AbsoluteFill>
  );
};
```

- [ ] **Step 4: Type-check**

Run: `cd e2e/demo-video && npx tsc --noEmit`
Expected: no errors.

- [ ] **Step 5: Commit**

```bash
cd /home/chaizhenhua/Codes/awaken
git add e2e/demo-video/src/components/Caption.tsx e2e/demo-video/src/components/Cursor.tsx e2e/demo-video/src/components/TitleCard.tsx
git commit -m "🎬 feat(demo-video): caption, cursor, title-card overlays"
```

---

## Task 6: Shot view (image + Ken Burns + overlays)

**Files:**
- Create: `e2e/demo-video/src/components/Shot.tsx`

- [ ] **Step 1: Create `Shot.tsx`**

Maps source-pixel coords to composition pixels via `scaleX/scaleY`. The Ken-Burns zoom wraps only the image so the cursor/caption layers stay pixel-stable.

```tsx
import React from 'react';
import {
  AbsoluteFill,
  Img,
  staticFile,
  useCurrentFrame,
  useVideoConfig,
  interpolate,
} from 'remotion';
import type { Shot } from '../manifest-types';
import { Caption } from './Caption';
import { Cursor } from './Cursor';
import { TitleCard } from './TitleCard';

export const ShotView: React.FC<{ shot: Shot; srcWidth: number; srcHeight: number }> = ({
  shot,
  srcWidth,
  srcHeight,
}) => {
  const frame = useCurrentFrame();
  const { width, height, durationInFrames } = useVideoConfig();

  if (shot.title) {
    return <TitleCard title={shot.title} subtitle={shot.subtitle ?? ''} />;
  }

  const scaleX = width / srcWidth;
  const scaleY = height / srcHeight;
  const progress = interpolate(frame, [0, durationInFrames], [0, 1], {
    extrapolateRight: 'clamp',
  });

  let transform = `scale(${interpolate(progress, [0, 1], [1, 1.04])})`;
  let transformOrigin = 'center center';
  if (shot.focus) {
    const cxPct = (((shot.focus.x + shot.focus.w / 2) * scaleX) / width) * 100;
    const cyPct = (((shot.focus.y + shot.focus.h / 2) * scaleY) / height) * 100;
    transformOrigin = `${cxPct}% ${cyPct}%`;
    transform = `scale(${interpolate(progress, [0, 1], [1, 1.12])})`;
  }

  return (
    <AbsoluteFill style={{ backgroundColor: '#020617', overflow: 'hidden' }}>
      <AbsoluteFill style={{ transform, transformOrigin }}>
        {shot.image ? (
          <Img
            src={staticFile(shot.image)}
            style={{ width: '100%', height: '100%', objectFit: 'cover' }}
          />
        ) : null}
      </AbsoluteFill>
      {shot.cursor ? (
        <Cursor x={shot.cursor.x * scaleX} y={shot.cursor.y * scaleY} click={shot.click === true} />
      ) : null}
      {shot.caption ? <Caption text={shot.caption} /> : null}
    </AbsoluteFill>
  );
};
```

- [ ] **Step 2: Type-check**

Run: `cd e2e/demo-video && npx tsc --noEmit`
Expected: no errors.

- [ ] **Step 3: Commit**

```bash
cd /home/chaizhenhua/Codes/awaken
git add e2e/demo-video/src/components/Shot.tsx
git commit -m "🎬 feat(demo-video): shot view with Ken Burns + overlays"
```

---

## Task 7: Demo series + composition registration

**Files:**
- Create: `e2e/demo-video/src/Demo.tsx`
- Create: `e2e/demo-video/src/Root.tsx`
- Create: `e2e/demo-video/src/index.ts`

- [ ] **Step 1: Create `Demo.tsx`**

Builds a `TransitionSeries`: a `Sequence` per shot, with a `Transition` inserted before every non-first, non-`cut` shot. The overlap math here matches `totalDurationInFrames` (Task 3).

```tsx
import React from 'react';
import { useVideoConfig } from 'remotion';
import { TransitionSeries, linearTiming } from '@remotion/transitions';
import { fade } from '@remotion/transitions/fade';
import { slide } from '@remotion/transitions/slide';
import type { Manifest, Shot } from './manifest-types';
import { holdFrames } from './timing';
import { ShotView } from './components/Shot';

export const TRANSITION_FRAMES = 12;

export const Demo: React.FC<{ manifest: Manifest; shots: Shot[] }> = ({ manifest, shots }) => {
  const { fps } = useVideoConfig();
  const { width: srcW, height: srcH } = manifest;
  const children: React.ReactNode[] = [];

  shots.forEach((s, i) => {
    if (i > 0 && (s.transition ?? 'fade') !== 'cut') {
      const presentation = s.transition === 'slide' ? slide() : fade();
      children.push(
        <TransitionSeries.Transition
          key={`t-${i}`}
          presentation={presentation}
          timing={linearTiming({ durationInFrames: TRANSITION_FRAMES })}
        />,
      );
    }
    children.push(
      <TransitionSeries.Sequence key={`s-${i}`} durationInFrames={holdFrames(s, fps)}>
        <ShotView shot={s} srcWidth={srcW} srcHeight={srcH} />
      </TransitionSeries.Sequence>,
    );
  });

  return <TransitionSeries>{children}</TransitionSeries>;
};
```

- [ ] **Step 2: Create `Root.tsx`**

Two compositions share the same component; `calculateMetadata` parses the manifest passed via `--props` and computes the exact duration. Empty/missing props (Studio default) fall back to a 1-frame stub instead of throwing.

```tsx
import React from 'react';
import { Composition } from 'remotion';
import { Demo, TRANSITION_FRAMES } from './Demo';
import { parseManifest, type Manifest, type Shot } from './manifest-types';
import { selectHighlight } from './highlight';
import { totalDurationInFrames } from './timing';

const FPS = 30;
const WIDTH = 1920;
const HEIGHT = 1200;

type Props = { manifest: Manifest; shots: Shot[] };

const stub: Props = {
  manifest: { locale: 'en', width: 3200, height: 2000, shots: [] },
  shots: [],
};

function metaFor(rawProps: unknown, pick: (m: Manifest) => Shot[]) {
  const raw = rawProps as any;
  if (!raw || !Array.isArray(raw.shots) || raw.shots.length === 0) {
    return { durationInFrames: 1, props: stub };
  }
  const manifest = parseManifest(raw);
  const shots = pick(manifest);
  return {
    durationInFrames: totalDurationInFrames(shots, FPS, TRANSITION_FRAMES),
    props: { manifest, shots } satisfies Props,
  };
}

export const RemotionRoot: React.FC = () => (
  <>
    <Composition
      id="DemoLong"
      component={Demo}
      fps={FPS}
      width={WIDTH}
      height={HEIGHT}
      durationInFrames={1}
      defaultProps={stub}
      calculateMetadata={({ props }) => metaFor(props, (m) => m.shots)}
    />
    <Composition
      id="DemoHighlight"
      component={Demo}
      fps={FPS}
      width={WIDTH}
      height={HEIGHT}
      durationInFrames={1}
      defaultProps={stub}
      calculateMetadata={({ props }) => metaFor(props, (m) => selectHighlight(m.shots))}
    />
  </>
);
```

- [ ] **Step 3: Create `index.ts`**

```ts
import { registerRoot } from 'remotion';
import { RemotionRoot } from './Root';

registerRoot(RemotionRoot);
```

- [ ] **Step 4: Type-check**

Run: `cd e2e/demo-video && npx tsc --noEmit`
Expected: no errors.

- [ ] **Step 5: Commit**

```bash
cd /home/chaizhenhua/Codes/awaken
git add e2e/demo-video/src/Demo.tsx e2e/demo-video/src/Root.tsx e2e/demo-video/src/index.ts
git commit -m "🎬 feat(demo-video): demo series + composition registration"
```

---

## Task 8: Render-all script

**Files:**
- Create: `e2e/demo-video/scripts/render-all.mjs`

- [ ] **Step 1: Create `render-all.mjs`**

Run with cwd = `e2e/demo-video`. Reads frames from `../target/demo-frames/<locale>` and writes to `out/`. Skips a locale whose manifest is missing.

```js
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
console.log(`Done: rendered ${rendered} locale(s) → ${OUT}`);
```

- [ ] **Step 2: Commit**

```bash
cd /home/chaizhenhua/Codes/awaken
git add e2e/demo-video/scripts/render-all.mjs
git commit -m "🎬 feat(demo-video): render-all script (MP4 + GIF, EN/ZH)"
```

---

## Task 9: Smoke-render with a synthetic manifest

Prove the compositor end-to-end before touching the capture stage, using a tiny hand-made manifest + a solid-color PNG. No backend or Gemini needed.

**Files:**
- Create (temporary, under gitignored `target/`): `e2e/target/demo-frames/en/manifest-en.json` and one PNG.

- [ ] **Step 1: Create a synthetic frame + manifest**

Run:
```bash
cd /home/chaizhenhua/Codes/awaken/e2e
mkdir -p target/demo-frames/en
# a 3200x2000 dark-blue PNG via ffmpeg
ffmpeg -y -f lavfi -i color=c=0x0b1220:s=3200x2000 -frames:v 1 target/demo-frames/en/02-providers-01.png
cat > target/demo-frames/en/manifest-en.json <<'JSON'
{
  "locale": "en",
  "width": 3200,
  "height": 2000,
  "shots": [
    { "scene": "01-intro", "hold": 2.5, "title": "Awaken", "subtitle": "Build, test & ship production AI agents", "transition": "fade" },
    { "scene": "02-providers", "hold": 2.5, "image": "02-providers-01.png", "caption": "Providers — wired to real Gemini", "cursor": { "x": 1600, "y": 1000 }, "click": true, "transition": "fade" }
  ]
}
JSON
```

- [ ] **Step 2: Render a still of each composition and eyeball it**

Run:
```bash
cd /home/chaizhenhua/Codes/awaken/e2e/demo-video
npx remotion still src/index.ts DemoLong out/still-title.png --frame=20 --public-dir="../target/demo-frames/en" --props="../target/demo-frames/en/manifest-en.json"
npx remotion still src/index.ts DemoLong out/still-shot.png --frame=70 --public-dir="../target/demo-frames/en" --props="../target/demo-frames/en/manifest-en.json"
```
Expected: two PNGs in `out/`. Use the Read tool on each: `still-title.png` shows the gradient "Awaken" title card; `still-shot.png` shows the dark frame with a crisp caption banner and the cyan cursor dot.

- [ ] **Step 3: Render the full MP4 to confirm the pipeline + duration**

Run:
```bash
cd /home/chaizhenhua/Codes/awaken/e2e/demo-video
npm run render:all
ffprobe -v error -select_streams v:0 -show_entries stream=width,height,r_frame_rate,duration -of default=noprint_wrappers=1 out/awaken-demo-en.mp4
```
Expected: `out/awaken-demo-en.mp4` exists; width=1920, height=1200, r_frame_rate=30/1, duration ≈ 4.9s (two 2.5s holds minus one 12-frame overlap = 138 frames ≈ 4.6s). The highlight render filters to scene `02-providers` only.

- [ ] **Step 4: Clean up the synthetic artifacts**

Run:
```bash
cd /home/chaizhenhua/Codes/awaken/e2e
rm -rf target/demo-frames target/demo-recordings/../ 2>/dev/null; rm -rf demo-video/out
```
(Only `target/` build output is removed; nothing is committed in this task.)

- [ ] **Step 5: No commit** (all artifacts are gitignored build output).

---

## Task 10: Capture config — frames, not video

**Files:**
- Modify: `e2e/playwright.demo.config.ts`

- [ ] **Step 1: Turn off video recording**

The frames are now the source. In `e2e/playwright.demo.config.ts`, replace:
```ts
    video: { mode: 'on', size: { width: 1600, height: 1000 } },
```
with:
```ts
    video: 'off',
```
Leave `viewport: { width: 1600, height: 1000 }` and `deviceScaleFactor: 2` unchanged (→ 3200×2000 screenshots). Leave the `launchOptions` args (they also keep headless screenshotting stable).

- [ ] **Step 2: Verify the config still loads**

Run: `cd e2e && npx playwright test -c playwright.demo.config.ts --list 2>&1 | head -5`
Expected: lists the `admin-demo.spec.ts` test without config errors (it will not run servers in `--list` mode).

- [ ] **Step 3: Commit**

```bash
cd /home/chaizhenhua/Codes/awaken
git add e2e/playwright.demo.config.ts
git commit -m "🎬 chore(demo): capture frames instead of continuous video"
```

---

## Task 11: Shot machinery in demo-helpers

Replace the on-page cursor/caption injection with a manifest+screenshot recorder. The `caption`/`scene`/`point` signatures stay identical so the spec body barely changes.

**Files:**
- Modify: `e2e/tests/demo-helpers.ts`

- [ ] **Step 1: Add Node imports + capture constants at the top of the file**

After the existing `import` lines (after line 3), add:
```ts
import fs from 'node:fs';
import path from 'node:path';

/** Source-frame geometry: viewport 1600×1000 at deviceScaleFactor 2. */
export const DSF = 2;
export const SRC_WIDTH = 1600 * DSF;
export const SRC_HEIGHT = 1000 * DSF;

const LOCALE_DIR = (process.env.DEMO_LOCALE === 'zh' || process.env.DEMO_LOCALE === 'zh-CN')
  ? 'zh'
  : 'en';

/** cwd is the e2e/ package when Playwright runs. */
export const FRAMES_DIR = path.resolve(process.cwd(), 'target/demo-frames', LOCALE_DIR);

type Transition = 'fade' | 'slide' | 'cut';
interface ManifestShot {
  scene: string;
  index: number;
  image?: string;
  caption?: string;
  hold: number;
  cursor?: { x: number; y: number };
  click?: boolean;
  focus?: { x: number; y: number; w: number; h: number };
  transition?: Transition;
  title?: string;
  subtitle?: string;
}

const recordedShots: ManifestShot[] = [];
const sceneCounters = new Map<string, number>();
let currentScene = 'init';
let pendingCaption: string | undefined;
```

- [ ] **Step 2: Add the capture lifecycle + `shot` API at the end of the file**

Append to `e2e/tests/demo-helpers.ts`:
```ts
/** Reset state and the frames dir at the start of a capture run. */
export function initCapture(): void {
  fs.rmSync(FRAMES_DIR, { recursive: true, force: true });
  fs.mkdirSync(FRAMES_DIR, { recursive: true });
  recordedShots.length = 0;
  sceneCounters.clear();
  currentScene = 'init';
  pendingCaption = undefined;
}

/** Tell the recorder which scene upcoming shots belong to. */
export function setCurrentScene(name: string): void {
  currentScene = name;
}

/** Centered 2×-pixel cursor target for a selector, or undefined if not found. */
export async function targetOf(
  page: Page,
  selector: string,
): Promise<{ x: number; y: number } | undefined> {
  const box = await page.locator(selector).first().boundingBox().catch(() => null);
  if (!box) return undefined;
  return { x: (box.x + box.width / 2) * DSF, y: (box.y + box.height / 2) * DSF };
}

interface ShotOpts {
  caption?: string;
  hold?: number;
  cursor?: { x: number; y: number };
  click?: boolean;
  focus?: { x: number; y: number; w: number; h: number };
  transition?: Transition;
  title?: string;
  subtitle?: string;
}

/**
 * Capture one beat: write a viewport PNG (unless it's a title card) and push a
 * manifest entry. `caption` persists across shots until changed; `cursor`/
 * `click`/`focus` apply only to this shot.
 */
export async function shot(page: Page, opts: ShotOpts = {}): Promise<void> {
  if (opts.caption !== undefined) pendingCaption = opts.caption;

  const n = (sceneCounters.get(currentScene) ?? 0) + 1;
  sceneCounters.set(currentScene, n);

  let image: string | undefined;
  if (!opts.title) {
    image = `${currentScene}-${String(n).padStart(2, '0')}.png`;
    const ok = await page
      .screenshot({ path: path.join(FRAMES_DIR, image) })
      .then(() => true)
      .catch(() => false);
    if (!ok) image = undefined;
  }

  recordedShots.push({
    scene: currentScene,
    index: recordedShots.length,
    image,
    caption: opts.title ? opts.caption : pendingCaption,
    hold: opts.hold ?? 2.2,
    cursor: opts.title ? undefined : opts.cursor,
    click: opts.click === true ? true : undefined,
    focus: opts.focus,
    transition: opts.transition ?? 'fade',
    title: opts.title,
    subtitle: opts.subtitle,
  });
}

/** Flush the manifest JSON next to the frames. */
export function writeManifest(): void {
  const manifest = {
    locale: DEMO_LOCALE,
    width: SRC_WIDTH,
    height: SRC_HEIGHT,
    shots: recordedShots,
  };
  fs.writeFileSync(
    path.join(FRAMES_DIR, `manifest-${LOCALE_DIR}.json`),
    JSON.stringify(manifest, null, 2),
  );
}
```

- [ ] **Step 3: Strip the on-page cursor/caption injection from `primeDemoPage`**

In `primeDemoPage`, keep the first `addInitScript` (localStorage token/locale/theme). Replace the **second** `addInitScript` (the one that paints dark + builds `#demo-cursor`/`#demo-caption`, currently lines ~102–194) with a minimal dark-paint-only version:
```ts
  // Paint the root dark immediately so the pre-React blank body never flashes
  // white between navigations. Cursor + captions are now drawn by Remotion, so
  // nothing is injected into the page (screenshots stay clean).
  await page.addInitScript(() => {
    try {
      const root = document.documentElement;
      root.style.background = '#020617';
      (root.style as any).colorScheme = 'dark';
      const darkStyle = document.createElement('style');
      darkStyle.textContent = 'html,body{background:#020617}';
      root.appendChild(darkStyle);
    } catch {
      /* document not ready — best effort */
    }
  });
```

- [ ] **Step 4: Re-point `caption`, `scene`, and `point` at the recorder**

Replace the existing `caption`, `scene`, and `point` functions (lines ~197–223) with:
```ts
/** Record a beat with a (locale-resolved) caption over the current screen. */
export async function caption(page: Page, text: string): Promise<void> {
  await shot(page, { caption: text });
}

/** A scene-title beat: longer hold, fade in. */
export async function scene(page: Page, pair: { en: string; zh: string }): Promise<void> {
  await shot(page, { caption: tr(pair), hold: 3.0, transition: 'fade' });
}

/** Record a beat with the cursor resting on the given element. */
export async function point(page: Page, selector: string): Promise<void> {
  const el = page.locator(selector).first();
  await el.scrollIntoViewIfNeeded().catch(() => {});
  const cursor = await targetOf(page, selector);
  await shot(page, { cursor });
}
```
`beat`, `typeSlow`, `smoothScroll`, `primeDemoPage` (now trimmed), and all i18n helpers stay as-is.

- [ ] **Step 5: Type-check the e2e package**

Run: `cd e2e && npx tsc --noEmit -p tsconfig.json 2>/dev/null || npx tsc --noEmit demo-video-noop 2>/dev/null; npx playwright test -c playwright.demo.config.ts --list 2>&1 | head -5`
Expected: the spec still lists without TypeScript errors from `demo-helpers.ts` (Playwright compiles specs on `--list`).

- [ ] **Step 6: Commit**

```bash
cd /home/chaizhenhua/Codes/awaken
git add e2e/tests/demo-helpers.ts
git commit -m "🎬 feat(demo): shot recorder replaces on-page overlay injection"
```

---

## Task 12: Wire the spec to emit shots

Make the spec's own helpers (`clickByName`, `goSidebar`, `clickTab`, `titleCard`) emit cursor/title shots, register the scene name for the recorder, initialize/flush capture, and add explicit captures after live-LLM replies (the "money shots").

**Files:**
- Modify: `e2e/tests/admin-demo.spec.ts`

- [ ] **Step 1: Extend the import from `./demo-helpers`**

Replace the existing import block (lines 2–15) with:
```ts
import {
  ADMIN_TOKEN,
  BACKEND_URL,
  DEMO_LOCALE,
  DSF,
  L,
  Lboth,
  beat,
  caption,
  initCapture,
  primeDemoPage,
  scene,
  setCurrentScene,
  shot,
  smoothScroll,
  tr,
  typeSlow,
  writeManifest,
} from './demo-helpers';
```

- [ ] **Step 2: Register the scene name inside `act`**

In `act`, after `currentScene = name;` (line ~83), add:
```ts
  setCurrentScene(name);
```

- [ ] **Step 3: Initialize capture at the start of the test**

In the test body, immediately after the `setTimeout(1_200_000)` call (line ~262), add `initCapture();` before `await primeDemoPage(page);`. The first three lines of the test become:
```ts
  initCapture();
  await primeDemoPage(page);
  watchForIssues(page);
```
(The `setTimeout(1_200_000)` line stays as the first statement, unchanged, directly above `initCapture()`.)

- [ ] **Step 4: Make `clickByName` emit a cursor+click shot before clicking**

Replace the body of `clickByName` (lines ~153–167) with:
```ts
async function clickByName(
  page: Page,
  role: Parameters<Page['getByRole']>[0],
  name: string | RegExp,
  opts: { timeout?: number } = {},
) {
  const resolved = typeof name === 'string' ? Lboth(name) : name;
  const loc = page.getByRole(role, { name: resolved }).first();
  await loc.scrollIntoViewIfNeeded().catch(() => {});
  const box = await loc.boundingBox().catch(() => null);
  if (box) {
    await shot(page, {
      cursor: { x: (box.x + box.width / 2) * DSF, y: (box.y + box.height / 2) * DSF },
      click: true,
    });
  }
  await loc.click({ timeout: opts.timeout ?? 15000 });
}
```

- [ ] **Step 5: Make `goSidebar` emit a cursor+click shot on the nav link**

In `goSidebar`, replace the `try` block that hovers/clicks the link (lines ~186–198) with:
```ts
  const link = page.locator('nav').getByRole('link', { name: Lboth(name, true) }).first();
  try {
    if (await link.count()) {
      const box = await link.boundingBox().catch(() => null);
      if (box) {
        await shot(page, {
          cursor: { x: (box.x + box.width / 2) * DSF, y: (box.y + box.height / 2) * DSF },
          click: true,
        });
      }
      await link.click();
      await page.waitForURL((u) => u.pathname === path, { timeout: 12000 });
    } else {
      await page.goto(path);
    }
  } catch {
    await page.goto(path);
  }
```

- [ ] **Step 6: Make `clickTab` emit a cursor+click shot on the tab**

Replace the body of `clickTab` (lines ~205–212) with:
```ts
async function clickTab(page: Page, label: string) {
  const idx = EDITOR_TABS.indexOf(label);
  const tab = page.getByRole('tab').nth(idx >= 0 ? idx : 0);
  const box = await tab.boundingBox().catch(() => null);
  if (box) {
    await shot(page, {
      cursor: { x: (box.x + box.width / 2) * DSF, y: (box.y + box.height / 2) * DSF },
      click: true,
    });
  }
  await tab.click().catch(() => {});
  await scrollTop(page);
}
```

- [ ] **Step 7: Convert `titleCard` to a title-card shot**

Replace the entire `titleCard` function (lines ~109–151) with:
```ts
/** A centered intro/outro title card (rendered by Remotion). */
async function titleCard(page: Page, title: string, subtitle: string, ms = 2600) {
  await shot(page, { title, subtitle, hold: ms / 1000, transition: 'fade' });
}
```

- [ ] **Step 8: Capture the live-LLM "money shots"**

Add explicit `shot()` calls right after each real Gemini result so the final answer / tool card is in a frame (the surrounding `caption()` fires before the reply arrives):

In **Scene 02** after `await beat(page, 1500);` (the post–"Test connection" wait, line ~312), add:
```ts
    await shot(page, { caption: tr({ en: 'Connection OK', zh: '连接正常' }) });
```

In **Scene 03** after the `model-test-response` wait + `await beat(page, 2500);` (line ~343), add:
```ts
    await shot(page, { caption: tr({ en: 'Real Gemini completion', zh: '真实 Gemini 回复' }) });
```

In **Scene 06** after the `dashboard_view` wait + `await beat(page, 2000);` (line ~437), add:
```ts
    await shot(page, { caption: tr({ en: 'Discovered: dashboard_view', zh: '已发现：dashboard_view' }) });
```

In **Scene 09**, after the first reply `await beat(page, 9000);` and `smoothScroll` (line ~566), add:
```ts
    await shot(page, { caption: tr({ en: 'Live agent reply', zh: '智能体实时回复' }) });
```
and after the second `await beat(page, 12000);` (line ~570), add:
```ts
    await shot(page, { caption: tr({ en: 'MCP tool → rendered UI card', zh: 'MCP 工具 → 渲染 UI 卡片' }) });
```

In **Scene 13** after `await beat(page, 9000);` (line ~719), add:
```ts
    await shot(page, { caption: tr({ en: 'New persona, instantly', zh: '新人设，立即生效' }) });
```

- [ ] **Step 9: Flush the manifest at the end of the test**

In the summary block, after the final `console.log(`TOTAL ISSUES: ...`)` (line ~804), add:
```ts
  writeManifest();
  // eslint-disable-next-line no-console
  console.log(`\n📝 manifest written: ${sceneResults.length} scenes captured`);
```

- [ ] **Step 10: Type-check / list**

Run: `cd e2e && npx playwright test -c playwright.demo.config.ts --list 2>&1 | head -8`
Expected: the test lists with no TypeScript errors. (`titleCard`/`scrollTop`/`Page` references all still resolve.)

- [ ] **Step 11: Commit**

```bash
cd /home/chaizhenhua/Codes/awaken
git add e2e/tests/admin-demo.spec.ts
git commit -m "🎬 feat(demo): emit manifest shots across all scenes"
```

---

## Task 13: Add the `capture:demo` script

**Files:**
- Modify: `e2e/package.json`

- [ ] **Step 1: Add the script**

In `e2e/package.json` `scripts`, add a `capture:demo` entry alongside `record:demo`:
```json
    "record:demo": "env -u NO_COLOR -u FORCE_COLOR playwright test -c playwright.demo.config.ts",
    "capture:demo": "env -u NO_COLOR -u FORCE_COLOR playwright test -c playwright.demo.config.ts"
```
(Same command; `capture:demo` is the name the new pipeline documents. `record:demo` stays for backward compatibility.)

- [ ] **Step 2: Commit**

```bash
cd /home/chaizhenhua/Codes/awaken
git add e2e/package.json
git commit -m "🎬 chore(demo): add capture:demo script"
```

---

## Task 14: Capture run against real Gemini (EN, then ZH)

This is the end-to-end functional verification. Requires a fresh Vertex token and `gcloud` access (see memory `project_demo_gemini_setup`).

**Files:** none (produces gitignored `e2e/target/demo-frames/<locale>/`).

- [ ] **Step 1: Mint a Vertex token + export env**

Run:
```bash
export VERTEX_API_KEY=$(gcloud auth print-access-token)
export VERTEX_PROJECT_ID=uncarve-ai VERTEX_LOCATION=us-central1
export AWAKEN_STORAGE_DIR="$(mktemp -d)/awaken-demo-rec"
```
Expected: `VERTEX_API_KEY` is a long token string; fresh storage dir avoids stale-key panics.

- [ ] **Step 2: Capture EN**

Run:
```bash
cd /home/chaizhenhua/Codes/awaken/e2e
DEMO_LOCALE=en npm run capture:demo
```
Expected: the run reaches the summary with `✔ no failed network responses`, `✔ no console errors`, `TOTAL ISSUES: 0`, and `📝 manifest written`. If issues appear, treat them as real bugs (the recording IS the E2E test) and fix before continuing.

- [ ] **Step 3: Validate EN frames + manifest**

Run:
```bash
cd /home/chaizhenhua/Codes/awaken/e2e
node -e "const m=require('./target/demo-frames/en/manifest-en.json');console.log('shots',m.shots.length,m.width+'x'+m.height);const fs=require('fs');for(const s of m.shots){if(s.image&&!fs.existsSync('target/demo-frames/en/'+s.image))throw new Error('missing '+s.image)}console.log('all images present')"
ffprobe -v error -select_streams v:0 -show_entries stream=width,height -of csv=p=0 target/demo-frames/en/02-providers-01.png
```
Expected: prints `shots <N> 3200x2000`, `all images present`, and `3200,2000` for a sampled PNG.

- [ ] **Step 4: Capture ZH (re-mint token if >45 min elapsed)**

Run:
```bash
export VERTEX_API_KEY=$(gcloud auth print-access-token)
cd /home/chaizhenhua/Codes/awaken/e2e
DEMO_LOCALE=zh npm run capture:demo
node -e "const m=require('./target/demo-frames/zh/manifest-zh.json');console.log('zh shots',m.shots.length)"
```
Expected: ZH run also reaches `TOTAL ISSUES: 0` and writes `manifest-zh.json`.

- [ ] **Step 5: No commit** (frames are gitignored build output).

---

## Task 15: Render, verify sharpness, and finalize

**Files:** none (produces gitignored `e2e/demo-video/out/`).

- [ ] **Step 1: Render all four videos + GIFs**

Run:
```bash
cd /home/chaizhenhua/Codes/awaken/e2e/demo-video
npm run render:all
ls -la out/
```
Expected: `awaken-demo-en.mp4`, `awaken-demo-en-highlight.mp4`, `awaken-demo-en.gif`, and the three `zh` equivalents.

- [ ] **Step 2: Probe duration/resolution/fps**

Run:
```bash
cd /home/chaizhenhua/Codes/awaken/e2e/demo-video
for f in out/awaken-demo-en.mp4 out/awaken-demo-en-highlight.mp4 out/awaken-demo-zh.mp4; do
  echo "== $f =="
  ffprobe -v error -select_streams v:0 -show_entries stream=width,height,r_frame_rate,duration -of default=noprint_wrappers=1 "$f"
done
```
Expected: each is `1920×1200`, `30/1` fps; long versions are minutes long, highlights ~60–90s.

- [ ] **Step 3: Eyeball sharpness vs the old output**

Run:
```bash
cd /home/chaizhenhua/Codes/awaken/e2e/demo-video
npx remotion still src/index.ts DemoLong out/check-en.png --frame=120 --public-dir="../target/demo-frames/en" --props="../target/demo-frames/en/manifest-en.json"
```
Read `out/check-en.png` with the Read tool and confirm UI text/lines are crisp and the caption/cursor render cleanly. Compare against `e2e/target/demo-recordings/out/awaken-demo-en.mp4` (old, blurry) on a matching scene.

- [ ] **Step 4: Confirm the real-Gemini money shots are present**

Read the captured PNGs for the live scenes and confirm real content (non-error):
- `e2e/target/demo-frames/en/03-models-*.png` — a real Gemini completion is visible.
- `e2e/target/demo-frames/en/09-sandbox-*.png` — a live agent reply and the MCP-rendered UI card are visible.

- [ ] **Step 5: Rotate/remove the Vertex credential (memory `project_demo_gemini_setup`)**

Run:
```bash
unset VERTEX_API_KEY
```
And remove any minted service-account key files created for this session so tokens don't leak into transcripts.

- [ ] **Step 6: Final commit (docs/scripts only — no media)**

All media stays under gitignored `target/` and `demo-video/out/`. Confirm nothing binary is staged:
```bash
cd /home/chaizhenhua/Codes/awaken
git status --short
```
Expected: clean (only the already-committed source/plan changes). If a README update is wanted to point at the new render path, do it in a separate, explicit task.

---

## Self-Review

**Spec coverage:**
- Capture → 2× PNG + manifest: Tasks 11–12, run in 14. ✓
- Manifest contract: Task 2 (types/validator), produced in Task 11 (`writeManifest`). ✓
- Clean screenshots (no injected cursor/caption): Task 11 Step 3. ✓
- Remotion project under `e2e/demo-video/` with isolated deps: Task 1. ✓
- Cursor / caption / focus-zoom / transitions / title cards: Tasks 5–7. ✓
- Long + Highlight compositions, both locales: Tasks 4, 7, 8. ✓
- Frames via `--public-dir`, manifest via `--props`, nothing committed: Tasks 8–9, gitignore in Task 1. ✓
- 1920×1200 / 30fps, MP4 + GIF: Tasks 7–8, verified Task 15. ✓
- Verification = green capture + manifest/frame checks + still inspection + ffprobe + real-Gemini money shots: Tasks 9, 14, 15. ✓
- Vertex token window + cleanup: Task 14 Step 1/4, Task 15 Step 5. ✓
- A2A config-only (SSRF guard): unchanged spec behavior, preserved in Task 12 (Scene 07 untouched). ✓

**Placeholder scan:** No TBD/TODO; every code step has complete code. ✓

**Type consistency:** `Shot`/`Manifest` shape is identical across `manifest-types.ts`, `timing.ts`, `highlight.ts`, `Demo.tsx`, `Root.tsx`, and the recorder's `ManifestShot` in `demo-helpers.ts`. `TRANSITION_FRAMES = 12` is shared by `Demo.tsx` and `Root.tsx`; `holdFrames`/`totalDurationInFrames` use the same overlap rule the `TransitionSeries` produces. `SRC_WIDTH/HEIGHT` (3200×2000) match the manifest `width/height` consumed by `Shot.tsx` scaling. ✓
