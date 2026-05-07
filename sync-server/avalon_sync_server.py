#!/usr/bin/env python3
import argparse
import json
import os
import sqlite3
import threading
from datetime import datetime, timezone
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.parse import urlparse


SCHEMA = """
CREATE TABLE IF NOT EXISTS edges (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    from_normalized TEXT NOT NULL,
    to_normalized TEXT NOT NULL,
    from_name TEXT NOT NULL,
    to_name TEXT NOT NULL,
    first_seen_at TEXT NOT NULL,
    last_seen_at TEXT NOT NULL,
    ttl_seconds INTEGER,
    expires_at TEXT,
    observations_count INTEGER NOT NULL DEFAULT 1,
    source TEXT NOT NULL DEFAULT 'client',
    UNIQUE(from_normalized, to_normalized)
);

CREATE INDEX IF NOT EXISTS idx_edges_last_seen_at ON edges(last_seen_at);
CREATE INDEX IF NOT EXISTS idx_edges_expires_at ON edges(expires_at);
"""


def now_iso() -> str:
    return datetime.now(timezone.utc).isoformat().replace("+00:00", "Z")


def normalize_name(value: str) -> str:
    return " ".join(value.strip().lower().split())


class EdgeStore:
    def __init__(self, db_path: Path):
        self.db_path = db_path
        self.lock = threading.Lock()
        self.db_path.parent.mkdir(parents=True, exist_ok=True)
        with self.connect() as conn:
            conn.executescript(SCHEMA)

    def connect(self) -> sqlite3.Connection:
        conn = sqlite3.connect(self.db_path)
        conn.row_factory = sqlite3.Row
        return conn

    def snapshot(self) -> list[dict]:
        with self.lock, self.connect() as conn:
            rows = conn.execute(
                """
                SELECT from_name, to_name, from_normalized, to_normalized,
                       first_seen_at, last_seen_at, ttl_seconds, expires_at,
                       observations_count, source
                FROM edges
                WHERE expires_at IS NULL OR datetime(expires_at) > datetime('now')
                ORDER BY last_seen_at DESC
                """
            ).fetchall()
            return [dict(row) for row in rows]

    def upsert_edge(self, payload: dict) -> dict:
        from_name = str(payload.get("from_name") or payload.get("from_location_name") or "").strip()
        to_name = str(payload.get("to_name") or payload.get("to_location_name") or "").strip()
        if not from_name or not to_name:
            raise ValueError("from_name and to_name are required")

        from_normalized = normalize_name(payload.get("from_normalized") or from_name)
        to_normalized = normalize_name(payload.get("to_normalized") or to_name)
        if from_normalized > to_normalized:
            from_normalized, to_normalized = to_normalized, from_normalized
            from_name, to_name = to_name, from_name

        first_seen_at = payload.get("first_seen_at") or now_iso()
        last_seen_at = payload.get("last_seen_at") or now_iso()
        ttl_seconds = payload.get("ttl_seconds")
        expires_at = payload.get("expires_at")
        observations_count = int(payload.get("observations_count") or 1)
        source = str(payload.get("source") or "client")[:64]

        with self.lock, self.connect() as conn:
            existing = conn.execute(
                """
                SELECT id, observations_count FROM edges
                WHERE from_normalized = ?1 AND to_normalized = ?2
                """,
                (from_normalized, to_normalized),
            ).fetchone()
            if existing:
                conn.execute(
                    """
                    UPDATE edges
                    SET from_name = ?1,
                        to_name = ?2,
                        last_seen_at = CASE
                            WHEN datetime(?3) > datetime(last_seen_at) THEN ?3
                            ELSE last_seen_at
                        END,
                        ttl_seconds = COALESCE(?4, ttl_seconds),
                        expires_at = COALESCE(?5, expires_at),
                        observations_count = MAX(observations_count, ?6),
                        source = ?7
                    WHERE id = ?8
                    """,
                    (
                        from_name,
                        to_name,
                        last_seen_at,
                        ttl_seconds,
                        expires_at,
                        observations_count,
                        source,
                        existing["id"],
                    ),
                )
            else:
                conn.execute(
                    """
                    INSERT INTO edges (
                        from_normalized, to_normalized, from_name, to_name,
                        first_seen_at, last_seen_at, ttl_seconds, expires_at,
                        observations_count, source
                    )
                    VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
                    """,
                    (
                        from_normalized,
                        to_normalized,
                        from_name,
                        to_name,
                        first_seen_at,
                        last_seen_at,
                        ttl_seconds,
                        expires_at,
                        observations_count,
                        source,
                    ),
                )
            conn.commit()

        return {
            "from_name": from_name,
            "to_name": to_name,
            "from_normalized": from_normalized,
            "to_normalized": to_normalized,
            "first_seen_at": first_seen_at,
            "last_seen_at": last_seen_at,
            "ttl_seconds": ttl_seconds,
            "expires_at": expires_at,
            "observations_count": observations_count,
            "source": source,
        }


