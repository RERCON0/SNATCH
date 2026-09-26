"""`snatch` command entry point."""
from __future__ import annotations

import argparse
import sys
from pathlib import Path

import questionary

from . import __version__, ui
from .config import Config
from .engines import (
    BuildError,
    Job,
    build,
    detect_engine,
    preflight_warning,
    run,
    validate_url,
)
from .tools import ToolNotFound, Toolchain

BANNER = r"""
 ____    __  __  ______  ______  ____     __  __
/\  _`\ /\ \/\ \/\  _  \/\__  _\/\  _`\  /\ \/\ \
\ \,\L\_\ \ `\\ \ \ \L\ \/_/\ \/\ \ \/\_\\ \ \_\ \
 \/_\__ \\ \ , ` \ \  __ \ \ \ \ \ \ \/_/_\ \  _  \
   /\ \L\ \ \ \`\ \ \ \/\ \ \ \ \ \ \ \L\ \\ \ \ \ \
   \ `\____\ \_\ \_\ \_\ \_\ \ \_\ \ \____/ \ \_\ \_\
    \/_____/\/_/\/_/\/_/\/_/  \/_/  \/___/   \/_/\/_/
                       yt-dlp + aria2c mini-combine
"""


def _force_utf8() -> None:
    for stream in (sys.stdout, sys.stderr):
        try:
            stream.reconfigure(encoding="utf-8", errors="replace")
        except (AttributeError, OSError):
            pass


def _parse_args(argv: list[str] | None) -> argparse.Namespace:
    p = argparse.ArgumentParser(
        prog="snatch",
        description="SNATCH — мини-комбайн для скачивания: yt-dlp + aria2c в одном CLI.",
    )
    p.add_argument("url", nargs="?", help="Ссылка (если не указать — спросит интерактивно)")
    p.add_argument("-o", "--output", help="Папка сохранения (без вопросов)")
    p.add_argument("-e", "--engine", choices=["yt-dlp", "aria2"], help="Движок (по умолчанию — авто-подсказка)")
    p.add_argument("-f", "--format", choices=["best", "1080p", "audio"], help="Формат для yt-dlp")
    p.add_argument("-y", "--yes", action="store_true", help="Пропустить все вопросы (нужны url и output)")
    p.add_argument("--clear-history", action="store_true", help="Забыть последние ссылки и папки")
    p.add_argument("--version", action="version", version=f"snatch {__version__}")
    return p.parse_args(argv)


def _plan_from_args(args: argparse.Namespace) -> dict | None:
    if not args.url or not args.output:
        print("Для режима -y нужны ссылка и --output.", file=sys.stderr)
        return None
    engine = args.engine or detect_engine(args.url)
    fmt = args.format or "best"
    return {"url": args.url, "engine": engine, "fmt": fmt, "out_dir": args.output}


def _download(plan: dict, cfg: Config, tc: Toolchain) -> int:
    try:
        url = validate_url(plan["url"])
    except ValueError as exc:
        print(f"✘ {exc}", file=sys.stderr)
        return 2

    job = Job(engine=plan["engine"], url=url,
              out_dir=Path(plan["out_dir"]), fmt=plan["fmt"])
    warn = preflight_warning(job)
    if warn:
        print(f"⚠ {warn}", file=sys.stderr)

    try:
        cmd = build(job, tc)
    except (ToolNotFound, BuildError) as exc:
        print(f"✘ {exc}", file=sys.stderr)
        return 2

    code = run(cmd)

    if code == 0:
        cfg.remember_url(url)
        cfg.remember_dir(plan["out_dir"])
        cfg.save()
        print(f"✔ Готово: {plan['out_dir']}")
    elif code == 130:
        print("Прервано пользователем (файл можно докачать той же командой).")
    elif code == 127:
        print("✘ Не удалось запустить загрузчик (бинарник пропал или не исполняем).",
              file=sys.stderr)
    else:
        print(f"✘ Ошибка (код {code}).", file=sys.stderr)
    return code


def main(argv: list[str] | None = None) -> int:
    _force_utf8()
    args = _parse_args(argv)
    cfg = Config.load()

    if args.clear_history:
        cfg.urls, cfg.dirs, cfg.last_dir = [], [], ""
        cfg.save()
        print("История очищена.")
        return 0

    if args.yes:
        plan = _plan_from_args(args)
        return _download(plan, cfg, Toolchain.discover()) if plan else 2

    tc = Toolchain.discover()
    if not (tc.yt_dlp or tc.aria2c):
        print("✘ Не найдено ни одного инструмента: поставь yt-dlp и/или aria2c "
              "(например, через winget).", file=sys.stderr)
        return 2

    print(BANNER)
    if not (sys.stdin.isatty() and sys.stdout.isatty()):
        print("✘ Интерактивный режим требует настоящий терминал. "
              "Используй Windows Terminal/cmd или режим -y со ссылкой и -o.",
              file=sys.stderr)
        return 2
    try:
        while True:
            plan = ui.collect(cfg, link=args.url)
            args.url = None
            if plan is None:
                print("Отменено.")
                return 130
            code = _download(plan, cfg, tc)
            if not questionary.confirm("\nСкачать ещё что-нибудь?", default=False).ask():
                return code
    except (KeyboardInterrupt, EOFError):
        print("\nОтменено.")
        return 130


if __name__ == "__main__":
    raise SystemExit(main())
