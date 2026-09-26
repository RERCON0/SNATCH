from pathlib import Path

import pytest

from snatch import engines
from snatch.engines import (
    BuildError,
    Job,
    build,
    clean_url,
    detect_engine,
    is_direct_download,
    preflight_warning,
    run,
    validate_url,
)
from snatch.tools import Toolchain, ToolNotFound

TC_BOTH = Toolchain(yt_dlp="yt-dlp", aria2c="aria2c")
TC_YTDLP = Toolchain(yt_dlp="yt-dlp", aria2c=None)
TC_ARIA2 = Toolchain(yt_dlp=None, aria2c="aria2c")


MAGNET_OK = "magnet:?xt=urn:btih:73510898AF9039563184FAFE9CB0F186DE6AAA4"


def test_validate_accepts_supported():
    assert validate_url("https://example.com/v.mp4") == "https://example.com/v.mp4"
    assert validate_url(f"  {MAGNET_OK} ") == MAGNET_OK
    assert validate_url("ftp://host/file.zip") == "ftp://host/file.zip"


def test_validate_accepts_magnet_with_tracker_and_name():
    magnet = f"{MAGNET_OK}&tr=http%3A%2F%2Fbt.example.org%2Fann&dn=Some%20Name"
    assert validate_url(magnet) == magnet


def test_validate_rejects_corrupted_magnet():
    # Same link as MAGNET_OK, but with both "&" separators lost/mangled the
    # way a Windows terminal paste bug in prompt_toolkit did in practice:
    # one "&" dropped, the other replaced by a stray letter.
    corrupted = (
        "magnet:?xt=urn:btih:73510898AF9039563184FAFE9CB0F186DE6AAA4Btr="
        "http%3A%2F%2Fbt.example.org%2Fanndn=Name"
    )
    with pytest.raises(ValueError):
        validate_url(corrupted)


def test_validate_rejects_magnet_without_xt():
    with pytest.raises(ValueError):
        validate_url("magnet:?dn=NoHashHere")


def test_validate_magnet_xt_key_case_insensitive():
    m = "magnet:?XT=urn:btih:73510898AF9039563184FAFE9CB0F186DE6AAA4B"
    assert validate_url(m) == m


def test_validate_rejects_control_chars():
    with pytest.raises(ValueError):
        validate_url("https://host.com/pa\nth")
    with pytest.raises(ValueError):
        validate_url("https://host.com/pa\x00th")


def test_validate_rejects_unparseable_url_with_clear_error():
    with pytest.raises(ValueError, match="разобрать"):
        validate_url("https://[::1")


def test_validate_rejects_unsupported_schemes():
    for bad in ("file:///etc/passwd", "ws://host/x", "sftp://host/x", "ftps://host/x"):
        with pytest.raises(ValueError):
            validate_url(bad)


def test_validate_rejects_option_like():
    with pytest.raises(ValueError):
        validate_url("-x")
    with pytest.raises(ValueError):
        validate_url("   ")
    with pytest.raises(ValueError):
        validate_url("\"\"")


def test_validate_torrent_file(tmp_path):
    f = tmp_path / "a.torrent"
    f.write_bytes(b"d4:infod0:e")
    assert validate_url(str(f)) == str(f)
    with pytest.raises(ValueError):
        validate_url(str(tmp_path / "missing.torrent"))


def test_clean_url_strips_quotes():
    assert clean_url('  "https://x"  ') == "https://x"
    assert clean_url("'https://x'") == "https://x"


def test_detect_engine():
    assert detect_engine("magnet:?xt=1") == "aria2"
    assert detect_engine("https://youtube.com/watch?v=1") == "yt-dlp"
    assert detect_engine("https://host.com/file.zip") == "aria2"
    assert detect_engine("https://host.com/file.MP4?x=1") == "aria2"


def test_detect_engine_torrent_goes_to_aria2(tmp_path):
    f = tmp_path / "a.torrent"
    f.write_bytes(b"d4:infod0:e")
    assert detect_engine(str(f)) == "aria2"
    assert detect_engine("https://host/x.torrent") == "aria2"
    assert detect_engine("https://host/x.meta4") == "aria2"


