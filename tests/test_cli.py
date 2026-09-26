import argparse

from snatch.cli import _plan_from_args, _should_offer_cookies

BASE_PLAN = {"url": "u", "engine": "yt-dlp", "fmt": "best", "out_dir": "d"}


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
