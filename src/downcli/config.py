"""Persistent config and history (last dirs, last links)."""
from __future__ import annotations

import json
import os
from dataclasses import asdict, dataclass, field
from pathlib import Path

MAX_HISTORY = 15


def config_dir() -> Path:
    base = os.environ.get("LOCALAPPDATA") or os.environ.get("XDG_CONFIG_HOME")
    if base:
        return Path(base) / "snatch"
    return Path.home() / ".config" / "snatch"


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
        cfg = cls(**{k: v for k, v in data.items() if k in cls.__dataclass_fields__})
        if not cfg.default_dir:
            cfg.default_dir = str(Path.home() / "Downloads")
        return cfg

    def save(self) -> None:
        p = self.path
        p.parent.mkdir(parents=True, exist_ok=True)
        tmp = p.with_suffix(".tmp")
        tmp.write_text(json.dumps(asdict(self), ensure_ascii=False, indent=2), "utf-8")
        tmp.replace(p)

    def remember_url(self, url: str) -> None:
        self.urls = [url, *(u for u in self.urls if u != url)][:MAX_HISTORY]

    def remember_dir(self, directory: str) -> None:
        d = str(Path(directory).expanduser())
        self.last_dir = d
        self.dirs = [d, *(x for x in self.dirs if x != d)][:MAX_HISTORY]
