from snatch import ui
from snatch.tools import Toolchain

TC_BOTH = Toolchain(yt_dlp="yt-dlp", aria2c="aria2c")


class _FakeAsk:
    def __init__(self, value):
        self.value = value

    def ask(self):
        return self.value


def test_ask_engine_auto_returns_single_available(monkeypatch):
    def boom(*a, **k):
        raise AssertionError("select must not be called with one engine")

    monkeypatch.setattr(ui.questionary, "select", boom)
    tc = Toolchain(yt_dlp="yt-dlp", aria2c=None)
    assert ui._ask_engine("https://youtube.com/watch?v=1", tc) == "yt-dlp"
    tc = Toolchain(yt_dlp=None, aria2c="aria2c")
    assert ui._ask_engine("https://youtube.com/watch?v=1", tc) == "aria2"


def test_ask_engine_no_engines_returns_none(monkeypatch):
    def boom(*a, **k):
        raise AssertionError("select must not be called")

    monkeypatch.setattr(ui.questionary, "select", boom)
    assert ui._ask_engine("https://x", Toolchain(yt_dlp=None, aria2c=None)) is None


def test_ask_engine_offers_guess_first(monkeypatch):
    seen = {}

    def fake_select(message, choices):
        seen["values"] = [c.value for c in choices]
        return _FakeAsk(seen["values"][0])

    monkeypatch.setattr(ui.questionary, "select", fake_select)
    assert ui._ask_engine("https://host/f.zip", TC_BOTH) == "aria2"
    assert seen["values"] == ["aria2", "yt-dlp"]


def test_confirm_mentions_cookies(monkeypatch):
    seen = {}

    def fake_confirm(message, **kwargs):
        seen["msg"] = message
        return _FakeAsk(True)

    monkeypatch.setattr(ui.questionary, "confirm", fake_confirm)
    assert ui._confirm("yt-dlp", "best", "d", "https://x", "chrome")
    assert "chrome" in seen["msg"]

    assert ui._confirm("yt-dlp", "best", "d", "https://x", None)
    assert "chrome" not in seen["msg"]


def test_confirm_aria2_has_no_cookies_line(monkeypatch):
    seen = {}

    def fake_confirm(message, **kwargs):
        seen["msg"] = message
        return _FakeAsk(True)

    monkeypatch.setattr(ui.questionary, "confirm", fake_confirm)
    assert ui._confirm("aria2", "best", "d", "magnet:?xt=1", None)
    assert "aria2c" in seen["msg"]
