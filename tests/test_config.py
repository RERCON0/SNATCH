import json

from snatch.config import Config


def _write_cfg(tmp_path, monkeypatch, data):
    monkeypatch.setenv("LOCALAPPDATA", str(tmp_path))
    d = tmp_path / "snatch"
    d.mkdir(parents=True, exist_ok=True)
    (d / "config.json").write_text(json.dumps(data), encoding="utf-8")


def test_load_non_dict_json_no_crash(tmp_path, monkeypatch):
    monkeypatch.setenv("LOCALAPPDATA", str(tmp_path))
    d = tmp_path / "snatch"
    d.mkdir()
    (d / "config.json").write_text("[1,2,3]", encoding="utf-8")
    cfg = Config.load()
    assert cfg.default_dir
    assert cfg.urls == []
    assert cfg.dirs == []


def test_load_wrong_field_types_sanitized(tmp_path, monkeypatch):
    _write_cfg(tmp_path, monkeypatch,
               {"urls": "abc", "dirs": 5, "last_dir": 42, "default_dir": None})
    cfg = Config.load()
    assert cfg.urls == []
    assert cfg.dirs == []
    assert cfg.last_dir == ""
    assert cfg.default_dir


def test_load_mixed_list_keeps_strings_only(tmp_path, monkeypatch):
    _write_cfg(tmp_path, monkeypatch, {"urls": ["http://ok", 7, None]})
    cfg = Config.load()
    assert cfg.urls == ["http://ok"]


def test_unknown_keys_ignored(tmp_path, monkeypatch):
    _write_cfg(tmp_path, monkeypatch, {"hacker": True, "urls": []})
    cfg = Config.load()
    assert not hasattr(cfg, "hacker")


def test_history_cap_and_dedupe(tmp_path, monkeypatch):
    monkeypatch.setenv("LOCALAPPDATA", str(tmp_path))
    cfg = Config.load()
    for i in range(20):
        cfg.remember_url(f"http://x/{i}")
    assert len(cfg.urls) == 15
    assert cfg.urls[0] == "http://x/19"
    cfg.remember_url("http://x/19")
    assert cfg.urls.count("http://x/19") == 1


def test_save_load_roundtrip(tmp_path, monkeypatch):
    monkeypatch.setenv("LOCALAPPDATA", str(tmp_path))
    cfg = Config.load()
    cfg.remember_url("https://example.com/v.mp4")
    cfg.remember_dir(str(tmp_path))
    cfg.save()
    again = Config.load()
    assert again.urls == cfg.urls
    assert again.dirs == cfg.dirs
    assert again.last_dir == cfg.last_dir


def test_remember_dir_expands_user(tmp_path, monkeypatch):
    monkeypatch.setenv("LOCALAPPDATA", str(tmp_path))
    cfg = Config.load()
    cfg.remember_dir("~/Videos")
    assert "~" not in cfg.last_dir
