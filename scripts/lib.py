"""
Shared utilities for all robo-croissant-dashboard scripts.

Imported by import_run.py, promote_kbs.py, setup.py, validate_all.py, etc.
"""

import datetime
import hashlib
import json
import os
import re
import sqlite3

# ── Leaf extraction ───────────────────────────────────────────────────────────

def extract_leaves(obj, prefix="") -> list[tuple[str, str]]:
    """
    Walk a JSON object and return (path, value) for every leaf node.
    Uses dot notation for keys and [n] for array indices.
    Top-level @context is skipped — it's JSON-LD schema, not reviewable content.
    Primitive-only lists are stored as a JSON array string.
    """
    results = []
    if isinstance(obj, dict):
        for k, v in obj.items():
            if k == "@context" and not prefix:
                continue
            path = f"{prefix}.{k}" if prefix else k
            if isinstance(v, dict):
                results.extend(extract_leaves(v, path))
            elif isinstance(v, list):
                if all(not isinstance(item, (dict, list)) for item in v):
                    results.append((path, json.dumps(v)))
                else:
                    results.extend(extract_leaves(v, path))
            else:
                results.append((path, "" if v is None else str(v)))
    elif isinstance(obj, list):
        for i, item in enumerate(obj):
            path = f"{prefix}[{i}]"
            if isinstance(item, (dict, list)):
                results.extend(extract_leaves(item, path))
            else:
                results.append((path, "" if item is None else str(item)))
    return results


# ── Directory name parsing ────────────────────────────────────────────────────

_DATE_RE = re.compile(r"^(\d{4})_(\d{2})(\d{2})(?:_(.+))?$")


def parse_dir_name(name: str) -> tuple[str | None, str | None]:
    """Return (date_str YYYY-MM-DD, model_str) from a directory name."""
    m = _DATE_RE.match(name)
    if m:
        year, month, day, rest = m.groups()
        date_str = f"{year}-{month}-{day}"
        model_str = rest.replace("_", " ").strip() if rest else None
        return date_str, model_str
    return None, name.strip() or None


# ── KB name normalisation ─────────────────────────────────────────────────────

def _norm(s: str) -> str:
    return re.sub(r"[^a-z0-9]", "", s.lower())


def load_kb_name_map(conn: sqlite3.Connection) -> dict[str, str]:
    """Return {normalised_name: canonical_name} for every KB in the master DB."""
    rows = conn.execute("SELECT name FROM knowledge_bases").fetchall()
    return {_norm(name): name for (name,) in rows}


def resolve_kb_name(candidates: list[str], kb_map: dict[str, str]) -> str:
    """Pick the canonical KB name from a ranked list of candidates."""
    for c in candidates:
        hit = kb_map.get(_norm(c))
        if hit:
            return hit
    return next((c for c in candidates if c), candidates[0])


# ── Hashing ───────────────────────────────────────────────────────────────────

def hash_file(path: str) -> str:
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(65536), b""):
            h.update(chunk)
    return h.hexdigest()


def hash_json_dir(dir_path: str) -> str:
    """Deterministic content hash over all JSON files in a directory."""
    h = hashlib.sha256()
    for fname in sorted(os.listdir(dir_path)):
        if not fname.endswith(".json"):
            continue
        h.update(fname.encode())
        h.update(open(os.path.join(dir_path, fname), "rb").read())
    return h.hexdigest()


# ── Schema bootstrap ──────────────────────────────────────────────────────────

def ensure_run_tables(conn: sqlite3.Connection):
    conn.executescript("""
        CREATE TABLE IF NOT EXISTS runs (
            id          INTEGER PRIMARY KEY AUTOINCREMENT,
            label       TEXT NOT NULL,
            run_date    TEXT,
            model       TEXT,
            file_hash   TEXT UNIQUE NOT NULL,
            imported_at TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS run_links (
            run_id  INTEGER NOT NULL,
            kb_name TEXT NOT NULL,
            path    TEXT NOT NULL,
            value   TEXT NOT NULL,
            PRIMARY KEY (run_id, kb_name, path)
        );
        CREATE INDEX IF NOT EXISTS idx_run_links_kb_path
            ON run_links(kb_name, path);
    """)


# ── Link insertion helpers ────────────────────────────────────────────────────

def insert_run_links(
    cur: sqlite3.Cursor,
    run_id: int,
    kb_name: str,
    links: list[tuple[str, str]],
) -> int:
    if not links:
        return 0
    cur.executemany(
        "INSERT OR IGNORE INTO run_links (run_id, kb_name, path, value) VALUES (?,?,?,?)",
        [(run_id, kb_name, path, value) for path, value in links],
    )
    return len(links)


def backfill_kb_links(
    conn: sqlite3.Connection,
    kb_name: str,
    metadata: dict,
    dry_run: bool = False,
) -> int:
    """Add any leaf paths from metadata that are missing from kb_links. Returns count added."""
    all_leaves = extract_leaves(metadata)
    cur = conn.cursor()
    cur.execute("SELECT path FROM kb_links WHERE kb_name = ?", (kb_name,))
    existing = {r[0] for r in cur.fetchall()}
    missing = [(path, val) for path, val in all_leaves if path not in existing]
    if missing and not dry_run:
        cur.executemany(
            "INSERT INTO kb_links (kb_name, path, value, url, confidence, reviewed) "
            "VALUES (?, ?, ?, '', 0.0, 0)",
            [(kb_name, path, val) for path, val in missing],
        )
    return len(missing)


# ── Run discovery ─────────────────────────────────────────────────────────────

def discover_runs(root: str) -> list[dict]:
    """
    Scan a directory tree for pipeline runs and return them sorted by date.
    A run is either:
      - A subdirectory containing robo_croissant.db
      - A subdirectory containing .json files (but not ONLY non-dataset files)
      - A .db file directly
    Each returned dict has keys: path, dir_name, run_date, model, is_db
    """
    runs = []

    def probe(path: str, dir_name: str):
        run_date, model = parse_dir_name(dir_name)
        if os.path.isfile(path) and path.endswith(".db"):
            runs.append(dict(path=path, dir_name=dir_name, run_date=run_date, model=model, is_db=True))
        elif os.path.isdir(path):
            db_path = os.path.join(path, "robo_croissant.db")
            if os.path.isfile(db_path):
                runs.append(dict(path=db_path, dir_name=dir_name, run_date=run_date, model=model, is_db=True))
            elif any(f.endswith(".json") for f in os.listdir(path)):
                runs.append(dict(path=path, dir_name=dir_name, run_date=run_date, model=model, is_db=False))

    if os.path.isfile(root) and root.endswith(".db"):
        probe(root, os.path.basename(os.path.dirname(root)))
    elif os.path.isdir(root):
        # Check if root itself is a run
        db_path = os.path.join(root, "robo_croissant.db")
        if os.path.isfile(db_path):
            probe(db_path, os.path.basename(root))
        else:
            # Scan immediate subdirectories
            for entry in os.listdir(root):
                probe(os.path.join(root, entry), entry)

    # Sort: dated runs chronologically first, then undated by name
    runs.sort(key=lambda r: (r["run_date"] or "9999", r["dir_name"]))
    return runs
