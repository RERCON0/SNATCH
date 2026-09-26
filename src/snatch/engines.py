"""Building and running yt-dlp / aria2c commands."""
from __future__ import annotations

import contextlib
import re
import shutil
import subprocess
from dataclasses import dataclass
from pathlib import Path
from urllib.parse import parse_qsl, urlparse

from .tools import Toolchain

# resume on drops + multi-connection splitting
ARIA2_RESUME = ["-c", "--max-tries=10", "--retry-wait=2", "--auto-file-renaming=false"]
ARIA2_WORKERS = ["-x", "16", "-s", "16", "-k", "1M"]
EXTERNAL_ARIA2_ARGS = f"{' '.join(ARIA2_RESUME)} {' '.join(ARIA2_WORKERS)} --no-conf"

FILE_EXT = {
    ".zip", ".rar", ".7z", ".tar", ".gz", ".bz2", ".xz", ".zst", ".lz4", ".iso",
    ".exe", ".msi", ".msix", ".appx", ".apk", ".deb", ".rpm", ".dmg", ".pkg",
    ".whl", ".jar", ".bin", ".img", ".pdf", ".epub", ".fb2", ".txt", ".csv",
    ".mp4", ".mkv", ".webm", ".avi", ".mov", ".flv", ".ts", ".m4v",
    ".mp3", ".m4a", ".aac", ".flac", ".ogg", ".wav",
    ".jpg", ".jpeg", ".png", ".webp", ".gif",
}

ALLOWED_SCHEMES = {"http", "https", "ftp"}
TORRENT_FILES = {".torrent", ".metalink", ".meta4"}

# BEP-9: "xt" (exact topic, the info-hash) is the one mandatory magnet
# parameter. If a terminal/paste bug mangles the query string (dropped or
# mistranslated "&"), the params glue together and this stops matching.
MAGNET_XT_RE = re.compile(r"^urn:[a-z0-9]+:[A-Za-z0-9]{16,100}$", re.IGNORECASE)

FORMATS = {
    "best": [],
    "1080p": ["-f", "bv*[height<=1080]+ba/b[height<=1080]"],
    "audio": ["-x", "--audio-format", "mp3"],
}

FORMAT_LABELS = {
    "best": "Лучшее качество",
    "1080p": "Видео до 1080p",
    "audio": "Только аудио (mp3)",
}

ENGINE_LABELS = {
    "yt-dlp": "yt-dlp — видео и стримы (YouTube и ещё тысячи сайтов)",
    "aria2": "aria2c — прямые ссылки, torrent, magnet",
}


class BuildError(RuntimeError):
    """Command could not be constructed."""


def clean_url(raw: str) -> str:
    return raw.strip().strip("\"'").strip()


def _has_valid_xt(magnet: str) -> bool:
    query = magnet.split("?", 1)[1] if "?" in magnet else ""
    return any(
        key.lower() == "xt" and MAGNET_XT_RE.match(value)
        for key, value in parse_qsl(query, keep_blank_values=True)
    )


def _contains_control_chars(u: str) -> bool:
    return any(ord(ch) < 0x20 or ord(ch) == 0x7F for ch in u)


def validate_url(url: str) -> str:
    """Normalize and reject anything that could be parsed as tool options."""
    u = clean_url(url)
    if not u:
        raise ValueError("Пустая ссылка.")
    if _contains_control_chars(u):
        raise ValueError(
            "Ссылка содержит управляющие символы (перенос строки?) — скопируйте её заново."
        )
    if u.startswith("-"):
        raise ValueError("Ссылка не может начинаться с «-» (похоже на опцию, а не на URL).")
    if u.lower().startswith("magnet:"):
        if not _has_valid_xt(u):
            raise ValueError(
                "Ссылка magnet повреждена (не найден корректный xt=urn:...). "
                "Похоже, часть символов «&» потерялась при вставке в терминал — "
                "вставьте ссылку ещё раз или передайте её через -y."
            )
        return u
    try:
        parsed = urlparse(u)
    except ValueError as exc:
        raise ValueError(f"Не получается разобрать ссылку: {u!r}") from exc
    if parsed.scheme in ALLOWED_SCHEMES and parsed.netloc:
        return u
    p = Path(u)
    if p.suffix.lower() in TORRENT_FILES and p.is_file():
        return u
    raise ValueError(f"Не понимаю ссылку: {u!r} (нужен http(s)://, ftp://, magnet: или .torrent-файл).")