def test_detect_engine_malformed_url_no_crash():
    assert detect_engine("https://[::1") == "yt-dlp"
    assert detect_engine("https://[bad") == "yt-dlp"


def test_is_direct_download():
    assert is_direct_download("https://host/file.zip")
    assert is_direct_download("ftp://host/file.iso")
    assert not is_direct_download("https://youtube.com/watch?v=1")
    assert not is_direct_download("magnet:?xt=1")
    assert not is_direct_download("https://host/page.html")


def test_build_aria2(tmp_path):
    job = Job(engine="aria2", url="magnet:?xt=1", out_dir=tmp_path / "d")
    cmd = build(job, TC_ARIA2)
    assert cmd[1] == "--no-conf"
    assert cmd[cmd.index("-d") + 1] == str(tmp_path / "d")
    assert cmd[-2] == "--"
    assert cmd[-1] == "magnet:?xt=1"


def test_build_ytdlp_page_no_external_downloader(tmp_path):
    job = Job(engine="yt-dlp", url="https://youtube.com/watch?v=1", out_dir=tmp_path)
    cmd = build(job, TC_BOTH)
    assert "--ignore-config" in cmd
    assert "--no-playlist" in cmd
    assert "--external-downloader" not in cmd
    assert cmd[-2:] == ["--", "https://youtube.com/watch?v=1"]


def test_build_ytdlp_direct_link_uses_external_downloader(tmp_path):
    job = Job(engine="yt-dlp", url="https://host/file.zip", out_dir=tmp_path)
    cmd = build(job, TC_BOTH)
    assert cmd[cmd.index("--external-downloader") + 1] == "aria2c"
    args = cmd[cmd.index("--external-downloader-args") + 1]
    assert "--no-conf" in args


def test_build_ytdlp_no_aria2_tool_no_external(tmp_path):
    job = Job(engine="yt-dlp", url="https://host/file.zip", out_dir=tmp_path)
    cmd = build(job, TC_YTDLP)
    assert "--external-downloader" not in cmd


def test_build_format_flags(tmp_path):
    job = Job(engine="yt-dlp", url="https://youtube.com/watch?v=1",
              out_dir=tmp_path, fmt="1080p")
    cmd = build(job, TC_YTDLP)
    assert "bv*[height<=1080]+ba/b[height<=1080]" in cmd
    job = Job(engine="yt-dlp", url="https://youtube.com/watch?v=1",
              out_dir=tmp_path, fmt="audio")
    cmd = build(job, TC_YTDLP)
    assert "-x" in cmd


