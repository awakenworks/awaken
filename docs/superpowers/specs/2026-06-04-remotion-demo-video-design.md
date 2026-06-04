# Remotion demo video — design

Re-produce the awaken-admin-console promo video (MP4 + GIF, EN/ZH) with sharp
text and lines, by replacing the lossy continuous screen recording with a
two-stage **capture → compose** pipeline. The recording run remains a real
end-to-end functional verification against real Gemini (Vertex, gemini-2.5-pro).

## Problem

The current `e2e/tests/admin-demo.spec.ts` drives the live console and captures a
continuous video via Playwright `recordVideo`. The result is blurry because:

- Capture is at **1× device scale** (1440×900), so text never has extra pixel
  detail to begin with.
- The intermediate is **lossy VP8 webm at 25fps**, which softens edges further.

Remotion is the requested tool, but Remotion is **not** a screen recorder — it is
a programmatic React video renderer. It cannot drive the live app and record
interactions. So "re-record with Remotion" is realized as: capture crisp source
material from the real app, then have Remotion compose the finished video.

## Approach (chosen)

Crisp screenshots + Remotion compositor (A1 — key-state screenshots):

```
① Capture   Playwright, real Gemini, all 15 scenes (still an E2E run)
            └─ at each beat: 2× lossless PNG (viewport) + a manifest entry
② Compose   Remotion project under e2e/demo-video/
            └─ read manifest + PNGs → vector overlays (cursor, caption, focus
               zoom, transitions, title cards) → lossless render
③ Export    Long + Highlight compositions, each EN/ZH
            └─ 4× MP4 (1920×1200, 30fps, H.264) + GIFs
```

Sharpness comes entirely from the lossless 2× PNG source plus Remotion's vector
rendering; there is no lossy continuous encode in the middle, and captions /
cursor are drawn by Remotion as vectors (crisp at any scale).

The capture run keeps the existing QA instrumentation (failed-response + console
watchers, per-scene `act()` wrapper). A green capture run = end-to-end
verification across providers, models, MCP, A2A, the AI assistant, the agent
editor, sandbox, tracing, datasets, eval, and audit — exactly as today.

### Why not the alternatives

- **Fix Playwright capture quality only** (deviceScaleFactor 2 + larger
  recordVideo size): least work and sharper, but not Remotion and still a lossy
  webm. Rejected because the user asked for Remotion.
- **Full Remotion re-creation (mockup)**: perfectly crisp but a mockup — loses
  the "every scene actually works on camera" verification value, most work.
  Rejected.

## Stage ① — Capture (reuse existing e2e)

Modify the existing files; reuse the spec body and selectors.

- **`e2e/playwright.demo.config.ts`** — repurpose to capture mode:
  `use.deviceScaleFactor: 2`, viewport `1440×900` (→ screenshots **2880×1800**,
  16:10), `video: 'off'`, `outputDir`/screenshot root under
  `target/demo-frames/<locale>/`. Backend webServer env unchanged from the
  current demo config (Vertex creds forwarded, trace routes exposed, demo seed,
  fresh storage dir). Single chromium project, generous timeout, retries 0.
- **`e2e/tests/demo-helpers.ts`** — capture-mode rework:
  - Remove the injected on-page cursor and caption banner (Remotion draws these
    as vectors). Keep the localStorage prime (admin token, locale, dark theme)
    and the dark-background paint that prevents white flashes.
  - Keep the i18n helpers (`L`, `Lboth`, `tr`, locale handling).
  - Add `shot(page, opts)`: screenshot the **viewport** to
    `target/demo-frames/<locale>/<scene>-<n>.png` and push a manifest entry with
    the pending caption, optional cursor target, click flag, focus rect, hold
    duration, and transition. Cursor / focus coordinates come from
    `locator.boundingBox()` (CSS px) multiplied by `deviceScaleFactor` to land in
    2× pixel space. Viewport (not full-page) screenshots so cursor coords map
    directly from clientX/Y × DSF without scroll offset.
  - Add a manifest writer that flushes `manifest-<locale>.json` at end of run.
  - Pacing (`beat`) is mostly removed from capture — pacing now lives in
    Remotion `hold` durations, which makes the capture run much faster (only real
    LLM waits remain), easing the Vertex token window.
- **`e2e/tests/admin-demo.spec.ts`** — keep the 15-scene structure, selectors,
  real-Gemini waits, fallbacks, and QA summary. Replace the promo-polish calls
  (`caption` / `scene` / `point` / click helpers) with shot-emitting
  equivalents. Dynamic content (LLM streaming, tool-call cards) is captured as
  2–3 key stills per the A1 model.

