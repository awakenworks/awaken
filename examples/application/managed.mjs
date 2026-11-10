// npm install @anthropic-ai/sdk
// Trusted backend example. Keep the service API key out of browser code.
import Anthropic from '@anthropic-ai/sdk';
// Run inside examples/application; support.mjs handles CLI options and observation.
import { readCommand, observe } from './support.mjs';

try {
  const { values, positionals, timeoutMs } = readCommand();
  const client = new Anthropic({
    apiKey: process.env.AWAKEN_API_KEY ?? 'local',
    baseURL: process.env.AWAKEN_BASE_URL ?? 'http://127.0.0.1:8080',
    maxRetries: 0, // A lost response does not make a mutation safe to repeat.
    timeout: timeoutMs,
  });
  const scope = {
    betas: ['managed-agents-2026-04-01'],
    ...(process.env.AWAKEN_WORKSPACE_ID ? { workspace_id: process.env.AWAKEN_WORKSPACE_ID } : {}),
  };
  if (!values.session && (!process.env.AWAKEN_AGENT_ID || !process.env.AWAKEN_ENVIRONMENT_ID)) {
    throw new Error('Set AWAKEN_AGENT_ID and AWAKEN_ENVIRONMENT_ID from the published Agent and Environment');
  }
  const session = values.session
    ? await client.beta.sessions.retrieve(values.session, scope)
    : await client.beta.sessions.create({
      agent: process.env.AWAKEN_AGENT_ID,
      environment_id: process.env.AWAKEN_ENVIRONMENT_ID,
      ...scope,
    });
  console.log(`session=${session.id}`);
  console.log(`Open in Console: /w/${encodeURIComponent(process.env.AWAKEN_WORKSPACE_ID ?? 'default')}/sessions/${session.id}`);

  if (values.history) {
    for await (const event of client.beta.sessions.events.list(session.id, scope)) {
      if (event.processed_at && event.type === 'agent.message') {
        for (const block of event.content ?? []) if (block.type === 'text') console.log(block.text);
      }
    }
  } else {
    const event = values.approve || values.deny
      ? { type: 'user.tool_confirmation', tool_use_id: values.approve ?? values.deny, result: values.approve ? 'allow' : 'deny' }
      : { type: 'user.message', content: [{ type: 'text', text: positionals.join(' ') || 'Reply with a two-item API review checklist.' }] };
    const receiptId = values.receipt ?? (await client.beta.sessions.events.send(session.id, {
      events: [event], ...scope,
    })).data[0]?.id;
    if (!receiptId) throw new Error('The server returned no Event receipt; inspect the Session before submitting again');
    console.log(`receipt=${receiptId}`);

    await observe(client, session.id, receiptId, scope, timeoutMs);
  }
} catch (error) {
  console.error(`Example failed: ${error.status ? `HTTP ${error.status}; ` : ''}${error.message}`);
  process.exitCode = 1;
}
