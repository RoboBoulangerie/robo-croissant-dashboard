"""
Copy KBs from a pipeline DB into the master working DB so they get
the full review UI (Schema.org Test, Review Fields, Download).

Only copies KBs not already in knowledge_bases — existing entries are
never overwritten. Inline-backfills any leaf paths missing from kb_links.

Usage:
    python3 scripts/promote_kbs.py /path/to/2026_0617/robo_croissant.db
    python3 scripts/promote_kbs.py /path/to/run.db --only BindingDB GlyGen
    python3 scripts/promote_kbs.py /path/to/run.db --dry-run
"""

import json
import os
import sqlite3
import sys

from lib import extract_leaves, backfill_kb_links

MASTER_DB = "db/robo_croissant.db"


def promote_from_db(
    source_path: str,
    master: sqlite3.Connection,
    only: set[str] | None = None,
    dry_run: bool = False,
) -> dict[str, int]:
    """
    Copy KBs from source_path into master. Returns {kb_name: link_count}.
    only: if provided, restrict to these KB names.
    """
    src = sqlite3.connect(source_path)
    existing = {row[0] for row in master.execute("SELECT name FROM knowledge_bases")}
    candidates = src.execute(
        "SELECT name, url, croissant_metadata FROM knowledge_bases"
    ).fetchall()

    to_import = [
        (name, url, meta)
        for name, url, meta in candidates
        if name not in existing and (not only or name in only)
    ]

    if not to_import:
        src.close()
        return {}

    cur = master.cursor()
    totals: dict[str, int] = {}

    for name, url, metadata_json in to_import:
        try:
            metadata = json.loads(metadata_json, strict=False)
        except Exception as e:
            print(f"  SKIP {name}: bad JSON — {e}")
            continue

        src_links = src.execute(
            "SELECT path, value, url, confidence FROM kb_links WHERE kb_name = ?", (name,)
        ).fetchall()
        src_paths = {r[0] for r in src_links}

        extra = [
            (path, value)
            for path, value in extract_leaves(metadata)
            if path not in src_paths
        ]

        n = len(src_links) + len(extra)
        print(f"  {name}: {len(src_links)} links + {len(extra)} backfilled")

        if not dry_run:
            cur.execute(
                "INSERT INTO knowledge_bases (name, url, croissant_metadata) VALUES (?,?,?)",
                (name, url, metadata_json),
            )
            cur.executemany(
                "INSERT OR IGNORE INTO kb_links (kb_name, path, value, url, confidence) "
                "VALUES (?,?,?,?,?)",
                [(name, path, value, link_url, conf)
                 for path, value, link_url, conf in src_links],
            )
            if extra:
                cur.executemany(
                    "INSERT OR IGNORE INTO kb_links (kb_name, path, value, url, confidence) "
                    "VALUES (?,?,?,0.0,0.0)",
                    [(name, path, value) for path, value in extra],
                )
        totals[name] = n

    src.close()
    return totals


def main():
    args = sys.argv[1:]
    dry_run = "--dry-run" in args
    args = [a for a in args if a != "--dry-run"]

    only: set[str] = set()
    if "--only" in args:
        idx = args.index("--only")
        args.pop(idx)
        while idx < len(args) and not args[idx].startswith("--"):
            only.add(args.pop(idx))

    if not args:
        print(__doc__); sys.exit(1)

    source_path = args[0]
    if not os.path.isfile(source_path):
        print(f"ERROR: not a file: {source_path}"); sys.exit(1)

    master = sqlite3.connect(MASTER_DB)
    print(f"Source : {source_path}")
    print(f"{'DRY RUN — ' if dry_run else ''}Importing new KBs:\n")

    totals = promote_from_db(source_path, master, only=only or None, dry_run=dry_run)

    if not totals:
        print("Nothing to import — all KBs already in working DB.")
    elif not dry_run:
        master.commit()
        print(f"\nDone. {len(totals)} KB(s) added.")
    else:
        print(f"\nDry run complete. Would add {len(totals)} KB(s).")

    master.close()


if __name__ == "__main__":
    main()
