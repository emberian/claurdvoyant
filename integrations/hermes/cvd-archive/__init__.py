"""clustervision archive plugin for Hermes.

Hermes loads directory plugins from ``~/.hermes/plugins/<name>/`` (and project
``./.hermes/plugins/<name>/``). Each plugin has a ``plugin.yaml`` manifest plus an
``__init__.py`` exposing ``register(ctx)``. Hooks are wired with
``ctx.register_hook("<event>", fn)``; the ``on_session_end`` hook is called with
``session_id``, ``completed``, ``interrupted`` keyword args (see the bundled
``disk-cleanup`` plugin in the Hermes source for the canonical pattern).

On session end this plugin:
  1. runs ``cvd sync`` to archive every harness's sessions into ~/.clustervision, and
  2. posts ``"hermes finished (<session_id>)"`` to the clustervision ``fleet`` board.

Set CV_BIN / CVD_BIN env vars if ``cv`` / ``cvd`` are not on PATH.
"""

from __future__ import annotations

import logging
import os
import subprocess
from typing import Any

logger = logging.getLogger(__name__)

_CV = os.environ.get("CV_BIN", "cv")
_CVD = os.environ.get("CVD_BIN", "cvd")


def _on_session_end(
    session_id: str = "",
    completed: bool = True,
    interrupted: bool = False,
    **_: Any,
) -> None:
    """Archive sessions and ping the fleet board. Best-effort; never raises."""
    try:
        subprocess.run(
            [_CVD, "sync"],
            check=False,
            capture_output=True,
            timeout=60,
        )
    except Exception as exc:  # noqa: BLE001 - plugin hooks must not crash the agent
        logger.debug("cvd-archive: cvd sync failed: %s", exc)

    status = "interrupted" if interrupted else ("done" if completed else "ended")
    try:
        subprocess.run(
            [
                _CV,
                "board",
                "post",
                "fleet",
                f"hermes {status} ({session_id or 'session'})",
                "--from",
                "hermes",
                "--kind",
                "status",
            ],
            check=False,
            capture_output=True,
            timeout=30,
        )
    except Exception as exc:  # noqa: BLE001
        logger.debug("cvd-archive: board post failed: %s", exc)


def register(ctx) -> None:
    """Hermes plugin entry point."""
    ctx.register_hook("on_session_end", _on_session_end)
