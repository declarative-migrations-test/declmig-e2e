#!/usr/bin/env python3
"""Takoda Python worker implementing the shared supervisor JSONL ABI."""

from __future__ import annotations

import json
import os
import sys
import time
import urllib.error
import urllib.parse
import urllib.request
from typing import Any

ELEMENT_KEY = "element-6066-11e4-a52e-4f735466cecf"
MAX_WEBDRIVER_RESPONSE_BYTES = 4 * 1024 * 1024


def validate_selenium_upstream_url(raw: str) -> str:
    try:
        parsed = urllib.parse.urlsplit(raw)
        port = parsed.port
    except ValueError as error:
        raise RuntimeError(
            "TKDA_SELENIUM_UPSTREAM_URL must be a valid loopback HTTP URL"
        ) from error

    if (
        parsed.scheme != "http"
        or parsed.hostname not in {"127.0.0.1", "::1"}
        or port is None
        or parsed.username is not None
        or parsed.password is not None
        or parsed.path not in {"", "/"}
        or parsed.query
        or parsed.fragment
    ):
        raise RuntimeError(
            "TKDA_SELENIUM_UPSTREAM_URL must be credential-free root HTTP "
            "on literal loopback with an explicit port"
        )
    return raw.rstrip("/")


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(
        self,
        req: urllib.request.Request,
        fp: Any,
        code: int,
        msg: str,
        headers: Any,
        newurl: str,
    ) -> None:
        return None


UPSTREAM_URL = validate_selenium_upstream_url(
    os.environ.get("TKDA_SELENIUM_UPSTREAM_URL", "http://127.0.0.1:9515")
)
BROWSER_NAME = os.environ.get("TKDA_SELENIUM_BROWSER", "chrome")
URL_OPENER = urllib.request.build_opener(NoRedirect())

session_id: str | None = None
browser_engine = "selenium"
lease_epoch: int | None = None


def emit(event: dict[str, Any]) -> None:
    sys.stdout.write(json.dumps(event, separators=(",", ":")) + "\n")
    sys.stdout.flush()


def emit_event(event: dict[str, Any]) -> None:
    if not isinstance(lease_epoch, int) or lease_epoch <= 0:
        return
    payload = dict(event)
    payload["lease_epoch"] = lease_epoch
    emit(payload)


def webdriver_request(method: str, path: str, body: Any | None = None) -> Any:
    payload = None
    headers = {"accept": "application/json"}
    if body is not None:
        payload = json.dumps(body, separators=(",", ":")).encode("utf-8")
        headers["content-type"] = "application/json"

    request = urllib.request.Request(
        f"{UPSTREAM_URL}{path}",
        data=payload,
        headers=headers,
        method=method,
    )

    try:
        with URL_OPENER.open(request, timeout=30) as response:
            payload = response.read(MAX_WEBDRIVER_RESPONSE_BYTES + 1)
            if len(payload) > MAX_WEBDRIVER_RESPONSE_BYTES:
                raise RuntimeError(
                    f"WebDriver upstream response exceeded {MAX_WEBDRIVER_RESPONSE_BYTES} bytes"
                )
            decoded = json.loads(payload.decode("utf-8") or "{}")
    except urllib.error.HTTPError as error:
        detail_bytes = error.read(MAX_WEBDRIVER_RESPONSE_BYTES + 1)
        if len(detail_bytes) > MAX_WEBDRIVER_RESPONSE_BYTES:
            raise RuntimeError(
                f"WebDriver upstream response exceeded {MAX_WEBDRIVER_RESPONSE_BYTES} bytes"
            ) from error
        detail = detail_bytes.decode("utf-8", errors="replace")[:20_000]
        raise RuntimeError(f"WebDriver upstream returned HTTP {error.code}: {detail}") from error
    except (urllib.error.URLError, TimeoutError) as error:
        raise RuntimeError(f"WebDriver upstream unavailable at {UPSTREAM_URL}: {error}") from error

    value = decoded.get("value") if isinstance(decoded, dict) else None
    if isinstance(value, dict) and value.get("error"):
        raise RuntimeError(f"WebDriver upstream error {value.get('error')}: {value.get('message', '')}")
    return value


def ensure_session() -> str:
    global session_id
    if session_id:
        return session_id

    value = webdriver_request(
        "POST",
        "/session",
        {
            "capabilities": {
                "alwaysMatch": {"browserName": BROWSER_NAME},
                "firstMatch": [{}],
            }
        },
    )
    if not isinstance(value, dict):
        raise RuntimeError("WebDriver upstream session response did not contain an object")

    candidate = value.get("sessionId")
    if not isinstance(candidate, str) or not candidate:
        raise RuntimeError("WebDriver upstream session response omitted sessionId")

    session_id = candidate
    return candidate


def find_element(selector: str) -> str:
    current = ensure_session()
    value = webdriver_request(
        "POST",
        f"/session/{current}/element",
        {"using": "css selector", "value": selector},
    )
    if not isinstance(value, dict):
        raise RuntimeError("WebDriver element response did not contain an object")

    element_id = value.get(ELEMENT_KEY)
    if not isinstance(element_id, str) or not element_id:
        raise RuntimeError("WebDriver element response omitted the W3C element id")
    return element_id


