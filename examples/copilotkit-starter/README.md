# CopilotKit Starter

Full-stack example: Next.js frontend with [CopilotKit](https://copilotkit.ai/) (AG-UI protocol), Rust agent backend.

## Setup

```bash
cp .env.example .env          # fill in your LLM provider key
npm install
npm run dev                    # starts Next.js dev server + Rust backend
```

Open http://localhost:3000 in your browser.

## Structure

- `agent/` — Rust backend (awaken runtime + awaken-server, AG-UI protocol)
- `app/` — Next.js app with CopilotKit components
- `.env.example` — required environment variables
