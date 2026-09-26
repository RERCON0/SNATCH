"""Persistent config and history (last dirs, last links)."""
from __future__ import annotations

import contextlib
import json
import os
import sys
from dataclasses import asdict, dataclass, field
from pathlib import Path

MAX_HISTORY = 15


def config_dir() -> Path:
    base = os.environ.get("LOCALAPPDATA") or os.environ.get("XDG_CONFIG_HOME")
    if base:
        return Path(base) / "snatch"
    return Path.home() / ".config" / "snatch"


def _opt_str(value: object) -> str:
    return value if isinstance(value, str) else ""


def _str_list(value: object) -> list[str]:
    if not isinstance(value, list):
        return []
    return [item for item in value if isinstance(item, str)]


@dataclass
class Config:
    default_dir: str = ""
    last_dir: str = ""
    urls: list[str] = field(default_factory=list)
    dirs: list[str] = field(default_factory=list)

    @property
    def path(self) -> Path:
        return config_dir() / "config.json"

    @classmethod
    def load(cls) -> "Config":
        p = config_dir() / "config.json"
        try:
            data = json.loads(p.read_text("utf-8"))
        except (OSError, ValueError):
            data = {}
        if not isinstance(data, dict):
            data = {}
        cfg = cls(
            default_dir=_opt_str(data.get("default_dir")),
            last_dir=_opt_str(data.get("last_dir")),
            urls=_str_list(data.get("urls")),
            dirs=_str_list(data.get("dirs")),
        )
        if not cfg.default_dir:
            cfg.default_dir = str(Path.home() / "Downloads")
        return cfg

    def save(self) -> None:
        p = self.path
        try:
            p.parent.mkdir(parents=True, exist_ok=True)
            tmp = p.with_suffix(".tmp")
            tmp.write_text(json.dumps(asdict(self), ensure_ascii=False, indent=2), "utf-8")
            if os.name == "posix":
                with contextlib.suppress(OSError):
                    os.chmod(tmp, 0o600)
            tmp.replace(p)
        except OSError as exc:
            print(f"⚠ Не удалось сохранить настройки: {exc}", file=sys.stderr)

    def remember_url(self, url: str) -> None:
        self.urls = [url, *(u for u in self.urls if u != url)][:MAX_HISTORY]

    def remember_dir(self, directory: str) -> None:
        d = str(Path(directory).expanduser())
        self.last_dir = d
        self.dirs = [d, *(x for x in self.dirs if x != d)][:MAX_HISTORY]