class Handler(BaseHTTPRequestHandler):
    store: EdgeStore
    write_token: str

    def log_message(self, fmt: str, *args):
        print(f"{self.address_string()} - {fmt % args}")

    def do_OPTIONS(self):
        self.send_response(204)
        self.send_headers()
        self.end_headers()

    def do_GET(self):
        path = urlparse(self.path).path.rstrip("/")
        if path in ("", "/health"):
            self.write_json({"ok": True, "service": "avalon-sync", "time": now_iso()})
            return
        if path.endswith("/snapshot"):
            self.write_json({"ok": True, "edges": self.store.snapshot()})
            return
        self.write_json({"ok": False, "error": "not found"}, status=404)

    def do_POST(self):
        path = urlparse(self.path).path.rstrip("/")
        if not path.endswith("/edges"):
            self.write_json({"ok": False, "error": "not found"}, status=404)
            return
        if self.write_token and self.headers.get("X-Avalon-Token") != self.write_token:
            self.write_json({"ok": False, "error": "unauthorized"}, status=401)
            return

        length = int(self.headers.get("Content-Length") or "0")
        try:
            payload = json.loads(self.rfile.read(length).decode("utf-8"))
            edge = self.store.upsert_edge(payload)
            self.write_json({"ok": True, "edge": edge})
        except Exception as exc:
            self.write_json({"ok": False, "error": str(exc)}, status=400)

    def send_headers(self):
        self.send_header("Access-Control-Allow-Origin", "*")
        self.send_header("Access-Control-Allow-Methods", "GET, POST, OPTIONS")
        self.send_header("Access-Control-Allow-Headers", "Content-Type, X-Avalon-Token")

    def write_json(self, payload: dict, status: int = 200):
        body = json.dumps(payload, ensure_ascii=False).encode("utf-8")
        self.send_response(status)
        self.send_header("Content-Type", "application/json; charset=utf-8")
        self.send_header("Content-Length", str(len(body)))
        self.send_headers()
        self.end_headers()
        self.wfile.write(body)


def main():
    parser = argparse.ArgumentParser(description="Avalon Mapper public sync server")
    parser.add_argument("--host", default=os.environ.get("AVALON_SYNC_HOST", "127.0.0.1"))
    parser.add_argument("--port", type=int, default=int(os.environ.get("AVALON_SYNC_PORT", "8787")))
    parser.add_argument(
        "--db",
        default=os.environ.get("AVALON_SYNC_DB", "/var/lib/avalon-sync/avalon-sync.sqlite"),
    )
    parser.add_argument("--write-token", default=os.environ.get("AVALON_SYNC_WRITE_TOKEN", ""))
    args = parser.parse_args()

    Handler.store = EdgeStore(Path(args.db))
    Handler.write_token = args.write_token
    server = ThreadingHTTPServer((args.host, args.port), Handler)
    print(f"avalon-sync listening on http://{args.host}:{args.port}")
    server.serve_forever()


if __name__ == "__main__":
    main()
