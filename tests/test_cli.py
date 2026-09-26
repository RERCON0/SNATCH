from snatch.cli import _should_offer_cookies

BASE_PLAN = {"url": "u", "engine": "yt-dlp", "fmt": "best", "out_dir": "d"}


def test_offers_after_child_failure():
    assert _should_offer_cookies(BASE_PLAN, 1)
    assert _should_offer_cookies(BASE_PLAN, 3)


def test_no_offer_on_success_or_own_errors():
    for code in (0, 2, 127, 130):
        assert not _should_offer_cookies(BASE_PLAN, code)


def test_no_offer_for_aria2_engine():
    assert not _should_offer_cookies({**BASE_PLAN, "engine": "aria2"}, 1)


def test_offer_repeats_after_failed_cookies_attempt():
    assert _should_offer_cookies({**BASE_PLAN, "cookies_browser": "chrome"}, 1)
