"""Local project registry. SQLite state is binary and separate from repositories."""
from contextlib import contextmanager
import os
from pathlib import Path
import secrets
import sqlite3
import threading
import time


class Projects:
    def __init__(self, path):
        self.path = Path(path)
        self.path.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
        fd = os.open(self.path, os.O_CREAT | os.O_RDWR, 0o600)
        os.close(fd)
        if os.name != "nt":
            self.path.chmod(0o600)
        self.lock = threading.RLock()
        with self.connect() as db:
            db.execute("CREATE TABLE IF NOT EXISTS projects (id TEXT PRIMARY KEY, path TEXT UNIQUE NOT NULL, opened INTEGER NOT NULL, touched REAL NOT NULL)")

    @contextmanager
    def connect(self):
        db = sqlite3.connect(self.path, timeout=5)
        try:
            with db:
                yield db
        finally:
            db.close()

    def list(self):
        with self.lock, self.connect() as db:
            records = db.execute("SELECT id,path,opened FROM projects ORDER BY touched DESC").fetchall()
        return [{"id": identity, "name": Path(path).name or path, "path": path,
                 "opened": bool(opened), "available": (Path(path) / ".wasm-git").is_dir()}
                for identity, path, opened in records]

    def open(self, path):
        path = str(Path(path).resolve())
        with self.lock, self.connect() as db:
            row = db.execute("SELECT id FROM projects WHERE path=?", (path,)).fetchone()
            identity = row[0] if row else secrets.token_hex(12)
            count = db.execute("SELECT count(*) FROM projects").fetchone()[0]
            if not row and count >= 100:
                raise ValueError("Remove a recent project before opening another (limit 100).")
            db.execute("INSERT INTO projects VALUES (?,?,1,?) ON CONFLICT(path) DO UPDATE SET opened=1,touched=excluded.touched",
                       (identity, path, time.time()))
        return next(p for p in self.list() if p["id"] == identity)

    def get(self, identity, opened=True):
        if not isinstance(identity, str):
            raise ValueError("Choose an open project first.")
        with self.lock, self.connect() as db:
            row = db.execute("SELECT path,opened FROM projects WHERE id=?", (identity,)).fetchone()
        if not row or (opened and not row[1]):
            raise ValueError("Choose an open project first.")
        return Path(row[0])

    def close(self, identity, forget=False):
        self.get(identity, opened=False)
        with self.lock, self.connect() as db:
            if forget:
                db.execute("DELETE FROM projects WHERE id=?", (identity,))
            else:
                db.execute("UPDATE projects SET opened=0 WHERE id=?", (identity,))


def directories(value):
    root = Path(value or Path.home()).expanduser().resolve()
    if not root.is_dir():
        raise ValueError("Choose an existing folder.")
    entries = []
    omitted = False
    with os.scandir(root) as children:
        for entry in children:
            if entry.name.startswith('.') or entry.is_symlink() or not entry.is_dir():
                continue
            if len(entries) >= 300:
                omitted = True
                break
            entries.append({"name": entry.name, "path": str(root / entry.name),
                            "repository": (root / entry.name / '.wasm-git').is_dir()})
    return {"path": str(root), "parent": str(root.parent),
            "repository": (root / '.wasm-git').is_dir(), "limited": omitted,
            "entries": sorted(entries, key=lambda p: p['name'].casefold())}