def test_build_out_dir_neutralized(tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    job = Job(engine="yt-dlp", url="https://x", out_dir=Path("-P"))
    cmd = build(job, TC_YTDLP)
    value = cmd[cmd.index("-P") + 1]
    assert not value.startswith("-")
    assert Path(value).is_absolute()


def test_build_cookies_browser(tmp_path):
    for value in ("chrome", "Chrome", "chrome:Profile 1",
                  "firefox+keyring:prof::C:\\container.sqli"):
        job = Job(engine="yt-dlp", url="https://youtube.com/watch?v=1",
                  out_dir=tmp_path, cookies_browser=value)
        cmd = build(job, TC_YTDLP)
        assert cmd[cmd.index("--cookies-from-browser") + 1] == value, value


def test_build_rejects_bad_browser_name(tmp_path):
    for bad in ("--exec=danger", "chrome;rm", "safari;rm -rf /", "",
                "nonsense", "somesite:prof", " chrome",
                "chrome:..", "chrome:../evil", "firefox:prof::..\\cnt",
                "chrome:pro\x00file", "chrome:a\nb"):
        job = Job(engine="yt-dlp", url="https://x", out_dir=tmp_path,
                  cookies_browser=bad)
        with pytest.raises(BuildError):
            build(job, TC_YTDLP)


def test_preflight_warns_on_aria2_with_cookies(tmp_path):
    job = Job(engine="aria2", url="magnet:?xt=1", out_dir=tmp_path,
              cookies_browser="chrome")
    warns = preflight_warning(job)
    assert any("yt-dlp" in w for w in warns)
    cmd = build(job, TC_ARIA2)
    assert "--cookies-from-browser" not in cmd


def test_preflight_warns_on_aria2_with_format(tmp_path):
    job = Job(engine="aria2", url="https://h/f.zip", out_dir=tmp_path, fmt="audio")
    warns = preflight_warning(job)
    assert any("формат" in w.lower() for w in warns)


def test_preflight_no_aria2_warning_without_cookies(tmp_path):
    job = Job(engine="aria2", url="magnet:?xt=1", out_dir=tmp_path)
    assert preflight_warning(job) == []


def test_preflight_warns_ffmpeg_missing_for_all_ytdlp_formats(tmp_path, monkeypatch):
    monkeypatch.setattr(engines.shutil, "which", lambda name: None)
    for fmt in ("audio", "best", "1080p"):
        job = Job(engine="yt-dlp", url="https://x", out_dir=tmp_path, fmt=fmt)
        assert any("ffmpeg" in w for w in preflight_warning(job)), fmt


def test_preflight_no_warning_when_ffmpeg_present(tmp_path, monkeypatch):
    monkeypatch.setattr(engines.shutil, "which", lambda name: "/usr/bin/ffmpeg")
    job = Job(engine="yt-dlp", url="https://x", out_dir=tmp_path, fmt="audio")
    assert preflight_warning(job) == []


def test_build_aria2_max_tries_bounded(tmp_path):
    job = Job(engine="aria2", url="magnet:?xt=1", out_dir=tmp_path)
    cmd = build(job, TC_ARIA2)
    assert "--max-tries=10" in cmd
    assert "--max-tries=0" not in cmd


class _FakeProc:
    def __init__(self, results):
        self.results = list(results)

    def wait(self, timeout=None):
        item = self.results.pop(0)
        if isinstance(item, BaseException):
            raise item
        return item

    def terminate(self):
        pass

    def kill(self):
        pass


def test_run_passthrough_codes(monkeypatch):
    monkeypatch.setattr(engines.subprocess, "Popen", lambda cmd: _FakeProc([0]))
    assert run(["x"]) == 0
    monkeypatch.setattr(engines.subprocess, "Popen", lambda cmd: _FakeProc([3]))
    assert run(["x"]) == 3


def test_run_missing_binary_127(monkeypatch):
    def boom(cmd):
        raise OSError("nope")

    monkeypatch.setattr(engines.subprocess, "Popen", boom)
    assert run(["x"]) == 127


def test_run_signal_code_mapped_posix_style(monkeypatch):
    monkeypatch.setattr(engines.subprocess, "Popen", lambda cmd: _FakeProc([-2]))
    assert run(["x"]) == 130
    monkeypatch.setattr(engines.subprocess, "Popen", lambda cmd: _FakeProc([-9]))
    assert run(["x"]) == 137


def test_run_keyboard_interrupt_child_exits_130(monkeypatch):
    proc = _FakeProc([KeyboardInterrupt(), 0])
    monkeypatch.setattr(engines.subprocess, "Popen", lambda cmd: proc)
    assert run(["x"]) == 130


def test_run_keyboard_interrupt_child_stuck_terminates(monkeypatch):
    import subprocess as sp

    proc = _FakeProc([KeyboardInterrupt(), sp.TimeoutExpired(cmd="x", timeout=5), 0])
    monkeypatch.setattr(engines.subprocess, "Popen", lambda cmd: proc)
    assert run(["x"]) == 130


def test_build_missing_tool(tmp_path):
    job = Job(engine="yt-dlp", url="https://x", out_dir=tmp_path)
    with pytest.raises(ToolNotFound):
        build(job, TC_ARIA2)


def test_build_mkdir_failure(tmp_path, monkeypatch):
    def boom(self, *args, **kwargs):
        raise OSError("disk full")

    monkeypatch.setattr("pathlib.Path.mkdir", boom)
    job = Job(engine="yt-dlp", url="https://x", out_dir=tmp_path / "sub")
    with pytest.raises(BuildError):
        build(job, TC_YTDLP)
