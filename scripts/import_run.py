"""
Import a pipeline run into the master robo_croissant.db for multi-run comparison.

Accepts either:
  - A SQLite .db file produced by the upstream pipeline
  - A directory of JSON files (raw Croissant or wrapped {name, url, croissant_metadata})

The date and model are parsed best-effort from the directory name:
  2026_0611/           → date 2026-06-11, no model
  Azure GPT-5.3-codex/ → no date, model "Azure GPT-5.3-codex"
  2026_0622_gpt54/     → date 2026-06-22, model "gpt54"

Use --label and --model to override parsed values.
Run --update <run_id> to edit the label/model of an already-imported run.

Usage:
    python3 scripts/import_run.py /path/to/2026_0617/robo_croissant.db
    python3 scripts/import_run.py /path/to/2026_0507/
    python3 scripts/import_run.py /path/to/run.db --label "GPT-5.4 high" --model "Azure GPT-5.4"
    python3 scripts/import_run.py --dry-run /path/to/run.db
    python3 scripts/import_run.py --update 3 --label "Re-labeled" --model "Azure GPT-5.4"
"""

import datetime
import json
import os
import sqlite3
import sys

from lib import (
    extract_leaves,
    hash_file,
    hash_json_dir,
    ensure_run_tables,
    insert_run_links,
    load_kb_name_map,
    parse_dir_name,
    resolve_kb_name,
)

MASTER_DB = "db/robo_croissant.db"


def import_db(
    source_path: str,
    master_conn: sqlite3.Connection,
    run_id: int,
    kb_map: dict[str, str],
    dry_run: bool,
) -> dict[str, int]:
    src = sqlite3.connect(source_path)
    cur = master_conn.cursor()
    totals: dict[str, int] = {}

    for raw_name, metadata_json in src.execute(
        "SELECT name, croissant_metadata FROM knowledge_bases"
    ).fetchall():
        canonical = resolve_kb_name([raw_name], kb_map)
        try:
            metadata = json.loads(metadata_json, strict=False)
        except Exception as e:
            print(f"  SKIP {raw_name}: bad JSON — {e}")
            continue

        # Start from existing kb_links in the source DB
        existing: dict[str, str] = {}
        try:
            rows = src.execute(
                "SELECT path, value FROM kb_links WHERE kb_name = ?", (raw_name,)
            ).fetchall()
            existing = {r[0]: r[1] for r in rows}
        except sqlite3.OperationalError:
            pass

        # Backfill any leaf paths not in kb_links
        for path, value in extract_leaves(metadata):
            existing.setdefault(path, value)

        links = list(existing.items())
        if not dry_run:
            n = insert_run_links(cur, run_id, canonical, links)
        else:
            n = len(links)
        totals[canonical] = n
        print(f"  {canonical}: {n} fields")

    src.close()
    return totals


def import_json_dir(
    dir_path: str,
    master_conn: sqlite3.Connection,
    run_id: int,
    kb_map: dict[str, str],
    dry_run: bool,
) -> dict[str, int]:
    cur = master_conn.cursor()
    totals: dict[str, int] = {}

    for fname in sorted(os.listdir(dir_path)):
        if not fname.endswith(".json"):
            continue
        fpath = os.path.join(dir_path, fname)

        try:
            with open(fpath, encoding="utf-8") as f:
                raw = json.load(f)
        except Exception as e:
            print(f"  SKIP {fname}: {e}")
            continue

        obj = raw[0] if isinstance(raw, list) and raw else raw if isinstance(raw, dict) else {}

        if "croissant_metadata" in obj and isinstance(obj["croissant_metadata"], dict):
            file_name = obj.get("name", "")
            metadata = obj["croissant_metadata"]
        elif "@type" in obj or "name" in obj:
            file_name = obj.get("name", "")
            metadata = obj
        else:
            print(f"  SKIP {fname}: unrecognised format")
            continue

        obj_type = metadata.get("@type", "")
        if obj_type and "Dataset" not in obj_type and "Croissant" not in obj_type:
            print(f"  SKIP {fname}: @type={obj_type!r} (not a dataset)")
            continue
        if not obj_type and not metadata.get("name"):
            print(f"  SKIP {fname}: no @type or name field")
            continue

        stem = os.path.splitext(fname)[0]
        canonical = resolve_kb_name([stem, file_name], kb_map)

        links = extract_leaves(metadata)
        if not dry_run:
            n = insert_run_links(cur, run_id, canonical, links)
        else:
            n = len(links)
        totals[canonical] = n
        print(f"  {canonical} (from {fname}): {n} fields")

    return totals


