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
