"""Discovery of the external binaries (yt-dlp, aria2c)."""
from __future__ import annotations

import os
import shutil
from dataclasses import dataclass
from pathlib import Path

_ENV_OVERRIDES = {
    "yt-dlp": "SNATCH_YT_DLP",
    "aria2c": "SNATCH_ARIA2C",
}

_WINGET_GLOBS = {
    "yt-dlp": [
        r"Microsoft\WinGet\Packages\yt-dlp*\yt-dlp.exe",
        r"Microsoft\WinGet\Links\yt-dlp.exe",
    ],
    "aria2c": [
        r"Microsoft\WinGet\Packages\aria2*\**\aria2c.exe",
        r"Microsoft\WinGet\Links\aria2c.exe",
    ],
}


class ToolNotFound(RuntimeError):
    def __init__(self, name: str):
        env = _ENV_OVERRIDES.get(name)
        hint = f" или задайте путь через переменную окружения {env}" if env else ""
        super().__init__(
            f"Не найден «{name}». Установите его (winget install {name}){hint}."
        )
        self.name = name


def _winget_candidates(name: str) -> list[Path]:
    local = os.environ.get("LOCALAPPDATA")
    if not local:
        return []
    root = Path(local)
    out: list[Path] = []
    for pattern in _WINGET_GLOBS.get(name, []):
        out.extend(sorted(root.glob(pattern)))
    return out


def find(name: str) -> str:
    """Return an absolute path to the executable or raise ToolNotFound."""
    env = _ENV_OVERRIDES.get(name)
    if env:
        candidate = os.environ.get(env)
        if candidate and Path(candidate).expanduser().is_file():
            return str(Path(candidate).expanduser())

    for probe in (name, f"{name}.exe"):
        found = shutil.which(probe)
        if found:
            return found

    hits = _winget_candidates(name)
    if hits:
        return str(hits[-1])

    raise ToolNotFound(name)


def try_find(name: str) -> str | None:
    try:
        return find(name)
    except ToolNotFound:
        return None


@dataclass(frozen=True)
class Toolchain:
    yt_dlp: str | None
    aria2c: str | None

    @classmethod
    def discover(cls) -> "Toolchain":
        return cls(yt_dlp=try_find("yt-dlp"), aria2c=try_find("aria2c"))

    def require(self, name: str) -> str:
        path = getattr(self, name.replace("-", "_"))
        if not path:
            raise ToolNotFound(name)
        return path