def main():
    args = sys.argv[1:]
    dry_run = "--dry-run" in args
    args = [a for a in args if a != "--dry-run"]

    # --update <id>
    if "--update" in args:
        idx = args.index("--update")
        run_id = int(args[idx + 1])
        label = args[args.index("--label") + 1] if "--label" in args else None
        model = args[args.index("--model") + 1] if "--model" in args else None

        conn = sqlite3.connect(MASTER_DB)
        ensure_run_tables(conn)
        if label:
            conn.execute("UPDATE runs SET label = ? WHERE id = ?", (label, run_id))
        if model is not None:
            conn.execute("UPDATE runs SET model = ? WHERE id = ?",
                         (model or None, run_id))
        conn.commit()
        row = conn.execute("SELECT id, label, model FROM runs WHERE id = ?", (run_id,)).fetchone()
        print(f"Updated run {run_id}: label={row[1]!r}, model={row[2]!r}" if row
              else f"No run found with id {run_id}")
        conn.close()
        return

    # Parse --label / --model flags
    label_override = model_override = source_path = None
    remaining = []
    i = 0
    while i < len(args):
        if args[i] == "--label" and i + 1 < len(args):
            label_override = args[i + 1]; i += 2
        elif args[i] == "--model" and i + 1 < len(args):
            model_override = args[i + 1]; i += 2
        else:
            remaining.append(args[i]); i += 1

    if not remaining:
        print(__doc__); sys.exit(1)

    source_path = remaining[0].rstrip("/")
    if not os.path.exists(source_path):
        print(f"ERROR: path not found: {source_path}"); sys.exit(1)

    is_db  = os.path.isfile(source_path) and source_path.endswith(".db")
    is_dir = os.path.isdir(source_path)
    if not is_db and not is_dir:
        print(f"ERROR: {source_path!r} must be a .db file or a directory"); sys.exit(1)

    dir_name = os.path.basename(os.path.dirname(source_path) if is_db else source_path)
    run_date, model_from_name = parse_dir_name(dir_name)
    model = model_override if model_override is not None else model_from_name

    parts = [p for p in [run_date, model] if p]
    label = label_override or " · ".join(parts) or dir_name or os.path.basename(source_path)

    file_hash = hash_file(source_path) if is_db else hash_json_dir(source_path)
    now = datetime.datetime.now().strftime("%Y-%m-%dT%H:%M:%S")

    print(f"Source  : {source_path}")
    print(f"Label   : {label}")
    print(f"Date    : {run_date or '(unknown)'}")
    print(f"Model   : {model or '(unknown)'}")
    print(f"Hash    : {file_hash[:16]}…")
    if dry_run:
        print("Mode    : DRY RUN — no changes will be written")
    print()

    conn = sqlite3.connect(MASTER_DB)
    ensure_run_tables(conn)
    kb_map = load_kb_name_map(conn)

    existing = conn.execute(
        "SELECT id, label FROM runs WHERE file_hash = ?", (file_hash,)
    ).fetchone()
    if existing:
        print(f"Already imported as run #{existing[0]} ({existing[1]!r}). Nothing to do.")
        print("Use --update to change the label or model.")
        conn.close()
        return

    if dry_run:
        run_id = -1
    else:
        cur = conn.cursor()
        cur.execute(
            "INSERT INTO runs (label, run_date, model, file_hash, imported_at) VALUES (?,?,?,?,?)",
            (label, run_date, model, file_hash, now),
        )
        run_id = cur.lastrowid

    print("Importing fields…")
    totals = import_db(source_path, conn, run_id, kb_map, dry_run) if is_db \
        else import_json_dir(source_path, conn, run_id, kb_map, dry_run)

    if not dry_run:
        conn.commit()

    total_fields = sum(totals.values())
    print()
    if dry_run:
        print(f"Dry run complete. Would import {total_fields} fields across {len(totals)} KBs.")
    else:
        print(f"Done. Run #{run_id} imported: {total_fields} fields across {len(totals)} KBs.")
        print("Open /runs in the dashboard to view or edit this run.")
    conn.close()


if __name__ == "__main__":
    main()
