#!/usr/bin/env python3
"""Minimal HTTP gateway for Quazar external agents (stub).

Endpoints:
  GET  /health
  GET  /v1/agents
  POST /v1/svyaznoy/cmd   — stub for Алгонизм-связной
  POST /v1/cursor/exec    — stub for Cursor worker

Real OAuth / Cursor login is out of scope for this placeholder;
compose must start and answer health checks.
"""
from __future__ import annotations

import json
import os
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from typing import Any
from urllib.parse import urlparse

PORT = int(os.environ.get("EXTERNAL_AGENTS_PORT", "8090"))
HOST = os.environ.get("EXTERNAL_AGENTS_HOST", "0.0.0.0")
AGENT_NAME = os.environ.get("EXTERNAL_AGENTS_NAME", "external-agents")


def _json_bytes(payload: dict[str, Any], status: int = 200) -> tuple[int, bytes, str]:
    body = json.dumps(payload, ensure_ascii=False).encode("utf-8")
    return status, body, "application/json; charset=utf-8"


class Handler(BaseHTTPRequestHandler):
    server_version = "QuazarExternalAgentsStub/0.1"

    def log_message(self, fmt: str, *args: Any) -> None:
        # Keep container logs short and structured.
        print(f"{self.log_date_time_string()} {self.address_string()} {fmt % args}", flush=True)

    def _read_json(self) -> dict[str, Any]:
        length = int(self.headers.get("Content-Length") or "0")
        if length <= 0:
            return {}
        raw = self.rfile.read(length)
        if not raw:
            return {}
        try:
            data = json.loads(raw.decode("utf-8"))
        except json.JSONDecodeError:
            return {"_parse_error": True}
        return data if isinstance(data, dict) else {"_value": data}

    def _send(self, status: int, body: bytes, content_type: str) -> None:
        self.send_response(status)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Cache-Control", "no-store")
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self) -> None:  # noqa: N802
        path = urlparse(self.path).path.rstrip("/") or "/"
        if path in ("/health", "/"):
            status, body, ctype = _json_bytes(
                {
                    "ok": True,
                    "service": AGENT_NAME,
                    "role": "external-agents",
                    "stub": True,
                    "agents": ["svyaznoy", "cursor-worker"],
                }
            )
            self._send(status, body, ctype)
            return
        if path == "/v1/agents":
            status, body, ctype = _json_bytes(
                {
                    "ok": True,
                    "agents": [
                        {
                            "id": "svyaznoy",
                            "url_path": "/v1/svyaznoy",
                            "env": "SVYAZNOY_URL",
                            "keys": "mounted in external container only",
                        },
                        {
                            "id": "cursor-worker",
                            "url_path": "/v1/cursor",
                            "env": "CURSOR_AGENT_URL",
                            "keys": "Cursor auth stays outside core",
                        },
                    ],
                }
            )
            self._send(status, body, ctype)
            return
        status, body, ctype = _json_bytes({"ok": False, "error": "not found"}, 404)
        self._send(status, body, ctype)

    def do_POST(self) -> None:  # noqa: N802
        path = urlparse(self.path).path.rstrip("/") or "/"
        payload = self._read_json()
        if payload.get("_parse_error"):
            status, body, ctype = _json_bytes({"ok": False, "error": "invalid json"}, 400)
            self._send(status, body, ctype)
            return

        if path == "/v1/svyaznoy/cmd":
            args = payload.get("args") or []
            status, body, ctype = _json_bytes(
                {
                    "ok": True,
                    "stub": True,
                    "agent": "algonism-svyaznoy",
                    "path": "http-stub",
                    "args": args,
                    "reply": (
                        "Stub Алгонизм-связного: контейнер external-agents жив. "
                        "Реальные ключи и CLI ещё не подключены к этому HTTP API."
                    ),
                    "from": payload.get("from") or "secretary",
                }
            )
            self._send(status, body, ctype)
            return

        if path == "/v1/cursor/exec":
            text = (payload.get("text") or payload.get("prompt") or "").strip()
            status, body, ctype = _json_bytes(
                {
                    "ok": True,
                    "stub": True,
                    "path": "direct",
                    "agent": "cursor-worker",
                    "identifier": "stub-cursor",
                    "reply": (
                        "Stub Cursor worker: задача принята контейнером external-agents. "
                        f"Текст ({len(text)} символов) не исполнен — нужен реальный worker."
                    ),
                    "from": payload.get("from") or "secretary",
                }
            )
            self._send(status, body, ctype)
            return

        status, body, ctype = _json_bytes({"ok": False, "error": "not found"}, 404)
        self._send(status, body, ctype)


def main() -> None:
    httpd = ThreadingHTTPServer((HOST, PORT), Handler)
    print(
        json.dumps(
            {
                "event": "start",
                "service": AGENT_NAME,
                "listen": f"{HOST}:{PORT}",
                "stub": True,
            },
            ensure_ascii=False,
        ),
        flush=True,
    )
    httpd.serve_forever()


if __name__ == "__main__":
    main()
