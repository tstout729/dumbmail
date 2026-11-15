# DumbMail web UI

The DumbMail web app: Vite, React 19, TypeScript (strict), React Router 7, TanStack Query, Tailwind CSS 4, lucide-react and Recharts. It talks to the Rust backend through the HTTP API in [`../docs/API.md`](../docs/API.md).

## Develop

```sh
cd web
pnpm install
pnpm dev          # http://<host>:7312, listens on all interfaces
```

Run the backend on `127.0.0.1:7311`. The dev server forwards `/api`, `/client`, `/t`, `/u`, `/f`, `/mcp`, `/webhooks`, `/onsite.js` and `/healthz` to it. The proxy rules match whole path segments, so `/f/...` is forwarded but client routes like `/flows` and `/forms` are not.

## Check and build

```sh
pnpm typecheck    # tsrs (Rust port of TypeScript 7); Microsoft's TS7 stays installed as `tsc`
pnpm lint         # ESLint with typescript-eslint and react-hooks
pnpm build        # outputs web/dist
```

The Rust binary embeds `web/dist` when it compiles and serves it with an SPA fallback. After `pnpm build`, rebuild the backend to pick up the new UI.

## Layout

- `src/api/`: typed API client. `types.ts` mirrors `docs/API.md`; each resource module (campaigns, flows, segments, …) holds its fetch calls and TanStack Query hooks. Failed mutations show the API's `error.message` in a toast, unless the screen shows the error inline.
- `src/components/`: the UI kit (`ui/`), app shell with sidebar and command palette (`shell/`), email and SMS editors (`email/`), the segment condition builder (`segments/`), the flow canvas and editors (`flows/`), charts and shared audience pieces.
- `src/pages/`: one folder per area. Each page loads as its own chunk.
- `src/lib/`: formatting, time-zone conversion, toasts and small hooks.

## Conventions

- Fetch data only through TanStack Query hooks, never in `useEffect`.
- Use an effect only to sync with something outside React (timers, window listeners, the API client's 401 hook), and give it a comment naming that system plus a cleanup.
- Show dates in the account's time zone. Scheduling turns the picked wall-clock time in that zone into UTC (`src/lib/tz.ts`).
- Use named exports everywhere except `vite.config.ts`.
