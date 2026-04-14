"""Helpers for checkleft repo-local exec-v1 checks."""

from __future__ import annotations

import json
import sys
from typing import Any, Iterable, Mapping


def read_request() -> Mapping[str, Any]:
    """Read one exec-v1 request JSON object from stdin."""

    return json.load(sys.stdin)


def write_response(findings: Iterable[Mapping[str, Any]]) -> None:
    """Write an exec-v1 findings response to stdout."""

    json.dump({"findings": list(findings)}, sys.stdout)
    sys.stdout.flush()
