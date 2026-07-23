import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';

function assignment(text, name) {
  const escaped = name.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
  return text.match(
    new RegExp(`(?:^|\\n)\\s*(?:export\\s+)?${escaped}\\s*=\\s*["']?([^\\s"'#;]+)`),
  )?.[1];
}

function codingOpenAIBase(anthropicBase) {
  const trimmed = anthropicBase.replace(/\/+$/, '');
  return trimmed.endsWith('/v1') ? trimmed : `${trimmed}/v1`;
}

/**
 * Resolve one Kimi credential into its two real protocol projections.
 *
 * Kimi Code membership keys authenticate at api.kimi.com/coding and can use
 * both its Anthropic root and OpenAI-compatible `/v1` endpoint. Moonshot
 * Platform keys are a different credential family and use api.moonshot.cn/v1.
 * Never infer that one key is accepted by the other service.
 */
export function loadKimiConfig() {
  let bashrc = '';
  try {
    bashrc = fs.readFileSync(path.join(os.homedir(), '.bashrc'), 'utf8');
  } catch {
    // Environment-only configuration remains valid.
  }

  const moonshotKey = process.env.MOONSHOT_API_KEY ?? assignment(bashrc, 'MOONSHOT_API_KEY');
  const codingKey = process.env.ANTHROPIC_API_KEY ?? assignment(bashrc, 'ANTHROPIC_API_KEY');
  const anthropicBase = process.env.ANTHROPIC_BASE_URL
    ?? assignment(bashrc, 'ANTHROPIC_BASE_URL')
    ?? 'https://api.kimi.com/coding/';

  if (moonshotKey) {
    return {
      key: moonshotKey,
      anthropicKey: codingKey,
      anthropicBase,
      anthropicModel: process.env.ANTHROPIC_MODEL ?? 'kimi-for-coding',
      openaiBase: process.env.MOONSHOT_BASE_URL ?? 'https://api.moonshot.cn/v1',
      openaiModel: process.env.MOONSHOT_MODEL ?? process.env.OPENAI_MODEL ?? 'kimi-k2.5',
      source: 'moonshot-platform',
    };
  }
  if (!codingKey) return null;
  return {
    key: codingKey,
    anthropicKey: codingKey,
    anthropicBase,
    anthropicModel: process.env.ANTHROPIC_MODEL ?? 'kimi-for-coding',
    openaiBase: codingOpenAIBase(anthropicBase),
    openaiModel: process.env.OPENAI_MODEL ?? 'kimi-for-coding',
    source: 'kimi-code',
  };
}
