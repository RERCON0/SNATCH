import pytest

from downcli import tools
from downcli.tools import ToolNotFound, Toolchain, find


def test_env_override_used(tmp_path, monkeypatch):
    exe = tmp_path / "my-yt-dlp.exe"
    exe.write_bytes(b"")
    monkeypatch.setenv("SNATCH_YT_DLP", str(exe))
    assert find("yt-dlp") == str(exe)


def test_env_override_missing_file_ignored(tmp_path, monkeypatch):
    monkeypatch.setenv("SNATCH_YT_DLP", str(tmp_path / "gone.exe"))
    monkeypatch.setattr(tools.shutil, "which", lambda name: "C:/fake/yt-dlp.exe")
    assert find("yt-dlp") == "C:/fake/yt-dlp.exe"


def test_which_found(monkeypatch):
    monkeypatch.delenv("SNATCH_YT_DLP", raising=False)
    monkeypatch.delenv("SNATCH_ARIA2C", raising=False)
    monkeypatch.setattr(tools.shutil, "which", lambda name: "C:/fake/aria2c.exe")
    assert find("aria2c") == "C:/fake/aria2c.exe"


def test_nothing_found_raises(monkeypatch, tmp_path):
    monkeypatch.delenv("SNATCH_YT_DLP", raising=False)
    monkeypatch.delenv("SNATCH_ARIA2C", raising=False)
    monkeypatch.setattr(tools.shutil, "which", lambda name: None)
    monkeypatch.setenv("LOCALAPPDATA", str(tmp_path))
    with pytest.raises(ToolNotFound):
        find("aria2c")


def test_require_raises_when_missing():
    tc = Toolchain(yt_dlp=None, aria2c=None)
    with pytest.raises(ToolNotFound):
        tc.require("yt-dlp")


def test_require_returns_path():
    tc = Toolchain(yt_dlp="/usr/bin/yt-dlp", aria2c=None)
    assert tc.require("yt-dlp") == "/usr/bin/yt-dlp"
