"""
Resync kb_links to exactly match a KB's current croissant_metadata: removes
stale/orphaned leaf paths, updates paths whose value changed (resetting
reviewed/auto_reviewed on those), and adds any new paths.

Run this after any whole-document replace — a full-JSON save via /update/<name>
or a /reconcile/<name>/commit — since those can reorder or resize arrays like
distribution/recordSet, which backfill_missing_paths.py's purely-additive
approach won't catch. Without this, kb_links (and the /update review grid and
validation issues built from it) silently drift out of sync with the actual
document.

Usage:
    python3 scripts/resync_kb_links.py --kb CADRE
    python3 scripts/resync_kb_links.py --kb CADRE --dry-run
    python3 scripts/resync_kb_links.py            # all KBs
"""

import json
import sqlite3
import sys

from lib import resync_kb_links

DB_PATH = "db/robo_croissant.db"


def main():
    dry_run = "--dry-run" in sys.argv
    filter_kb = None
    if "--kb" in sys.argv:
        idx = sys.argv.index("--kb")
        filter_kb = sys.argv[idx + 1]

    conn = sqlite3.connect(DB_PATH)
    cur = conn.cursor()
    cur.execute("SELECT name, croissant_metadata FROM knowledge_bases ORDER BY name")
    rows = cur.fetchall()

    for kb_name, metadata_json in rows:
        if filter_kb and kb_name != filter_kb:
            continue
        try:
            metadata = json.loads(metadata_json, strict=False)
        except Exception as e:
            print(f"{kb_name}: SKIP bad JSON — {e}")
            continue

        result = resync_kb_links(conn, kb_name, metadata, dry_run=dry_run)
        if result["added"] or result["removed"] or result["updated"]:
            verb = "would change" if dry_run else "changed"
            print(f"{kb_name}: {verb} — +{result['added']} added, "
                  f"-{result['removed']} removed, ~{result['updated']} updated")
        else:
            print(f"{kb_name}: already in sync")

    print("\nDry run — no changes written." if dry_run else "\nDone.")
    conn.close()


if __name__ == "__main__":
    main()