def detect_engine(url: str) -> str:
    try:
        parsed = urlparse(clean_url(url))
    except ValueError:
        return "yt-dlp"
    if parsed.scheme == "magnet":
        return "aria2"
    suffix = Path(parsed.path).suffix.lower()
    if suffix in TORRENT_FILES or suffix in FILE_EXT:
        return "aria2"
    return "yt-dlp"


def is_direct_download(url: str) -> bool:
    parsed = urlparse(url)
    return (parsed.scheme in ALLOWED_SCHEMES
            and Path(parsed.path).suffix.lower() in FILE_EXT)


COOKIES_BROWSERS = frozenset({
    "chrome", "chromium", "brave", "opera", "edge", "vivaldi",
    "firefox", "safari", "whale",
})


@dataclass
class Job:
    engine: str
    url: str
    out_dir: Path
    fmt: str = "best"
    cookies_browser: str | None = None


def _validate_cookies_browser(value: str) -> None:
    if _contains_control_chars(value):
        raise BuildError(
            f"Имя браузера для куки содержит управляющие символы: {value!r}"
        )
    parts = re.split(r"[+:]", value)
    head = parts[0].lower()
    if head not in COOKIES_BROWSERS:
        known = ", ".join(sorted(COOKIES_BROWSERS))
        raise BuildError(
            f"Неизвестный браузер для куки: {value!r} (поддерживаются: {known}; "
            "профиль можно указать так: chrome:Profile 1)"
        )
    if any(".." in part for part in parts[1:]):
        raise BuildError(
            f"Профиль/контейнер для куки не может содержать «..»: {value!r}"
        )


def build(job: Job, tc: Toolchain) -> list[str]:
    if job.cookies_browser is not None and job.engine == "yt-dlp":
        _validate_cookies_browser(job.cookies_browser)

    out = job.out_dir.expanduser().absolute()
    try:
        out.mkdir(parents=True, exist_ok=True)
    except OSError as exc:
        raise BuildError(f"Не удалось создать папку {out}: {exc}") from exc

    if job.engine == "aria2":
        exe = tc.require("aria2c")
        # --no-conf: never load aria2.conf from cwd/APPDATA (option hijacking)
        return [exe, "--no-conf", "-d", str(out),
                *ARIA2_RESUME, *ARIA2_WORKERS, "--", job.url]

    exe = tc.require("yt-dlp")
    # --ignore-config: never load yt-dlp.conf from cwd (would allow --exec RCE)
    cmd = [exe, "--ignore-config", "-P", str(out), "--no-playlist"]
    if job.cookies_browser is not None:
        cmd += ["--cookies-from-browser", job.cookies_browser]
    cmd += FORMATS.get(job.fmt, [])
    if tc.aria2c and is_direct_download(job.url):
        cmd += ["--external-downloader", tc.aria2c,
                "--external-downloader-args", EXTERNAL_ARIA2_ARGS]
    cmd += ["--", job.url]
    return cmd


def preflight_warning(job: Job) -> list[str]:
    warns: list[str] = []
    if job.engine != "yt-dlp":
        if job.cookies_browser is not None:
            warns.append(
                "Куки из браузера применимы только к yt-dlp — для этого движка они проигнорированы."
            )
        if job.fmt != "best":
            warns.append("Формат применим только к yt-dlp — для aria2 он проигнорирован.")
    elif not shutil.which("ffmpeg"):
        if job.fmt == "audio":
            warns.append("Не найден ffmpeg — извлечение mp3 может не сработать.")
        else:
            warns.append(
                "Не найден ffmpeg — склейка видео и аудио недоступна, "
                "yt-dlp выберет однодорожечный формат (качество может быть ниже)."
            )
    return warns


def run(cmd: list[str]) -> int:
    try:
        proc = subprocess.Popen(cmd)
    except OSError:
        return 127
    try:
        code = proc.wait()
    except KeyboardInterrupt:
        try:
            proc.wait(timeout=5)
        except (subprocess.TimeoutExpired, KeyboardInterrupt):
            _terminate(proc)
        return 130
    if code < 0:
        return 128 - code
    return code


def _terminate(proc: subprocess.Popen) -> None:
    try:
        proc.terminate()
        proc.wait(timeout=5)
    except (OSError, subprocess.TimeoutExpired, KeyboardInterrupt):
        with contextlib.suppress(OSError):
            proc.kill()
