"""
One-command dashboard setup. Discovers pipeline runs in a directory, imports
them all, promotes any brand-new KBs to the working DB, stages any changed
re-runs of KBs already in the working DB for reconciliation (/reconcile/<name>),
backfills, and validates.

Run this instead of the individual scripts:

    python3 scripts/setup.py /path/to/runs/directory/

Or for a single run:

    python3 scripts/setup.py /path/to/2026_0617/robo_croissant.db

Then launch the dashboard:

    cargo run --release

Options:
    --no-validate    Skip the validate_all step (faster, no URL checks)
    --dry-run        Preview what would happen, no writes
    --promote-from LABEL_OR_DATE
                     Promote KBs to working DB from this specific run only
                     (default: most recent run containing each KB)
"""

import json
import os
import sqlite3
import sys

from lib import (
    discover_runs,
    ensure_run_tables,
    ensure_staged_tables,
    hash_file,
    hash_json_dir,
    load_kb_name_map,
    backfill_kb_links,
)
from import_run import import_db, import_json_dir
from promote_kbs import promote_from_db
from stage_kb import stage_kb

MASTER_DB = "db/robo_croissant.db"


def _run_validation(dry_run: bool, kb_names: list[str]):
    """Call validate_all logic directly (no subprocess)."""
    import datetime
    try:
        import validate_all
    except ImportError as e:
        print(f"  Could not import validate_all: {e}")
        return

    conn = sqlite3.connect(MASTER_DB)
    conn.row_factory = sqlite3.Row
    cur = conn.cursor()
    validate_all.ensure_issues_table(cur)

    now_str = datetime.datetime.now().strftime("%Y-%m-%d %H:%M:%S")
    grand_auto = 0
    all_issues = []

    for kb_name in kb_names:
        cur.execute("SELECT path, value, url FROM kb_links WHERE kb_name = ?", (kb_name,))
        links = [dict(r) for r in cur.fetchall()]
        print(f"  {kb_name}: {len(links)} fields", flush=True)

        cur.execute("SELECT croissant_metadata FROM knowledge_bases WHERE name = ?", (kb_name,))
        meta_row = cur.fetchone()
        metadata_json = meta_row["croissant_metadata"] if meta_row else "{}"

        passing, issues = validate_all.validate_kb(links)
        issues = issues + validate_all.flag_structural_issues(metadata_json)

        for iss in issues:
            iss["kb_name"] = kb_name
        all_issues.extend(issues)

        if not dry_run:
            passing_list = sorted(passing)
            for start in range(0, len(passing_list), 500):
                chunk = passing_list[start:start + 500]
                ph = ",".join("?" * len(chunk))
                cur.execute(
                    f"UPDATE kb_links SET auto_reviewed = 1 "
                    f"WHERE kb_name = ? AND path IN ({ph})",
                    [kb_name] + chunk,
                )
            grand_auto += len(passing)
            cur.execute("DELETE FROM validation_issues WHERE kb_name = ?", (kb_name,))
            cur.executemany(
                "INSERT INTO validation_issues "
                "(kb_name, issue_type, path, value, detail, created_at) VALUES (?,?,?,?,?,?)",
                [(kb_name, i["issue_type"], i["path"], i["value"], i["detail"], now_str)
                 for i in issues if i.get("kb_name") == kb_name],
            )

    if all_issues and not dry_run:
        import datetime as _dt
        os.makedirs("reports", exist_ok=True)
        ts = _dt.datetime.now().strftime("%Y%m%d_%H%M%S")
        report_path = os.path.join("reports", f"llm_feedback_{ts}.md")
        validate_all.write_llm_report(all_issues, kb_names, report_path)
        print(f"  Report → {report_path}")

    if not dry_run:
        conn.commit()
        print(f"  {grand_auto} fields auto-verified, {len(all_issues)} issues recorded.")
    else:
        print(f"  Dry run — would verify ~{len(set(i['path'] for i in all_issues))} paths.")

    conn.close()