### Manifest contract (① ↔ ②)

One entry per shot, in play order. Coordinates are in 2× pixel space; captions
are finalized to the active locale at capture time.

```jsonc
{
  "scene": "02-providers", "index": 12,
  "image": "02-providers-03.png",
  "caption": "Adapter: vertex · live credentials",
  "hold": 2.0,
  "cursor": { "x": 1840, "y": 560 },
  "click": true,
  "focus": { "x": 1200, "y": 300, "w": 1400, "h": 900 },
  "transition": "fade"
}
```

`cursor`, `click`, `focus`, and `transition` are optional. `hold` is seconds.

## Stage ② — Compose (`e2e/demo-video/`, isolated package)

```
e2e/demo-video/
  package.json            # remotion, react, react-dom (dependency isolation)
  remotion.config.ts
  src/Root.tsx            # registers DemoLong / DemoHighlight; locale via props
  src/Demo.tsx            # builds <Series> from the manifest
  src/components/Shot.tsx     # <Img> + Ken Burns zoom (interpolate over focus)
  src/components/Cursor.tsx   # spring move prev→current target + click ripple
  src/components/Caption.tsx  # vector caption, spring in/out (sharp)
  src/components/TitleCard.tsx# intro/outro "Awaken" gradient title
  src/manifest-types.ts   # manifest TypeScript types
```

- Frames are not committed: they live in `target/demo-frames/`. Remotion reads
  them via `--public-dir` pointed at that directory; the manifest is passed via
  `--props`. This avoids the repo's test-data/temp git hooks.
- **Compositions**: `DemoLong` (all scenes) and `DemoHighlight` (a 60–90s subset:
  `02 Providers → 04 AI builds agent → 06 MCP → 09 Sandbox tool card →
  12 Eval`). Both parameterized by locale → 4 renders.
- No audio (matches current output; BGM can be added later).
- Render target: 1920×1200 (16:10), 30fps, H.264 MP4. (Composition can be bumped
  to 2560×1600 later if more sharpness is wanted; 2880-wide source supersamples
  cleanly into either.)

## Stage ③ — Run & output

```bash
export VERTEX_API_KEY=$(gcloud auth print-access-token)
export VERTEX_PROJECT_ID=uncarve-ai VERTEX_LOCATION=us-central1
cd e2e
DEMO_LOCALE=en npm run capture:demo   # → target/demo-frames/en + manifest-en.json
DEMO_LOCALE=zh npm run capture:demo
cd demo-video
npm run render:all                    # → target/demo-recordings/out/*.mp4 + *.gif
```

- `e2e/package.json` gains `capture:demo`. `e2e/demo-video/package.json` provides
  `render:long`, `render:highlight`, `render:all`.
- GIF: Remotion `--codec gif` on the highlight composition (or ffmpeg
  palettegen/paletteuse). Artifacts land in `target/demo-recordings/out/` and are
  not committed (build output; repo hooks forbid test-data/temp files).

## Verification

- The capture run must be green with the existing QA instrumentation (0
  network/console issues) → end-to-end verification preserved.
- Validate the manifest entry count and confirm each referenced PNG is exactly
  2880×1800.
- After render: export a still per composition (`remotion still`), inspect it
  with the Read tool for crispness, and `ffprobe` each MP4 for duration /
  resolution / fps. Compare text sharpness against the old output on a matching
  frame.
- Confirm real Gemini actually answered (Scene 3 model test, Scene 9 sandbox
  tool call are real, non-error).
- A2A scene shows configuration only (the backend SSRF guard blocks loopback
  discovery on camera).

## Risks / notes

- **Coordinate alignment**: viewport (not full-page) screenshots so cursor/focus
  coords (clientX/Y × DSF) map directly; capture coords immediately after
  `scrollIntoViewIfNeeded`.
- **Remotion deps are heavy** (hundreds of MB); isolated via the separate
  `e2e/demo-video/package.json`.
- **Frame disk**: ~(60–80) × 2 locales × 2× PNG ≈ 300–600 MB under `target/`
  (gitignored).
- **Vertex token (~1h)**: capture is faster than recording (most `beat()` waits
  removed), so both locales finish comfortably in one window; rotate/remove the
  Vertex SA key and minted tokens after recording.
- **Live-LLM nondeterminism**: dynamic shots capture whatever is on screen with
  tolerant assertions, so wording never fails the run while the path is still
  proven.
