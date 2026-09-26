import argparse

from snatch import cli
from snatch.cli import _plan_from_args, _should_offer_cookies

BASE_PLAN = {"url": "u", "engine": "yt-dlp", "fmt": "best", "out_dir": "d",
             "cookies_browser": None}


class _FakeAsk:
    def __init__(self, value):
        self.value = value

    def ask(self):
        return self.value


def test_offers_only_on_child_generic_failure():
    assert _should_offer_cookies(BASE_PLAN, 1)
    for code in (0, 2, 3, 127, 130, -9):
        assert not _should_offer_cookies(BASE_PLAN, code), code


def test_no_offer_for_aria2_engine():
    assert not _should_offer_cookies({**BASE_PLAN, "engine": "aria2"}, 1)


def test_offer_repeats_after_failed_cookies_attempt():
    assert _should_offer_cookies({**BASE_PLAN, "cookies_browser": "chrome"}, 1)


def test_plan_from_args_keeps_cookies_value():
    args = argparse.Namespace(url="https://x", output="d", engine=None,
                              format=None, cookies_browser="Chrome:Profile 1")
    plan = _plan_from_args(args)
    assert plan["cookies_browser"] == "Chrome:Profile 1"

    args.cookies_browser = None
    assert _plan_from_args(args)["cookies_browser"] is None


def test_retry_with_cookies_retries_and_succeeds(monkeypatch):
    confirms = iter([True])
    selects = iter(["chrome"])
    monkeypatch.setattr(cli.questionary, "confirm",
                        lambda *a, **k: _FakeAsk(next(confirms)))
    monkeypatch.setattr(cli.questionary, "select",
                        lambda *a, **k: _FakeAsk(next(selects)))
    codes = iter([0])
    seen = []

    def fake_download(plan, cfg, tc=None):
        seen.append(plan)
        return next(codes)

    monkeypatch.setattr(cli, "_download", fake_download)
    assert cli._retry_with_cookies(BASE_PLAN, 1, None, None) == 0
    assert seen == [{**BASE_PLAN, "cookies_browser": "chrome"}]


def test_retry_with_cookies_declined_returns_original_code(monkeypatch):
    monkeypatch.setattr(cli.questionary, "confirm", lambda *a, **k: _FakeAsk(None))

    def boom(*a, **k):
        raise AssertionError("_download must not be called")

    monkeypatch.setattr(cli, "_download", boom)
    assert cli._retry_with_cookies(BASE_PLAN, 1, None, None) == 1


def test_retry_with_cookies_no_browser_selected(monkeypatch):
    monkeypatch.setattr(cli.questionary, "confirm", lambda *a, **k: _FakeAsk(True))
    monkeypatch.setattr(cli.questionary, "select", lambda *a, **k: _FakeAsk(None))

    def boom(*a, **k):
        raise AssertionError("_download must not be called")

    monkeypatch.setattr(cli, "_download", boom)
    assert cli._retry_with_cookies(BASE_PLAN, 1, None, None) == 1


def test_retry_gives_up_after_second_failure(monkeypatch):
    confirms = iter([True, False])
    selects = iter(["chrome"])
    monkeypatch.setattr(cli.questionary, "confirm",
                        lambda *a, **k: _FakeAsk(next(confirms)))
    monkeypatch.setattr(cli.questionary, "select",
                        lambda *a, **k: _FakeAsk(next(selects)))
    calls = []

    def fake_download(plan, cfg, tc=None):
        calls.append(plan)
        return 1

    monkeypatch.setattr(cli, "_download", fake_download)
    assert cli._retry_with_cookies(BASE_PLAN, 1, None, None) == 1
    assert len(calls) == 1


def test_yes_mode_bad_url_skips_tool_discovery(tmp_path, monkeypatch, capsys):
    monkeypatch.setenv("LOCALAPPDATA", str(tmp_path))

    class FakeTC:
        @classmethod
        def discover(cls):
            raise AssertionError("discover should not run for invalid url")

    monkeypatch.setattr(cli, "Toolchain", FakeTC)
    assert cli.main(["https://[::1", "-y", "-o", str(tmp_path)]) == 2
    assert "разобрать" in capsys.readouterr().err


def test_yes_mode_requires_url_and_output(tmp_path, monkeypatch):
    monkeypatch.setenv("LOCALAPPDATA", str(tmp_path))
    assert cli.main(["-y"]) == 2
