# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

A local SQL tool built with a Rust core and React/TypeScript frontend. It supports MySQL/MariaDB, PostgreSQL, SQLite (and experimental SQL Server/MongoDB/Redis/Oracle via capability levels). The app runs in two modes:

- **Web mode**: `web-server` (Axum) serves both a REST API and the static `web-ui/dist` frontend
- **Desktop mode**: `web-ui/src-tauri` wraps the same frontend in a Tauri 2 shell with direct `core_lib` calls via `invoke()`

The frontend auto-detects Tauri at runtime (`isDesktopRuntime()`) and falls back to HTTP when Tauri invoke fails (`DESKTOP_HTTP_FALLBACK:` prefix).

## Build & Run Commands

### Rust workspace (three crates: `core_lib`, `web-server`, `e2e-runner`)
```bash
cargo build --workspace          # build all
cargo test --workspace           # run all Rust tests
cargo test -p core_lib           # run core_lib tests only
cargo run -p web-server          # start API server on :3000
```

### Frontend (`web-ui/`)
```bash
cd web-ui && npm ci               # install dependencies
npm run dev                       # dev server (Vite, proxies /backend → :3000)
npm run build                     # tsc -b && vite build → dist/
npm run test                      # vitest run (node environment)
npm run lint                      # eslint
```

### Tauri desktop (`web-ui/src-tauri/`)
```bash
cd web-ui && npx tauri dev        # dev mode with hot reload
npx tauri build                   # production build
```

### E2E runner
```bash
cargo run -p e2e-runner           # requires E2E_MYSQL_URL, E2E_POSTGRES_URL, etc.
```

See `docs/e2e-real-env.md` for required environment variables.

## Architecture

### Workspace layout
- **`core_lib/`** — shared Rust library: DB client (`db.rs`), schema extraction (`schema.rs`), SQL execution, AI planning (`ai/`), sync engines (`mysql_sync.rs`, `sync.rs`), transfer engine (`transfer.rs`), config management (`config.rs`), rule engine (`rule_engine.rs`), knowledge base (`knowledge_base.rs`), Navicat file parsing (`navicat.rs`), offline SQL parser (`offline_parser.rs`), performance reporting (`perf_report.rs`), timeout policies (`timeout_policy.rs`)
- **`web-server/`** — Axum HTTP server. Routes are defined in `main.rs` under the `/backend` prefix. All mutable state lives in `AppState` (wrapped in `Arc<RwLock<…>>`). AI-specific handlers are in `ai_handlers.rs`
- **`web-ui/`** — React 19 + TypeScript + Vite + Tailwind. Monaco editor for SQL input. State is local (no Redux/Zustand). API client in `api.ts` handles both Tauri invoke and HTTP modes. All UI text uses `tr(zh, en)` from `i18n.ts` for bilingual support
- **`web-ui/src-tauri/`** — Tauri 2 desktop wrapper. `lib.rs` contains all Tauri commands that call `core_lib` directly. `main.rs` is minimal (calls `app_lib::run()`)
- **`e2e-runner/`** — standalone Axum-based E2E test runner that exercises real DB connections

### Key design patterns

**DB connection model**: `DbClient` wraps `sqlx::MySqlPool`. A connection cache (`db_client_cache`) with TTL manages multiple connections by URL hash. The active connection is selected via `db_id` from `AppConfig.db_connections`.

**AI layer** (`core_lib/src/ai/`): `gateway.rs` handles multi-provider HTTP calls (OpenAI, Deepseek, Moonshot, Zhipu, Anthropic, custom). `planner.rs` orchestrates SQL generation. `policy_store.rs` manages prompt policies with snapshot/rollback. `prompting.rs` builds prompt templates. `extractor.rs` parses structured AI outputs.

**Query execution**: Supports chunked streaming (`chunk_offset`/`chunk_size`), transaction management (`execute_transaction`), and query cancellation (`execute_cancel`). Results include `transaction_state` tracking.

**Config persistence**: `AppConfig` (in `core_lib/src/config.rs`) is loaded from/saved to a JSON file in the user's home directory (`dirs::home_dir()`).

### Frontend component structure
`App.tsx` is the monolithic main component (~1700+ lines) containing the SQL workbench. Key components:
- `DbExplorerSidebar` — database tree navigator
- `QueryEditorActionPanel` — Monaco editor + action buttons
- `QueryResultsPanel` — result table with pagination
- `DataTable` / `SimpleDataTable` — virtualized table rendering (uses `@tanstack/react-virtual`)
- `Tabs` — workbench tab management
- Lazy-loaded: `ExecutionPlan`, `QueryBuilder`, `TableWorkspace`, `SessionInfoPanel`

### i18n
Bilingual (zh/en) via `tr(zh, en)` function. Backend also supports locale via `x-locale` header / `Accept-Language`.

## CI

GitHub Actions (`.github/workflows/ci.yml`):
- Rust: `cargo test --workspace` on ubuntu-latest
- Web UI: `npm ci && npm run build` in `web-ui/` with Node 20

## Important Notes

- All API routes are prefixed with `/backend` (Axum nests them; Vite dev proxy forwards `/backend` → `localhost:3000`)
- The Tauri app and web-server are **separate binaries** — Tauri calls `core_lib` directly, web-server exposes it via HTTP
- `web-server/src/bin/` contains standalone binaries for MySQL sync benchmarking and performance CI gates
- `sqlx` is configured with MySQL, PostgreSQL, and SQLite features; the runtime is `tokio-rustls`
- Test files: `core_lib/tests/timeout_policy_test.rs`, `web-ui/src/sqlStatements.test.ts`, `web-ui/src/utils.test.ts`
