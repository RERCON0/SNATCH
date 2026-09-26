"""`snatch` command entry point."""
from __future__ import annotations

import argparse
import contextlib
import sys
from pathlib import Path

import questionary

from . import __version__, ui
from .config import Config
from .engines import (
    COOKIES_BROWSERS,
    BuildError,
    Job,
    build,
    detect_engine,
    preflight_warning,
    run,
    validate_url,
)
from .tools import Toolchain, ToolNotFound

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
        with contextlib.suppress(AttributeError, OSError):
            stream.reconfigure(encoding="utf-8", errors="replace")


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
    p.add_argument("--cookies-from-browser", dest="cookies_browser", metavar="BROWSER",
                   help="Откуда взять куки (chrome, firefox, edge, brave, opera, vivaldi, "
                        "safari, chromium, whale) — для «Sign in to confirm you're not a bot» "
                        "и возрастных ограничений")
    p.add_argument("--clear-history", action="store_true", help="Забыть последние ссылки и папки")
    p.add_argument("--version", action="version", version=f"snatch {__version__}")
    return p.parse_args(argv)


def _plan_from_args(args: argparse.Namespace) -> dict | None:
    if not args.url or not args.output:
        print("Для режима -y нужны ссылка и --output.", file=sys.stderr)
        return None
    engine = args.engine or detect_engine(args.url)
    fmt = args.format or "best"
    return {"url": args.url, "engine": engine, "fmt": fmt, "out_dir": args.output,
            "cookies_browser": args.cookies_browser}


def _download(plan: dict, cfg: Config, tc: Toolchain | None = None) -> int:
    try:
        url = validate_url(plan["url"])
    except ValueError as exc:
        print(f"✘ {exc}", file=sys.stderr)
        return 2

    if tc is None:
        tc = Toolchain.discover()

    job = Job(engine=plan["engine"], url=url,
              out_dir=Path(plan["out_dir"]), fmt=plan["fmt"],
              cookies_browser=plan.get("cookies_browser"))
    for warn in preflight_warning(job):
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


BROWSERS = sorted(COOKIES_BROWSERS)

RETRY_CODE = 1


def _should_offer_cookies(plan: dict, code: int) -> bool:
    return code == RETRY_CODE and plan["engine"] == "yt-dlp"


def _retry_with_cookies(plan: dict, code: int, cfg: Config, tc: Toolchain) -> int:
    while _should_offer_cookies(plan, code):
        had_cookies = bool(plan.get("cookies_browser"))
        retry = questionary.confirm(
            f"\nНе скачалось (код {code}). "
            + ("Попробовать с куками другого браузера?" if had_cookies
               else "Сайту вроде YouTube часто нужна авторизация: можно взять куки "
                    "из браузера и повторить. Попробовать?"),
            default=not had_cookies,
        ).ask()
        if not retry:
            return code
        browser = questionary.select(
            "Браузер:",
            choices=[questionary.Choice(b, value=b) for b in BROWSERS],
        ).ask()
        if not browser:
            return code
        plan = {**plan, "cookies_browser": browser}
        code = _download(plan, cfg, tc)
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
        return _download(plan, cfg) if plan else 2

    tc = Toolchain.discover()
    if not (tc.yt_dlp or tc.aria2c):
        print("✘ Не найдено ни одного инструмента: поставь yt-dlp и/или aria2c "
              "(например, через winget).", file=sys.stderr)
        return 2

    if not (sys.stdin.isatty() and sys.stdout.isatty()):
        print("✘ Интерактивный режим требует настоящий терминал. "
              "Используй Windows Terminal/cmd или режим -y со ссылкой и -o.",
              file=sys.stderr)
        return 2
    print(BANNER)
    try:
        while True:
            plan = ui.collect(cfg, tc, link=args.url, cookies_browser=args.cookies_browser)
            args.url = None
            if plan is None:
                print("Отменено.")
                return 130
            code = _download(plan, cfg, tc)
            code = _retry_with_cookies(plan, code, cfg, tc)
            if not questionary.confirm("\nСкачать ещё что-нибудь?", default=False).ask():
                return code
    except (KeyboardInterrupt, EOFError):
        print("\nОтменено.")
        return 130


if __name__ == "__main__":
    raise SystemExit(main())
