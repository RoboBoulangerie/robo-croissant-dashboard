"""
Stage a newer Croissant export for one KB so it can be reconciled against the
working copy in the dashboard's Reconcile UI (/reconcile/<name>).

Reads croissant_metadata for kb_name from source_db_path, writes it into
staged_kb_versions (replacing any prior staged version for that KB), and runs
flag_structural_issues against it so the reconcile page can show context for
what's wrong with the staged version.

One staged version per KB — re-staging replaces it.

Usage:
    python3 scripts/stage_kb.py BindingDB /path/to/newer/robo_croissant.db
    python3 scripts/stage_kb.py BindingDB /path/to/robo_croissant.db --label "Jason 2026-07-21"
"""

import datetime
import json
import os
import sqlite3
import sys

from lib import ensure_staged_tables
from validate_all import flag_structural_issues

MASTER_DB = "db/robo_croissant.db"


def stage_kb(
    kb_name: str,
    source_path: str,
    master: sqlite3.Connection,
    label: str | None = None,
) -> int:
    """Stage kb_name from source_path into master. Returns the issue count.

    Raises RuntimeError if kb_name isn't in source_path or its metadata isn't
    valid JSON — callers processing many KBs in a batch should catch this per
    KB rather than let one bad entry abort the whole run.
    """
    src = sqlite3.connect(source_path)
    row = src.execute(
        "SELECT croissant_metadata FROM knowledge_bases WHERE name = ?", (kb_name,)
    ).fetchone()
    src.close()

    if row is None:
        raise RuntimeError(f"'{kb_name}' not found in {source_path}")

    metadata_json = row[0]
    try:
        json.loads(metadata_json, strict=False)
    except Exception as e:
        raise RuntimeError(f"'{kb_name}' metadata in {source_path} is not valid JSON — {e}")

    ensure_staged_tables(master)
    source_label = label or os.path.basename(os.path.normpath(source_path))
    staged_at = datetime.datetime.now().strftime("%Y-%m-%d %H:%M:%S")

    cur = master.cursor()
    cur.execute(
        "INSERT INTO staged_kb_versions (kb_name, source_label, croissant_metadata, staged_at) "
        "VALUES (?,?,?,?) "
        "ON CONFLICT(kb_name) DO UPDATE SET "
        "source_label=excluded.source_label, "
        "croissant_metadata=excluded.croissant_metadata, "
        "staged_at=excluded.staged_at",
        (kb_name, source_label, metadata_json, staged_at),
    )

    cur.execute("DELETE FROM staged_kb_issues WHERE kb_name = ?", (kb_name,))
    issues = flag_structural_issues(metadata_json)
    cur.executemany(
        "INSERT INTO staged_kb_issues (kb_name, issue_type, path, value, detail) "
        "VALUES (?,?,?,?,?)",
        [(kb_name, i["issue_type"], i["path"], i["value"], i["detail"]) for i in issues],
    )

    return len(issues)


def main():
    args = sys.argv[1:]
    label = None
    if "--label" in args:
        idx = args.index("--label")
        if idx + 1 >= len(args):
            print("ERROR: --label requires a value"); sys.exit(1)
        label = args[idx + 1]
        args = args[:idx] + args[idx + 2:]

    if len(args) < 2:
        print(__doc__); sys.exit(1)

    kb_name, source_path = args[0], args[1]
    if not os.path.isfile(source_path):
        print(f"ERROR: not a file: {source_path}"); sys.exit(1)

    master = sqlite3.connect(MASTER_DB)

    try:
        issue_count = stage_kb(kb_name, source_path, master, label=label)
    except RuntimeError as e:
        print(f"ERROR: {e}")
        master.close()
        sys.exit(1)

    master.commit()
    master.close()

    print(f"Staged '{kb_name}' from {source_path} ({issue_count} structural issue(s) found).")
    print(f"Review at /reconcile/{kb_name}")


if __name__ == "__main__":
    main()
