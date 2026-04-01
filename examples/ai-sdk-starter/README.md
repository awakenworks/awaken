# AI SDK Starter

Full-stack example: React + Vite frontend with [AI SDK](https://sdk.vercel.ai/) v6, Rust agent backend.

## Setup

```bash
cp .env.example .env          # fill in your LLM provider key
npm install
npm run dev                    # starts both frontend and backend
```

Open http://localhost:5173 in your browser.

## Structure

- `agent/` — Rust backend (awaken runtime + awaken-server)
- `src/` — React frontend with AI SDK `useChat` hook
- `.env.example` — required environment variables
