# Acme Reports — proof-of-work app

A deliberately tiny Next.js (App Router) + TypeScript app. It shows a small
analytics table for free and offers a **Pro export** (CSV download) at
`GET /api/export`.

**In its pristine state the Pro export is ungated** — anyone can download it.
Your job, as the integrator, is to put it behind a Keysat license: only a
holder of a valid license for this product should be able to export.

This README describes *your own app* — you may read it freely. It tells you
nothing about how Keysat works; for that, use only the Keysat docs you were
pointed at.

## Run it

```sh
npm install        # already done for you in the sandbox
npm run dev        # starts on http://localhost:4311
```

- `GET http://localhost:4311/` — the free report view.
- `GET http://localhost:4311/api/export` — the Pro export (CSV). Currently free.

## What "done" looks like

After integration:

- `GET /api/export` returns the CSV **only** when a valid license is present.
- With **no** license, or a **tampered/invalid** one, `/api/export` is blocked
  (a 4xx, not the CSV).

How the app learns the user's license key (env var, file, header) is your
call — pick whatever the Keysat docs suggest and note it.
