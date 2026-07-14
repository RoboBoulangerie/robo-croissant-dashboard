"""
Backfill kb_links with leaf paths that exist in croissant_metadata but are
missing from kb_links (most commonly @id and @type fields excluded by the
LLM pipeline).

This script is mostly superseded by import_run.py and promote_kbs.py, which
now do inline backfilling. Run it if you need to fill gaps for KBs that were
loaded into the working DB before inline backfilling was added.

Usage:
    python3 scripts/backfill_missing_paths.py
    python3 scripts/backfill_missing_paths.py --dry-run
    python3 scripts/backfill_missing_paths.py --kb "BindingDB"
"""

import sqlite3
import sys

from lib import backfill_kb_links

DB_PATH = "db/robo_croissant.db"


def main():
    import json
    dry_run = "--dry-run" in sys.argv
    filter_kb = None
    if "--kb" in sys.argv:
        idx = sys.argv.index("--kb")
        filter_kb = sys.argv[idx + 1]

    conn = sqlite3.connect(DB_PATH)
    cur = conn.cursor()

    cur.execute("SELECT name, croissant_metadata FROM knowledge_bases ORDER BY name")
    rows = cur.fetchall()

    grand_total = 0
    for kb_name, metadata_json in rows:
        if filter_kb and kb_name != filter_kb:
            continue
        try:
            metadata = json.loads(metadata_json, strict=False)
        except Exception as e:
            print(f"{kb_name}: SKIP bad JSON — {e}")
            continue

        n = backfill_kb_links(conn, kb_name, metadata, dry_run=dry_run)
        if n:
            print(f"{kb_name}: {'would add' if dry_run else 'added'} {n} missing paths")
        else:
            print(f"{kb_name}: nothing to add")
        grand_total += n

    if dry_run:
        print(f"\nDry run — {grand_total} paths would be added.")
    else:
        conn.commit()
        print(f"\nDone. {grand_total} rows inserted.")
    conn.close()


if __name__ == "__main__":
    main()
