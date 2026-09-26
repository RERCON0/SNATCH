"""Interactive terminal flow: pick link, engine, format and folder."""
from __future__ import annotations

from pathlib import Path

import questionary

from .config import Config
from .engines import (
    ENGINE_LABELS,
    FORMATS,
    FORMAT_LABELS,
    detect_engine,
)


def _ask_link(cfg: Config, preset: str | None) -> str | None:
    if preset:
        return preset.strip()

    choices = [questionary.Choice("Вставить новую ссылку", value="__new__")]
    for url in cfg.urls[:6]:
        choices.append(questionary.Choice(_clip(url), value=url))

    pick = questionary.select("Ссылка:", choices=choices).ask()
    if pick is None:
        return None
    if pick != "__new__":
        return pick
    return questionary.text("Ссылка:").ask()


def _ask_engine(cfg: Config, url: str) -> str | None:
    guess = detect_engine(url)
    other = "aria2" if guess == "yt-dlp" else "yt-dlp"
    choices = [
        questionary.Choice(f"{ENGINE_LABELS[guess]}  (рекомендуется)", value=guess),
        questionary.Choice(ENGINE_LABELS[other], value=other),
    ]
    return questionary.select("Чем скачивать:", choices=choices).ask()


def _ask_format(url: str) -> str | None:
    choices = [questionary.Choice(FORMAT_LABELS[key], value=key) for key in FORMATS]
    return questionary.select("Формат (yt-dlp):", choices=choices).ask()


def _browse_dir(start: Path) -> str | None:
    """Minimal interactive directory browser (name -> descend, .. -> up)."""
    current = start if start.exists() else Path.home()
    while True:
        entries = [
            p for p in _safe_iterdir(current)
            if p.is_dir() and not p.name.startswith(".")
        ]
        choices = [
            questionary.Choice("✓ Выбрать эту папку", value=str(current)),
            questionary.Choice("↑ Наверх", value=".."),
        ]
        for d in entries:
            choices.append(questionary.Choice(f"📁 {d.name}", value=f"down::{d}"))
        ans = questionary.select(f"Папка: {current}", choices=choices).ask()
        if ans is None:
            return None
        if ans == "..":
            if current.parent != current:
                current = current.parent
            continue
        if ans.startswith("down::"):
            current = Path(ans[len("down::"):])
            continue
        return ans


def _ask_dir(cfg: Config) -> str | None:
    quick = [
        questionary.Choice("Загрузки", value=str(Path.home() / "Downloads")),
        questionary.Choice("Рабочий стол", value=str(Path.home() / "Desktop")),
        questionary.Choice("Текущая папка", value=str(Path.cwd())),
    ]
    if cfg.last_dir and Path(cfg.last_dir).exists():
        quick.insert(0, questionary.Choice(f"Последняя: {_clip(cfg.last_dir)}", value=cfg.last_dir))
    quick.append(questionary.Choice("Выбрать в проводнике…", value="__browse__"))

    pick = questionary.select("Куда сохранять:", choices=quick).ask()
    if pick is None:
        return None
    if pick != "__browse__":
        return pick
    return _browse_dir(Path(cfg.default_dir))


def _confirm(cfg: Config, engine: str, fmt: str, out_dir: str, url: str) -> bool:
    line = _clip(url, 60)
    msg = f" yt-dlp · {FORMAT_LABELS.get(fmt, fmt)} · → {_clip(out_dir, 40)}" if engine == "yt-dlp" \
        else f" aria2c → {_clip(out_dir, 40)}"
    return questionary.confirm(f"{msg}\n  {line}\nСкачать?").ask() or False


def _clip(text: str, width: int = 70) -> str:
    text = text.strip()
    return text if len(text) <= width else text[: width - 1] + "…"


def _safe_iterdir(path: Path):
    try:
        return sorted(path.iterdir(), key=lambda p: p.name.lower())
    except (PermissionError, OSError):
        return []


def collect(cfg: Config, link: str | None = None) -> dict | None:
    """Run the prompts and return a plan dict, or None if the user aborts."""
    url = _ask_link(cfg, link)
    if not url:
        return None
    engine = _ask_engine(cfg, url)
    if not engine:
        return None
    fmt = "best"
    if engine == "yt-dlp":
        fmt = _ask_format(url)
        if fmt is None:
            return None
    out_dir = _ask_dir(cfg)
    if not out_dir:
        return None
    if not _confirm(cfg, engine, fmt, out_dir, url):
        return None
    return {"url": url, "engine": engine, "fmt": fmt, "out_dir": out_dir}
