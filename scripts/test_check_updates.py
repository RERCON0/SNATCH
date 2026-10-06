import io
import unittest
from unittest.mock import patch
import urllib.error

import check_updates


class Response(io.BytesIO):
    status = 200

    def __init__(self, data, length=None):
        super().__init__(data)
        self.headers = {} if length is None else {"Content-Length": str(length)}


class UpdateTests(unittest.TestCase):
    def test_pin_changes_are_detected(self):
        self.assertFalse(check_updates.compare("1.99.0", "1.99.0", "aria.zip", ["aria.zip"]))
        self.assertEqual(len(check_updates.compare("1.98.0", "1.99.0", "old.zip", ["new.zip"])), 2)

    def test_origin_and_token_never_escape_github(self):
        with self.assertRaises(ValueError):
            check_updates.fetch("http://api.github.com/repos/x")
        with self.assertRaises(ValueError):
            check_updates.fetch("https://api.github.com.evil.example/x")
        with self.assertRaises(ValueError):
            check_updates.fetch("https://static.rust-lang.org/dist/x", token="test-token")
        with self.assertRaises(ValueError):
            check_updates.NoRedirect().redirect_request(None, None, 302, "", {}, "https://evil.example")

    def test_success_oversize_and_truncated_metadata(self):
        with patch("check_updates.urllib.request.build_opener") as opener:
            opener.return_value.open.return_value = Response(b"abc", 3)
            self.assertEqual(check_updates.fetch("https://api.github.com/x"), b"abc")
            opener.return_value.open.return_value = Response(b"abc", 100)
            with self.assertRaises(ValueError):
                check_updates.fetch("https://api.github.com/x", max_bytes=3)
            opener.return_value.open.side_effect = lambda *a, **kw: Response(b"abc", 5)
            with patch("check_updates.time.sleep"), self.assertRaises(check_updates.http.client.IncompleteRead):
                check_updates.fetch("https://api.github.com/x")
            self.assertEqual(opener.return_value.open.call_count, 5)

    def test_transient_errors_retry_but_permanent_errors_do_not(self):
        with patch("check_updates.urllib.request.build_opener") as opener, patch("check_updates.time.sleep"):
            opener.return_value.open.side_effect = [TimeoutError(), Response(b"ok", 2)]
            self.assertEqual(check_updates.fetch("https://api.github.com/x"), b"ok")
            opener.return_value.open.reset_mock()
            opener.return_value.open.side_effect = urllib.error.HTTPError("https://api.github.com/x", 403, "denied", {}, None)
            with self.assertRaises(urllib.error.HTTPError):
                check_updates.fetch("https://api.github.com/x")
            opener.return_value.open.assert_called_once()


if __name__ == "__main__":
    unittest.main()
