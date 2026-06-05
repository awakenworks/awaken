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
  /** Repo / project URL shown under a title card (intro/outro). */
  link?: string;
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
      link: s.link ?? undefined,
    };
  });
  return { locale: m.locale, width: m.width, height: m.height, shots };
}
