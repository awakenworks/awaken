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
  const click = (scene: string, image: string, caption?: string): Shot => ({
    scene,
    index: 0,
    hold: 2,
    image,
    caption,
    cursor: { x: 10, y: 20 },
    click: true,
    focus: { x: 0, y: 0, w: 5, h: 5 },
  });

  it('keeps the complete ordered story of each scene incl. nav/click/cursor beats', () => {
    const shots = [
      sc('01-intro', 'x.png', 'skip me'),
      click('09-sandbox', 'nav.png', 'Sandbox'), // sidebar nav INTO the scene
      sc('09-sandbox', 'open.png', 'Sandbox'), // landed (static, dup caption of nav)
      click('09-sandbox', 'send.png', 'Sandbox'), // press Send
      sc('09-sandbox', 'reply.png', 'Live agent reply'), // payoff
    ];
    const out = selectGifShots(shots);
    expect(out.map((s) => s.image)).toEqual(['nav.png', 'open.png', 'send.png', 'reply.png']);
    // interaction beats keep cursor/click/focus so the camera can move + ripple + highlight
    const nav = out[0];
    expect(nav.click).toBe(true);
    expect(nav.cursor).toEqual({ x: 10, y: 20 });
    expect(nav.focus).toEqual({ x: 0, y: 0, w: 5, h: 5 });
    // action beats are held briefly; reading beats held longer
    expect(nav.hold).toBe(0.9);
    expect(out[3].hold).toBe(1.6); // reply payoff
  });

  it('collapses only adjacent dead-duplicate static beats', () => {
    const shots = [
      sc('02-providers', 'a.png', 'reply'),
      sc('02-providers', 'b.png', 'reply'), // dead dup -> dropped
      sc('02-providers', 'c.png', 'done'),
    ];
    const out = selectGifShots(shots);
    expect(out.map((s) => s.image)).toEqual(['a.png', 'c.png']);
  });

  it('never collapses an interaction beat even with a duplicate caption', () => {
    const shots = [
      sc('06-mcp', 'a.png', 'Verify tools'),
      click('06-mcp', 'b.png', 'Verify tools'), // same caption but it is a click -> kept
      sc('06-mcp', 'c.png', 'Discovered'),
    ];
    const out = selectGifShots(shots);
    expect(out.map((s) => s.image)).toEqual(['a.png', 'b.png', 'c.png']);
  });

  it('honours custom holds and scene set', () => {
    const shots = [sc('02-providers', 'a.png', 'x'), sc('zzz', 'b.png', 'y')];
    const out = selectGifShots(shots, { holdSeconds: 2.0, scenes: ['02-providers'] });
    expect(out.map((s) => s.scene)).toEqual(['02-providers']);
    expect(out[0].hold).toBe(2.0);
  });
});
