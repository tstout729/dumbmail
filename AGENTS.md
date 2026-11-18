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
- **Marketing hold** (`profiles.marketing_hold`, helpers in `src/marketing_hold.rs`). It means "we have this person's details for service reasons (a booking, an order), but they haven't agreed to marketing". Only code that *creates* a profile from a source without an opt-in may set it, and only on the profile it just created (`marketing_hold::hold_new_profile`): an integration whose owner hasn't confirmed its contacts opted in, or an import of people who never agreed. Never set it on a profile that already existed. It's enforced in two places: campaign audiences (`marketing_hold::push_campaign_filter`, called from `campaigns::push_eligible`) and the final email gate in `sending::send` (`marketing_hold::blocks_email`), which lets a held person get email only from a flow with `settings.service_messages: true`. Segments' `can_receive` email rule is false for held people. Unsubscribes and suppressions are checked first and always win. The hold clears on a real opt-in: a database trigger keeps a profile whose `email_consent` is `subscribed` from ever being held, whatever path subscribed them.
- Every route and MCP tool needs a permission rule, or it's refused (403 naming the route or tool and where to add it). One guard checks them for every request, keyed on the matched route and method (`src/features/team/guard.rs`), so a handler never checks roles itself. A feature declares its rules in `access()`, with paths exactly as registered:

  ```rust
  use crate::features::team::access::{public, rule, Access, Perm, RouteRule, ToolRule};
  static ROUTES: &[RouteRule] = &[rule("GET", "/api/things", Perm::ContentRead), rule("POST", "/api/things/{id}/send", Perm::CampaignSend), public("POST", "/webhooks/things")];
  static TOOLS: &[ToolRule] = &[ToolRule { name: "list_things", perm: Perm::ContentRead }];
  fn access(&self) -> Access { Access { routes: ROUTES, tools: TOOLS } }
  ```

  Pick the narrowest `Perm` in `src/features/team/access.rs` (roles and API key scopes follow from it; `Perm::TransactionalSend` is reserved for transactional sending). `public` is only for routes with no sign-in (webhooks, public pages); their handlers get no permission check. Checks that depend on data (only drafts, only teammates) go in the write itself. Record changes with `features::team::audit(state, ctx, action, target, details)` right after they commit, with fixed fields, never request bodies. `tests/team.rs` checks every rule against the real router.

## Checks

CI (`.github/workflows/ci.yml`) runs on every pull request. Its `ci` job is the required check:

```sh
cargo clippy --all-targets --locked -- -D warnings
TEST_DATABASE_URL=postgres://postgres@localhost:5432/postgres cargo test --locked
pnpm -C web typecheck && pnpm -C web lint && pnpm -C web build
```

Before you push, run clippy and the web checks, and at most the one test file your change touches (`cargo test --test <name>`, or `--lib` with a filter). Never run the whole suite (`cargo test` with no `--test` or filter, or `cargo build --tests`) on the agent host: it links every test binary, and several sessions doing it at once stalled the host's disk and T3 on 2026-11-17. Push and let CI run it; read a failure with `gh run view <run> --log-failed`. The host's `cargo` refuses a whole-suite run, shares one target folder across dumbmail worktrees (one build at a time) and stops the whole build on a timeout.

The deploy image is built from only the paths the `Dockerfile` copies (`src`, `migrations`, `docs`, the built dashboard). If the server embeds a file from any other folder (`include_str!`, `include_bytes!`, rust-embed), add a `COPY` for that folder to the `Dockerfile` in the same PR. CI runs `scripts/check-docker-context.sh` to catch a miss; a missed folder took a deploy down on 2026-11-17.

## Release

- Every change goes through a pull request into `main`, with auto-merge on.
- Merging is the release. On each push to `main`, CI deploys that commit to Railway (project `postlark`, service `postlark`, environment `production`) and waits until https://dumbmail.ai/healthz reports it as `git_sha`.
- Don't run `railway up` by hand. A build from a local folder can lack migrations production already applied, and then the server can't start; that took dumbmail.ai down on 2026-11-16.
- Production hosts: the dashboard, new email links and forms are on PUBLIC_URL (app.dumbmail.ai once SITE_URL is set); the landing page, Pricing, About, Privacy and Terms are on SITE_URL (dumbmail.ai). dumbmail.ai keeps answering the API, MCP, webhooks, tracking links, `/healthz` and the Google sign-in callback, so the deploy check above still works. See "Marketing site and app on separate hosts" in the README.
- After a merge, confirm the deploy job passed. Then check the changed flows on dumbmail.ai and the logs: `railway logs --service postlark --environment production` (link the project once with `railway link`).
