# Agent Tooling Guidelines

This repository follows these conventions when creating projects, installing dependencies, and running code.

### Node.js / JavaScript (use **Bun**)

- **Install dependencies**: use `bun install`
- **Add a dependency**:
  - `bun add <pkg>` (runtime dependency)
  - `bun add -d <pkg>` (dev dependency)
- **Remove a dependency**: `bun remove <pkg>`
- **Run a JS/TS file**: `bun <file>` (example: `bun src/index.ts`)
- **Run a package.json script**: `bun run <script>`
- **One-off CLI execution (npx equivalent)**: `bunx <pkg> [args...]`

Avoid using `npm`, `yarn`, or `pnpm` in this repo unless explicitly required.

### Python (use **uv**)

- **Create a virtual environment**: `uv venv`
- **Run a command in the project environment**: `uv run <command>`
  - Example: `uv run python -m your_module`
- **Add dependencies**: `uv add <pkg>`
- **Add dev dependencies**: `uv add --dev <pkg>`
- **Install/sync from lock**: `uv sync`

Prefer `uv` over `pip`, `pip-tools`, or `poetry` unless explicitly required.

### Rust (use **cargo**)

- **Create a new project**:
  - Binary: `cargo init --bin --edition 2024`
  - Library: `cargo init --lib --edition 2024`
- **Add dependencies**: use `cargo add <crate>`
  - Example: `cargo add anyhow`

Conventions:
- Use **Rust 1.92.0** and **edition 2024**.
- **Library/crate** error handling: use `thiserror`.
- **CLI / non-library** error handling: use `anyhow`.
- Do **not** run Rust code inside a Docker container.

### Databases (DuckDB / PostgreSQL / MySQL)

These CLI clients are available in the Docker image: `duckdb`, `psql`, `mysql`.

- **DuckDB (CLI)**:
  - Open a local DB file: `duckdb path/to.db`
  - In-memory session: `duckdb :memory:`
  - Run a query non-interactively: `duckdb path/to.db -c "SELECT 42;"`
- **PostgreSQL (psql)**:
  - Connect via URL: `psql "postgresql://USER:PASSWORD@HOST:5432/DBNAME"`
  - Connect via flags: `psql -h HOST -p 5432 -U USER -d DBNAME`
- **MySQL / MariaDB (mysql)**:
  - Connect: `mysql -h HOST -P 3306 -u USER -p DBNAME`

If you need language drivers, install them using the repo conventions:
- **Node.js**: `bun add pg mysql2` (and only add DuckDB bindings if you actually need them)
- **Python**: `uv add psycopg[binary] mysql-connector-python duckdb`
- **Rust**: `cargo add tokio-postgres mysql_async duckdb` (or `sqlx` if you want one crate across DBs)

### Tools available in the Docker image

Derived from `Dockerfile` + `docker/versions.env`:

- **Pinned tools**:
  - **Rust** via rustup (toolchain `1.92.0`)
  - **Go** (`1.25.5`)
  - **Bun** (`1.3.5`)
  - **uv/uvx** (`0.9.22`) + **Python** installed via `uv python install` (default `3.13`)
  - **DuckDB CLI** (`1.4.3`)
  - **opencode** (`v1.1.7`) (default `CMD` under `tini`)
- **Database CLIs**:
  - `duckdb`, `psql` (postgresql-client), `mysql` (default-mysql-client), `sqlite3`
- **Build tooling**:
  - `build-essential`, `make`, `cmake`, `pkg-config`, `libssl-dev`
- **Common utilities**:
  - `git`, `curl`, `wget`, `jq`, `ripgrep` (`rg`), `fd`, `rsync`, `tree`, `zip`/`unzip`, `openssh-client`, `sudo`
- **Container tooling**:
  - `docker` (via `docker.io`)
