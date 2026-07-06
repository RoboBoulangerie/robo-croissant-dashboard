# Robo Croissant Dashboard

A web dashboard for reviewing LLM-generated [Croissant 1.1](https://mlcommons.org/croissant/) metadata — a Schema.org-based JSON-LD format for describing ML datasets, maintained by MLCommons. Reviewers examine each field the LLM produced, accept or correct it, and export a clean, validated Croissant JSON file.

---

## Prerequisites

### Rust + Cargo

Install via [rustup](https://rustup.rs/):

```sh
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

Verify:
```sh
cargo --version
```

### SQLite

**macOS:**
```sh
brew install sqlite
```

**Linux (Debian/Ubuntu):**
```sh
sudo apt install libsqlite3-dev
```

**Linux (Fedora/RHEL):**
```sh
sudo dnf install sqlite-devel
```

### Python 3 (for setup scripts)

Python 3.9+ is required. Install the one dependency:

```sh
pip install requests
```

---

## Quick Start

```sh
# 1. Set up the database from one or more pipeline runs
python3 scripts/setup.py /path/to/runs/

# 2. Build and launch the dashboard
cargo run --release
```

Open `http://localhost:8000` in your browser.

---

## Data Setup

### What the upstream pipeline produces

The LLM pipeline writes its output to a SQLite file named `robo_croissant.db`. Each run lives in its own directory, typically named by date and/or model:

```
pipeline-output/
  2026_0601/
    robo_croissant.db
  2026_0617_gpt54/
    robo_croissant.db
  Azure GPT-5.3-codex/
    robo_croissant.db
```

The dashboard never modifies these pipeline files. It imports them read-only into its own working database at `db/robo_croissant.db`.

### Where to put run files for comparison

Place pipeline run directories anywhere on disk — the path is passed to `setup.py` or `import_run.py` at import time. Once imported, the run's field values are stored in `run_links` inside `db/robo_croissant.db` and the originals are no longer needed by the dashboard.

The **Compare Runs** feature on the home page shows which field values changed between runs. This works from the imported history, not from the original files.

### Setting up for the first time

```sh
python3 scripts/setup.py /path/to/runs/
```

`setup.py` does four things in order:

1. **Import runs** — scans the directory for pipeline `.db` files, hashes each one to detect duplicates, and imports new runs into `run_links` for comparison history. Already-imported runs are skipped automatically.

2. **Promote KBs** — copies each knowledge base (KB) from the pipeline DB into the working DB (`db/robo_croissant.db`) so it gets the full review UI. If a KB already exists in the working DB it is never overwritten — the working copy is the reviewer's ground truth.

3. **Backfill** — adds any leaf paths that exist in the pipeline metadata but are missing from `kb_links`. This is the only step that modifies an existing KB's leaf-path records automatically.

4. **Validate** — runs `validate_all.py` against all KBs, auto-approving fields that pass programmatic checks and writing detected issues to the `validation_issues` table.

**Options:**

```sh
# Preview what would happen without writing anything
python3 scripts/setup.py /path/to/runs/ --dry-run

# Skip URL checks (much faster — skips step 4's network I/O)
python3 scripts/setup.py /path/to/runs/ --no-validate

# Promote KBs from a specific run only (by label or date substring)
python3 scripts/setup.py /path/to/runs/ --promote-from 2026_0617
```

### Adding a new pipeline run later

```sh
# Import a single run file into comparison history
python3 scripts/import_run.py /path/to/2026_0701/robo_croissant.db

# Then re-validate
python3 scripts/validate_all.py
```

If the new run contains KBs not yet in the working DB, promote them:

```sh
python3 scripts/promote_kbs.py /path/to/2026_0701/robo_croissant.db
```

---

## Scripts Reference

All scripts live in `scripts/` and are run from the repo root.

### `setup.py` — one-command setup

```sh
python3 scripts/setup.py /path/to/runs/           # whole directory
python3 scripts/setup.py /path/to/run.db          # single file
python3 scripts/setup.py /path/to/runs/ --dry-run
python3 scripts/setup.py /path/to/runs/ --no-validate
python3 scripts/setup.py /path/to/runs/ --promote-from 2026_0617
```

The recommended entry point for first-time setup and for ingesting a batch of runs at once.

### `import_run.py` — import one run into comparison history

```sh
python3 scripts/import_run.py /path/to/2026_0617/robo_croissant.db
python3 scripts/import_run.py /path/to/run.db --label "GPT-5.4 high" --model "Azure GPT-5.4"
python3 scripts/import_run.py --dry-run /path/to/run.db
```

The date and model are parsed from the directory name automatically (e.g. `2026_0622_gpt54` → date 2026-06-22, model `gpt54`). Use `--label` and `--model` to override.

### `promote_kbs.py` — copy KBs from a pipeline DB into the working DB

```sh
python3 scripts/promote_kbs.py /path/to/run.db
python3 scripts/promote_kbs.py /path/to/run.db --only BindingDB GlyGen
python3 scripts/promote_kbs.py /path/to/run.db --dry-run
```

Use `--only` to promote a subset of KBs from a run.

### `validate_all.py` — run programmatic checks

```sh
python3 scripts/validate_all.py                        # all KBs
python3 scripts/validate_all.py --kb "BRCA Exchange"   # one KB
python3 scripts/validate_all.py --dry-run              # preview only
python3 scripts/validate_all.py --reset                # clear auto_reviewed first, then re-run
```

What validation does:
- **Auto-approves** fields that pass programmatic checks (vocabulary, live URL, `source.extract.column` matched against actual file headers, Content-Type/Content-Length from server responses).
- **Flags issues** to the `validation_issues` table, visible at `/issues` in the dashboard.
- **Writes a report** to `reports/llm_feedback_<timestamp>.md` describing every issue found with guidance for the LLM pipeline.

Re-run this any time after adding new pipeline runs or after editing metadata to clear resolved issues.

---

## Dashboard Walkthrough

### Home page (`/`)

Lists every KB in the working DB with:
- **Review Progress** — progress bar showing what fraction of fields are accepted or auto-approved.
- **Issues** — badge showing total issues: red for errors, amber for warnings, green **✓ clean** if the validator ran and found nothing, gray **not validated** if the validator has never been run for that KB. Clicking an error/warning badge goes to `/issues`.
- **Schema.org Test** — submits the KB's JSON to Google's Rich Results Test in a new tab.
- **Review Fields** — opens the field-level review page.
- **Compare Runs** — shows field values that changed between pipeline runs.
- **Download** — downloads the clean Croissant JSON for the KB.

### Review page (`/update/<name>`)

Fields are grouped by section (Dataset, Creator, Distribution, RecordSet, etc.) and paginated 500 fields per page. Each row shows:

- **Field path** — dot-path to the field (e.g. `distribution[0].encodingFormat`). A red flag badge appears if a validation issue exists for this field. A **Δ** badge appears if the value changed across imported pipeline runs; clicking it navigates directly to that field in the Compare Runs page.
- **Value** — the current value. Click to expand long values.
- **Source URL** — the page the LLM cited.
- **Confidence** — LLM self-reported confidence. `⚙ Auto` means the validator auto-approved the field.

**Reviewing a field:**
- **Accept** — marks the field as human-reviewed without changing the value.
- **Edit** — opens inline editing for the value and source URL. After editing, click **Accept** to save or **Cancel** to discard. Changes are written to the database and recorded for the LLM feedback loop.

**Auto-approved fields** (`⚙ Auto`) have been verified programmatically — the value passed a vocabulary check or was confirmed against live server data. You do not need to manually accept these, but you can override them.

The **Export JSON** button in the header activates once all fields are accepted or auto-approved, and downloads the final clean Croissant file.

The sticky header also shows:
- **Schema panel** — lists all RecordSets and their fields with schema structure; flags placeholder field names.
- **Fix Suggestions panel** — if structural issues were detected (e.g. missing field sources, archive files without inner file definitions), this panel proposes fixes with a preview. Each fix must be explicitly approved — nothing changes in the metadata without your confirmation.

### Issues page (`/issues`)

Aggregated view of all validation issues across all KBs, grouped by KB and collapsible. Each issue shows the field path, current value, and the detail message explaining the problem.

**Issue types:**

| Severity | Type | Meaning |
|----------|------|---------|
| Error | `missing_required_field` | `name`, `description`, `url`, `license`, or `conformsTo` is absent |
| Error | `missing_field_source` | `cr:Field` has no `source` property |
| Warning | `duplicate_distribution_id` | Two distributions share the same `@id` |
| Warning | `relative_content_url` | `contentUrl` is not an absolute URL |
| Warning | `wrong_distribution_type` | Distribution `@type` is not `cr:FileObject` or `cr:FileSet` |
| Warning | `format_extension_mismatch` | URL extension conflicts with `encodingFormat` |
| Warning | `placeholder_value` | Value looks like a generated placeholder (e.g. `[license]`, `TODO`) |
| Warning | `placeholder_field_name` | Field name is `column_1`, `column_2`, etc. |
| Warning | `license_is_terms_page` | `license` URL points to a terms-of-service page, not a data license |
| Warning | `invalid_checksum_format` | `md5` is not 32 hex chars, or `sha256` is not 64 hex chars |
| Warning | `inline_data_rows` | RecordSet has `data` rows when it should use `examples` |
| Warning | `zip_without_inner_file` | Compressed archive contains tabular data but no inner `cr:FileObject` is defined |
| Info | `url_unreachable` | HEAD request to `contentUrl` failed |
| Info | `url_error_response` | HEAD request returned an HTTP error |

### Compare Runs page (`/compare/<name>`)

Shows a side-by-side diff of field values across all imported pipeline runs for a given KB. Only fields whose value actually differs between runs are shown — a KB being absent from a run does not count as a change. Click any value cell to expand it and see the full text. Rows targeted by the Δ badge from the review page are highlighted in yellow when the page loads.

---

## Database Layout

The working database lives at `db/robo_croissant.db`.

```
db/
  robo_croissant.db        ← working copy (reviewed by humans)
```

**Do not** put pipeline run files in `db/`. Pipeline runs are imported from wherever they live on disk; the dashboard only writes its own working DB here.

### Key tables

| Table | Purpose |
|-------|---------|
| `knowledge_bases` | One row per KB: `name`, `url`, `croissant_metadata` (full JSON blob) |
| `kb_links` | One row per leaf field: `kb_name`, `path`, `value`, `url`, `confidence`, `reviewed`, `auto_reviewed` |
| `validation_issues` | Issues from the last `validate_all.py` run: `kb_name`, `issue_type`, `path`, `value`, `detail` |
| `runs` | Imported pipeline run metadata: `id`, `label`, `run_date`, `model`, `file_hash` |
| `run_links` | Field values from each pipeline run (read-only history for comparison) |
| `corrections` | Fields where a reviewer changed the value or source URL |

### Replacing or updating the database

SQLite writes sidecar files when the database is open:
- `robo_croissant.db-wal` — write-ahead log
- `robo_croissant.db-shm` — shared memory index

If you swap in a new `.db` file while the app is stopped, delete the sidecars first:

```sh
# Stop the app, then:
rm -f db/robo_croissant.db-wal db/robo_croissant.db-shm
cp /path/to/new/robo_croissant.db db/robo_croissant.db
```

---

## Developer Reference

### Stack

| Layer | Technology |
|-------|-----------|
| Web framework | [Rocket 0.5](https://rocket.rs/) |
| ORM | [Diesel 2](https://diesel.rs/) |
| Database | SQLite |
| Templating | [Tera](https://keats.github.io/tera/) (`.html.tera` files) |

### Project structure

```
src/
  main.rs              — all routes, business logic, template context building
  db_model.rs          — Diesel model structs
  db_schema.rs         — Diesel table! macros
scripts/
  setup.py             — one-command setup (import + promote + backfill + validate)
  import_run.py        — import a pipeline run into comparison history
  promote_kbs.py       — copy KBs from a pipeline DB into the working DB
  validate_all.py      — programmatic validation + issue flagging
  lib.py               — shared utilities (leaf extraction, backfill, run discovery)
templates/
  base.html.tera       — HTML layout, shared styles, nav
  nav.html.tera        — navigation bar
  index.html.tera      — home page
  update.html.tera     — field-level review page
  issues.html.tera     — validation issues viewer
  compare.html.tera    — cross-run field comparison
  runs.html.tera       — imported run list
db/
  robo_croissant.db    — working database (gitignored)
reports/               — LLM feedback reports from validate_all.py (gitignored)
Rocket.toml            — server config (ports, workers)
```

### Routes

| Method | Path | Description |
|--------|------|-------------|
| `GET` | `/` | Home page — KB list with progress and issues |
| `GET` | `/update/<name>?<page>` | Field review page (paginated) |
| `POST` | `/update/<name>/fields?<page>` | Save field edits |
| `GET` | `/issues` | All validation issues across all KBs |
| `GET` | `/knowledge_base/<name>` | Serve raw Croissant JSON |
| `GET` | `/knowledge_base/names` | JSON array of KB names |
| `GET` | `/compare/<name>` | Cross-run field comparison |
| `GET` | `/runs` | List imported pipeline runs |
| `GET` | `/suggest_fixes/<name>` | JSON list of structural fix proposals (read-only) |
| `POST` | `/apply_fix/<name>` | Apply a reviewer-approved structural fix |

### Server configuration

`Rocket.toml` sets environment-specific defaults:

- **debug** — `127.0.0.1:8000`, 1 worker
- **release** — `0.0.0.0:8000`, 4 workers

To change the port:
```toml
# Rocket.toml
[release]
port = 9000
```
