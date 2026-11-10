import { useEffect, useState } from 'react';
import { useChat } from '@ai-sdk/react';
import { DefaultChatTransport, type UIMessage } from 'ai';

type Connection = { access_token: string; expires_at: string; thread_id: string };

function Conversation({ connection, messages }: { connection: Connection; messages: UIMessage[] }) {
  const { access_token, thread_id: threadId } = connection;
  const [input, setInput] = useState('');
  const [stopError, setStopError] = useState('');
  const chat = useChat({
    id: threadId,
    messages,
    transport: new DefaultChatTransport({
      api: `/awaken/v1/ai-sdk/threads/${threadId}/runs`,
      headers: { Authorization: `Bearer ${access_token}` },
    }),
  });
  async function stop() {
    const result = await fetch(`/awaken/v1/ai-sdk/threads/${threadId}/runs`, {
      method: 'DELETE', headers: { Authorization: `Bearer ${access_token}` },
    });
    if (result.status !== 204) { setStopError(`Stop refused: HTTP ${result.status}`); return; }
    setStopError('');
    await chat.stop();
  }
  return <>
    <p>Conversation: {threadId} · Status: <span data-testid="status">{chat.status}</span></p>
    <ol aria-label="Conversation">
      {chat.messages.map(message => <li key={message.id}>
        <strong>{message.role}</strong>: {message.parts.map((part, index) => part.type === 'text'
          ? <span key={index}>{part.text}</span>
          : <span key={index}> [{part.type}: inspect tools and approvals in Console] </span>)}
      </li>)}
    </ol>
    <form onSubmit={event => {
      event.preventDefault();
      if (!input.trim()) return;
      void chat.sendMessage({ text: input }); setInput('');
    }}>
      <label>Message <input value={input} onChange={event => setInput(event.target.value)} /></label>
      <button disabled={chat.status === 'streaming' || chat.status === 'submitted'}>Send</button>
    </form>
    <button type="button" onClick={() => void stop().catch(() => setStopError('Stop failed; keep observing the same Session'))}>Stop</button>
    {(chat.error || stopError) && <p role="alert">{stopError || chat.error?.message}. Check Console; reconnect to read history before sending again.</p>}
  </>;
}

export default function App() {
  const [loaded, setLoaded] = useState<{ connection: Connection; messages: UIMessage[] }>();
  const [error, setError] = useState('');
  useEffect(() => {
    const controller = new AbortController();
    void (async () => {
      const result = await fetch('/api/connection', { method: 'POST', signal: controller.signal });
      if (!result.ok) throw new Error(`Connection failed: HTTP ${result.status}`);
      const connection: Connection = await result.json();
      const history = await fetch(`/awaken/v1/ai-sdk/threads/${connection.thread_id}/messages`, {
        headers: { Authorization: `Bearer ${connection.access_token}` }, signal: controller.signal,
      });
      if (!history.ok) throw new Error(`History failed: HTTP ${history.status}`);
      const body = await history.json();
      if (!Array.isArray(body.items)) throw new Error('History did not contain messages');
      setLoaded({ connection, messages: body.items });
    })().catch(reason => { if (!controller.signal.aborted) setError(reason.message); });
    return () => controller.abort();
  }, []);
  return <main>
    <h1>Your first Awaken application</h1>
    <p>This local example reuses one published Session. Messages and results remain in Awaken.</p>
    {!loaded && !error && <p>Connecting and reading committed history…</p>}
    {error && <p role="alert">{error}</p>}
    {loaded && <Conversation connection={loaded.connection} messages={loaded.messages} />}
    <button onClick={() => location.reload()}>Reconnect and read history</button>
  </main>;
}