def main():
    import datetime

    args = sys.argv[1:]
    dry_run      = "--dry-run"      in args
    no_validate  = "--no-validate"  in args
    args = [a for a in args if a not in ("--dry-run", "--no-validate")]

    promote_from_filter = None
    if "--promote-from" in args:
        idx = args.index("--promote-from")
        promote_from_filter = args[idx + 1]
        args = args[:idx] + args[idx + 2:]

    if not args:
        print(__doc__)
        sys.exit(1)

    source_root = args[0].rstrip("/")
    if not os.path.exists(source_root):
        print(f"ERROR: path not found: {source_root}")
        sys.exit(1)

    # ── 1. Discover runs ──────────────────────────────────────────────────────
    runs = discover_runs(source_root)
    if not runs:
        print(f"No pipeline runs found under: {source_root}")
        sys.exit(1)

    print(f"Found {len(runs)} run(s) in {source_root}:\n")
    for r in runs:
        print(f"  [{r['run_date'] or '????-??-??'}] {r['dir_name']}  ({r['path']})")
    print()

    if dry_run:
        print("DRY RUN — no changes will be written.\n")

    conn = sqlite3.connect(MASTER_DB)
    ensure_run_tables(conn)

    # ── 2. Import each run into run_links ─────────────────────────────────────
    print("── Step 1: Import runs into comparison history ───────────────────")
    newly_imported: list[dict] = []

    for r in runs:
        path     = r["path"]
        dir_name = r["dir_name"]
        run_date = r["run_date"]
        model    = r["model"]

        file_hash = hash_file(path) if r["is_db"] else hash_json_dir(path)
        existing  = conn.execute(
            "SELECT id, label FROM runs WHERE file_hash = ?", (file_hash,)
        ).fetchone()

        if existing:
            print(f"  {dir_name}: already imported as run #{existing[0]} — skipped")
            continue

        parts = [p for p in [run_date, model] if p]
        label = " · ".join(parts) or dir_name
        now   = datetime.datetime.now().strftime("%Y-%m-%dT%H:%M:%S")

        if dry_run:
            run_id = -1
            print(f"  {dir_name}: would import as '{label}'")
        else:
            cur = conn.cursor()
            cur.execute(
                "INSERT INTO runs (label, run_date, model, file_hash, imported_at) "
                "VALUES (?,?,?,?,?)",
                (label, run_date, model, file_hash, now),
            )
            run_id = cur.lastrowid
            print(f"  {dir_name}: importing as run #{run_id} '{label}'")

        kb_map = load_kb_name_map(conn)
        if r["is_db"]:
            totals = import_db(path, conn, run_id, kb_map, dry_run)
        else:
            totals = import_json_dir(path, conn, run_id, kb_map, dry_run)

        if not dry_run:
            conn.commit()

        newly_imported.append({**r, "run_id": run_id, "label": label, "totals": totals})
        print(f"    → {sum(totals.values())} fields across {len(totals)} KBs")

    print()

    # ── 3. Promote new KBs / stage changed KBs for reconciliation ─────────────
    print("── Step 2: Promote new KBs, stage changed KBs for reconciliation ─")
    existing_kbs = {row[0] for row in conn.execute("SELECT name FROM knowledge_bases")}

    # Determine which source DBs to scan.
    # Default: scan runs newest-to-oldest, use the first run that has each KB.
    source_runs = [r for r in reversed(runs) if r["is_db"]]
    if not source_runs:
        print("  No .db files found — JSON-only runs cannot promote/reconcile KBs (no metadata JSON).")
        print("  Skipping promote/reconcile step.\n")
    else:
        if promote_from_filter:
            source_runs = [r for r in source_runs if promote_from_filter in r["path"]]
            if not source_runs:
                print(f"  --promote-from '{promote_from_filter}' matched no runs.")

        ensure_staged_tables(conn)

        promoted_total = 0
        staged_total = 0
        reconciled_this_run: set[str] = set()  # avoid restaging from an older run once handled

        for r in source_runs:
            src_path = r["path"]
            label = " · ".join(p for p in [r["run_date"], r["model"]] if p) or r["dir_name"]

            src = sqlite3.connect(src_path)
            src_kbs = {row[0] for row in src.execute("SELECT name FROM knowledge_bases")}

            # New KBs: promote wholesale (never overwrites an existing working copy)
            new_here = src_kbs - existing_kbs
            if new_here:
                print(f"  From {label}: {len(new_here)} new KB(s) — promoting: {', '.join(sorted(new_here))}")
                totals = promote_from_db(src_path, conn, only=new_here, dry_run=dry_run)
                if not dry_run:
                    conn.commit()
                    existing_kbs |= new_here
                promoted_total += len(totals)

            # Re-runs of known KBs: stage for reconciliation if the metadata actually changed
            candidates = sorted((src_kbs & existing_kbs) - reconciled_this_run)
            for kb_name in candidates:
                src_row = src.execute(
                    "SELECT croissant_metadata FROM knowledge_bases WHERE name = ?", (kb_name,)
                ).fetchone()
                working_row = conn.execute(
                    "SELECT croissant_metadata FROM knowledge_bases WHERE name = ?", (kb_name,)
                ).fetchone()
                if not src_row or not working_row:
                    continue
                try:
                    changed = json.loads(src_row[0], strict=False) != json.loads(working_row[0], strict=False)
                except Exception:
                    changed = src_row[0] != working_row[0]
                if not changed:
                    continue

                reconciled_this_run.add(kb_name)
                if dry_run:
                    print(f"  {kb_name}: changed in {label} — would stage for reconciliation")
                    staged_total += 1
                    continue
                try:
                    issue_count = stage_kb(kb_name, src_path, conn, label=label)
                except RuntimeError as e:
                    print(f"  {kb_name}: SKIP staging — {e}")
                    continue
                conn.commit()
                staged_total += 1
                print(f"  {kb_name}: changed in {label} — staged for reconciliation "
                      f"({issue_count} structural issue(s) in staged version)")

            src.close()

        if promoted_total == 0:
            print("  No new KBs to promote.")
        if staged_total == 0:
            print("  No changed KBs to reconcile.")
        elif not dry_run:
            print(f"  → {staged_total} KB(s) staged. Review each at /reconcile/<name> "
                  f"or via the Reconcile button on the home page.")
        print()

    # ── 4. Backfill gaps in working KB links ──────────────────────────────────
    print("── Step 3: Backfill any missing leaf paths ────────────────────────")
    conn.row_factory = sqlite3.Row
    rows = conn.execute(
        "SELECT name, croissant_metadata FROM knowledge_bases ORDER BY name"
    ).fetchall()
    conn.row_factory = None

    grand_backfill = 0
    for row in rows:
        kb_name = row["name"]
        try:
            metadata = json.loads(row["croissant_metadata"], strict=False)
        except Exception:
            continue
        n = backfill_kb_links(conn, kb_name, metadata, dry_run=dry_run)
        if n:
            print(f"  {kb_name}: {'+' if not dry_run else 'would add '}{n} paths")
            grand_backfill += n

    if grand_backfill == 0:
        print("  Nothing to backfill.")
    elif not dry_run:
        conn.commit()
    print()

    conn.close()

    # ── 5. Validate ───────────────────────────────────────────────────────────
    if no_validate:
        print("── Step 4: Validation skipped (--no-validate) ────────────────────")
    else:
        print("── Step 4: Validate all KBs ───────────────────────────────────────")
        print("  (URL checks may take a few minutes for large KBs)\n")
        master_conn = sqlite3.connect(MASTER_DB)
        all_kb_names = [
            row[0] for row in master_conn.execute(
                "SELECT name FROM knowledge_bases ORDER BY name"
            )
        ]
        master_conn.close()
        _run_validation(dry_run=dry_run, kb_names=all_kb_names)

    print()
    if dry_run:
        print("Dry run complete — no changes written. Re-run without --dry-run to apply.")
    else:
        print("Setup complete. Start the dashboard with:  cargo run --release")


if __name__ == "__main__":
    main()
