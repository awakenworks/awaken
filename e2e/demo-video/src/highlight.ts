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
 * Numbered "build -> test -> ship" narration for the GIF, so the highlight
 * beats read as one continuous flow rather than five disconnected snapshots.
 */
export const GIF_NARRATION: Record<string, { en: string; zh: string }> = {
  '02-providers': { en: '1 · Connect a real model', zh: '① 接入真实模型' },
  '04-assistant-create-agent': { en: '2 · Describe it — AI builds the agent', zh: '② 描述需求，AI 自动构建' },
  '06-mcp': { en: '3 · Plug in tools over MCP', zh: '③ 通过 MCP 接入工具' },
  '09-sandbox': { en: '4 · Test it live in the sandbox', zh: '④ 沙盒里实时测试' },
  '12-eval': { en: '5 · Evaluate & ship', zh: '⑤ 评测并上线' },
};

/** Localized narration map (scene -> caption) for a given locale. */
export function gifNarration(locale: 'en' | 'zh-CN'): Record<string, string> {
  const out: Record<string, string> = {};
  for (const [scene, pair] of Object.entries(GIF_NARRATION)) {
    out[scene] = locale === 'zh-CN' ? pair.zh : pair.en;
  }
  return out;
}

/**
 * One enlarged, readable "payoff" beat per highlight scene for the GIF: the
 * last caption-bearing image shot of each scene, held long enough to read,
 * captioned with the continuous numbered narration, and zoomed in a touch so
 * the UI is legible at small GIF sizes. Far fewer, slower beats than the full
 * highlight — a looping GIF that tells the build->test->ship story.
 */
export function selectGifShots(
  shots: Shot[],
  holdSeconds = 2.4,
  captions?: Record<string, string>,
  zoom?: number,
  scenes: string[] = HIGHLIGHT_SCENES,
): Shot[] {
  const out: Shot[] = [];
  for (const scene of scenes) {
    const inScene = shots.filter((s) => s.scene === scene && s.image);
    if (inScene.length === 0) continue;
    const captioned = inScene.filter((s) => s.caption);
    const pool = captioned.length > 0 ? captioned : inScene;
    const pick = pool[pool.length - 1];
    out.push({
      ...pick,
      hold: holdSeconds,
      transition: 'fade',
      cursor: undefined,
      click: false,
      caption: captions?.[scene] ?? pick.caption,
      zoom: zoom ?? pick.zoom,
    });
  }
  return out;
}
