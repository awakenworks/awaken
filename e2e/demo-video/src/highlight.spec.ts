import { describe, it, expect } from 'vitest';
import { selectHighlight, selectGifShots, HIGHLIGHT_SCENES } from './highlight';
import type { Shot } from './manifest-types';

const s = (scene: string): Shot => ({ scene, index: 0, hold: 2, image: 'a.png' });
const sc = (scene: string, image: string, caption?: string): Shot =>
  ({ scene, index: 0, hold: 2, image, caption });

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

describe('selectGifShots', () => {
  it('keeps real captions, drops cursor-only frames, and shows intro + payoff per scene', () => {
    const shots = [
      sc('01-intro', 'x.png', 'skip me'),
      sc('02-providers', 'a.png', 'Providers'),
      sc('02-providers', 'b.png', 'Adapter: vertex'),
      sc('02-providers', 'c.png', 'Test connection'),
      sc('02-providers', 'd.png', 'Connection OK'),
      { scene: '09-sandbox', index: 0, hold: 2, image: 'cur.png' }, // cursor-only, no caption
      // nav/click frame carrying a bled caption from the previous scene:
      { scene: '09-sandbox', index: 0, hold: 2, image: 'nav.png', caption: 'Connection OK', click: true },
      sc('09-sandbox', 'e.png', 'Sandbox'),
      sc('09-sandbox', 'f.png', 'tool card'),
    ];
    const out = selectGifShots(shots, 1.5, 1.18, 3);
    // 02 has 4 captioned beats -> first + final two; real captions preserved
    expect(out.filter((s) => s.scene === '02-providers').map((s) => s.caption)).toEqual([
      'Providers',
      'Test connection',
      'Connection OK',
    ]);
    // 09 has the cursor-only frame dropped -> both captioned beats kept
    expect(out.filter((s) => s.scene === '09-sandbox').map((s) => s.caption)).toEqual([
      'Sandbox',
      'tool card',
    ]);
    expect(out[0].zoom).toBe(1.18);
    expect(out[0].hold).toBe(1.5);
    expect(out[0].cursor).toBeUndefined();
  });

  it('dedupes consecutive duplicate captions', () => {
    const shots = [
      sc('02-providers', 'a.png', 'reply'),
      sc('02-providers', 'b.png', 'reply'),
      sc('02-providers', 'c.png', 'done'),
    ];
    const out = selectGifShots(shots, 1.5, 1.18, 5);
    expect(out.map((s) => s.caption)).toEqual(['reply', 'done']);
  });
});
