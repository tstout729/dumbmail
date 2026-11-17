# Working on DumbMail

The code lives at https://github.com/tstout729/dumbmail. Work in `~/projects/dumbmail` (or a worktree of it), never in an old scratch copy.

## Layout

- `src/`: the Rust server (axum + sqlx on Postgres). `lib.rs` wires it up; `api/*` is the dashboard and REST API; `public.rs` serves the tracking, unsubscribe, form and `/healthz` routes; `mcp.rs` is the MCP server; `jobs.rs` is the job queue and scheduler.
- `src/features/`: one module per feature (integrations, catalog, team and so on). A feature plugs its routes, MCP tools and docs, jobs and scheduler work in through the `Feature` trait in `src/features/mod.rs`, so it doesn't touch `lib.rs`, `mcp.rs` or `jobs.rs`. Its dashboard pages, API hooks and types live in `web/src/features/<name>/` and plug in through `index.ts` (routes, settings tabs, sidebar entries).
- `src/segments.rs`: the segment rule language, compiled to parameterized SQL. Campaign audiences, flow conditions, flow filters and exit conditions all use it.
- `migrations/`: sqlx migrations, applied by the server when it starts. Add a new numbered file; never edit one that has shipped.
- `web/`: the dashboard (React, Vite, Tailwind), built into the binary.
- `tests/`: end-to-end tests against a real Postgres. Set `TEST_DATABASE_URL` to an admin URL; each test makes and drops its own database. `tests/common/mod.rs` is the shared harness; put a new feature's tests in their own file (`tests/<feature>.rs`) that starts with `#[macro_use] mod common;`.

## Rules

- Anything the dashboard can do, the API and the MCP server can do too. When you add a feature, add its MCP tools and document it: core features in `docs/API.md` and the MCP reference docs in `src/mcp.rs`; a feature module in its own `docs/<FEATURE>.md`, served to MCP clients through the module's `mcp_docs`.
- Build new features as feature modules (see Layout). Change the shared files only for what a module can't do from its own.
- Values in SQL are always bound (`push_bind`), never formatted into the query.
- Dates people see, and day boundaries, use the account's time zone.

## Checks

CI (`.github/workflows/ci.yml`) runs on every pull request. Its `ci` job is the required check:

```sh
cargo clippy --all-targets --locked -- -D warnings
TEST_DATABASE_URL=postgres://postgres@localhost:5432/postgres cargo test --locked
pnpm -C web typecheck && pnpm -C web lint && pnpm -C web build
```

Run clippy and the web checks before you push. Leave the full test run to CI.

## Release

- Every change goes through a pull request into `main`, with auto-merge on.
- Merging is the release. On each push to `main`, CI deploys that commit to Railway (project `postlark`, service `postlark`, environment `production`) and waits until https://dumbmail.ai/healthz reports it as `git_sha`.
- Don't run `railway up` by hand. A build from a local folder can lack migrations production already applied, and then the server can't start; that took dumbmail.ai down on 2026-11-16.
- After a merge, confirm the deploy job passed. Then check the changed flows on dumbmail.ai and the logs: `railway logs --service postlark --environment production` (link the project once with `railway link`).
