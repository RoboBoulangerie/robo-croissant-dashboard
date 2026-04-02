# Robo Croissant Dashboard

A web dashboard for managing [Croissant](https://mlcommons.org/croissant/) metadata — a schema.org-based JSON-LD format for describing ML datasets, maintained by MLCommons. Each record in the dashboard is a named dataset description ("knowledge source") stored in a local SQLite database.

---

## For Users

Open the dashboard in your browser after starting the app (see below). You'll see a table listing all dataset metadata records. For each entry you can:

- **Validate** — submits the JSON to Google's Rich Results Test tool in a new tab to check if the Croissant metadata is valid structured data
- **Update** — opens the field-level review page for that record
- **Download** — downloads the clean Croissant JSON (envelope-stripped) as `<name>.json`

### Reviewing a record

The Update page shows every field the LLM generated, grouped by section (Dataset, Creator, Distribution, etc.). Each field displays:

- **Value** — what the LLM produced
- **Source URL** — the page the LLM cited for that value
- **Confidence** — the LLM's self-reported confidence (color coded: green ≥90%, yellow ≥70%, red below that)

A progress counter at the top tracks **Accepted**, **Edited**, and **Remaining** fields.

For each field you can:
- **Accept** — one click to mark the field as verified without changing it
- **Edit** — opens the value and source URL for editing. Once you're done, click **Accept** to apply or **Cancel** to discard. If you accept without making any changes, the field is marked as accepted (not saved to the database)

Once all fields are reviewed the **Export JSON** button activates, downloading the final clean Croissant file for that source.

Edits (fields where the value or source URL actually changed) are saved to the database and recorded in a `corrections` table for use in the LLM feedback loop.

The app does not currently support creating or deleting records through the UI. To add or remove records, edit the SQLite database directly (see Developer section below).

---

## For Developers

### Prerequisites

#### Rust + Cargo

Install via [rustup](https://rustup.rs/) — works on macOS, Linux, and Windows:

**macOS / Linux:**
```sh
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

**Windows:**
Download and run [rustup-init.exe](https://win.rustup.rs/) from the rustup website.

After installation, verify with:
```sh
cargo --version
```

#### SQLite

**macOS:**
```sh
# Using Homebrew
brew install sqlite

# Using MacPorts
port install sqlite3
```

**Linux (Debian/Ubuntu):**
```sh
sudo apt install libsqlite3-dev
```

**Linux (Fedora/RHEL):**
```sh
sudo dnf install sqlite-devel
```

**Windows:**
Download the precompiled binaries from the [SQLite download page](https://www.sqlite.org/download.html) and add them to your PATH. Alternatively, if you use [Scoop](https://scoop.sh/) or [Chocolatey](https://chocolatey.org/):
```sh
scoop install sqlite
# or
choco install sqlite
```

### Running locally

```sh
cargo run --release
```

The server starts at `http://localhost:8000`.

### Stack

| Layer | Technology |
|-------|-----------|
| Web framework | [Rocket 0.5](https://rocket.rs/) |
| ORM | [Diesel 2](https://diesel.rs/) |
| Database | SQLite (`db/robo_croissant.db`) |
| Templating | [Tera](https://keats.github.io/tera/) (`.html.tera` files) |

### Project structure

```
src/main.rs              — all backend logic (routes, models, DB queries)
templates/
  base.html.tera         — HTML layout with styles and nav
  nav.html.tera          — navigation bar
  index.html.tera        — home page: table of all knowledge sources
  update.html.tera       — edit form for a single record
db/robo_croissant.db     — SQLite database
Rocket.toml              — server configuration
```

### Routes

| Method | Path | Description |
|--------|------|-------------|
| `GET` | `/` | List all knowledge sources (HTML) |
| `GET` | `/update/<name>` | Edit form for a record (HTML) |
| `POST` | `/update/<name>` | Save updated JSON, redirect to `/` |
| `GET` | `/knowledge_source/<name>` | Serve raw Croissant JSON for a record |
| `GET` | `/knowledge_source/names` | JSON array of all record names |

### Database

The database has a single table:

```sql
CREATE TABLE knowledge_sources (
    name VARCHAR(255),       -- unique identifier, used in URLs
    croissant_metadata JSON  -- full Croissant JSON-LD blob
);
```

To inspect or seed the database directly:

```sh
sqlite3 db/robo_croissant.db
```

### Server configuration

`Rocket.toml` controls environment-specific settings:

- **debug** — `127.0.0.1:8000`, 1 worker, keep-alive off
- **release** — `0.0.0.0:8000`, 4 workers, keep-alive on