def execute_action(action: dict[str, Any]) -> Any:
    if browser_engine != "selenium":
        raise NotImplementedError(
            f"Python worker currently supports browser_engine=selenium, not {browser_engine}"
        )

    operation = action.get("op")
    current = ensure_session()

    if operation == "navigate":
        url = action.get("url")
        if not isinstance(url, str) or not url:
            raise ValueError("navigate requires url")
        webdriver_request("POST", f"/session/{current}/url", {"url": url})
        return {"url": webdriver_request("GET", f"/session/{current}/url")}

    if operation in {"url", "current_url"}:
        return {"url": webdriver_request("GET", f"/session/{current}/url")}

    if operation == "title":
        return {"title": webdriver_request("GET", f"/session/{current}/title")}

    if operation in {"click", "fill", "text"}:
        selector = action.get("selector") or "body"
        if not isinstance(selector, str) or not selector:
            raise ValueError(f"{operation} requires selector")
        element_id = find_element(selector)
        element_path = f"/session/{current}/element/{element_id}"

        if operation == "click":
            webdriver_request("POST", f"{element_path}/click", {})
            return {"ok": True}
        if operation == "fill":
            value = str(action.get("value") or "")
            webdriver_request(
                "POST",
                f"{element_path}/value",
                {"text": value, "value": list(value)},
            )
            return {"ok": True}

        text = webdriver_request("GET", f"{element_path}/text")
        return {"text": str(text or "")[:200_000]}

    if operation == "screenshot":
        screenshot = webdriver_request("GET", f"/session/{current}/screenshot")
        if not isinstance(screenshot, str):
            raise RuntimeError("WebDriver screenshot response was not base64 text")
        return {"screenshot_base64": screenshot, "omitted": False}

    if operation == "sleep":
        milliseconds = int(action.get("milliseconds") or 250)
        time.sleep(max(0, min(milliseconds, 30_000)) / 1000)
        return {"slept_ms": milliseconds}

    raise NotImplementedError(f"unsupported Python Selenium action: {operation!r}")


def close_session() -> None:
    global session_id
    current = session_id
    session_id = None
    if not current:
        return
    try:
        webdriver_request("DELETE", f"/session/{current}")
    except RuntimeError as error:
        if isinstance(lease_epoch, int) and lease_epoch > 0:
            emit_event({"type": "log", "level": "warn", "message": str(error)[:20_000]})
        else:
            print(f"failed to close WebDriver session before start: {error}", file=sys.stderr)


def handle(command: dict[str, Any]) -> bool:
    global browser_engine, lease_epoch
    command_type = command.get("type")

    if command_type == "start":
        requested_epoch = command.get("lease_epoch")
        if (
            not isinstance(requested_epoch, int)
            or isinstance(requested_epoch, bool)
            or requested_epoch <= 0
        ):
            print("start command requires positive lease_epoch", file=sys.stderr)
            return False
        lease_epoch = requested_epoch
        requested_engine = command.get("browser_engine")
        if isinstance(requested_engine, str):
            browser_engine = requested_engine
        emit_event({"type": "ready", "transport": "stdio"})
        emit_event(
            {
                "type": "log",
                "level": "info",
                "message": f"python worker ready for run {command.get('run_id')} engine={browser_engine}",
            }
        )
        return True

    if not isinstance(lease_epoch, int) or lease_epoch <= 0:
        print(
            f"refusing {command_type!r} command before a positive lease_epoch is established",
            file=sys.stderr,
        )
        return True

    if command_type == "driver":
        try:
            body = command.get("body")
            if not isinstance(body, dict):
                raise ValueError("driver body must be an action object")
            result = execute_action(body)
            emit_event(
                {
                    "type": "driver_response",
                    "request_id": command.get("request_id"),
                    "status": 200,
                    "body": result,
                }
            )
        except (RuntimeError, ValueError, NotImplementedError) as error:
            emit_event(
                {
                    "type": "driver_response",
                    "request_id": command.get("request_id"),
                    "status": 502 if isinstance(error, RuntimeError) else 501,
                    "body": {"error": str(error)[:20_000]},
                }
            )
        return True

    if command_type == "replan":
        emit_event(
            {
                "type": "needs_replan",
                "reason": command.get("reason", "replan requested"),
            }
        )
        return True

    if command_type == "cancel":
        close_session()
        emit_event(
            {
                "type": "log",
                "level": "info",
                "message": f"cancelled: {command.get('reason', 'requested')}",
            }
        )
        return False

    emit_event(
        {
            "type": "failed",
            "retryable": False,
            "error": f"unknown command type: {command_type!r}",
        }
    )
    return True


def main() -> int:
    print(
        f"python adapter boot pid={os.getpid()} upstream={UPSTREAM_URL}",
        file=sys.stderr,
    )

    last_heartbeat = time.monotonic()
    try:
        for line in sys.stdin:
            line = line.strip()
            if not line:
                continue

            try:
                command = json.loads(line)
            except json.JSONDecodeError as error:
                if isinstance(lease_epoch, int) and lease_epoch > 0:
                    emit_event({"type": "failed", "retryable": False, "error": str(error)})
                else:
                    print(f"invalid worker command before start: {error}", file=sys.stderr)
                continue

            if not handle(command):
                return 0

            now = time.monotonic()
            if now - last_heartbeat >= 15:
                emit_event({"type": "heartbeat"})
                last_heartbeat = now
    finally:
        close_session()

    return 0


if __name__ == "__main__":
    raise SystemExit(main())
