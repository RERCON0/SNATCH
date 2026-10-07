# SNATCH command line

[English](CLI.md) · [Русский](CLI.ru.md)

| Flag | Purpose |
|---|---|
| `--lang <en|ru>` | Set and remember the interface language; with `--help`, preview help without saving |
| `-o, --output <folder>` | Download folder |
| `-j, --jobs <1..16>` | Concurrent downloads for multiple links (default: 3) |
| `-e, --engine {yt-dlp,aria2}` | Engine (default: automatic) |
| `-f, --format {best,2160p,1440p,1080p,720p,480p,audio,audio-src}` | yt-dlp format; `audio` converts to mp3, `audio-src` keeps the original audio |
| `--subs` | Download subtitles alongside the video (yt-dlp) |
| `--sub-langs <ru,en>` | Subtitle languages (default: ru,en; independent of interface language) |
| `--playlist` | Download the entire playlist instead of a single video |
| `--yt-dlp-args <ARGS>` | Unrestricted yt-dlp arguments, including its config, output naming and `--exec`; SNATCH's safety guarantees apply to its own flags and the aria2 allowlist |
| `--aria2-args <ARGS>` | Allowed aria2c options: speed/connection limits, proxies, headers, User-Agent/Referer, timeouts and retries. Values accept `=` or a space (`--max-tries=3`, `-x 16`). Options affecting filenames, folders, resume, config or input files, including abbreviations, are rejected |
| `--no-continue` | Restart an interrupted download from scratch (useful for corrupt `.part` files; the GUI retries automatically) |
| `--cookies-from-browser <browser[:profile]>` | Pass a browser session to yt-dlp; e.g. `chrome:Profile 1` |
| `-y, --yes` | Skip prompts (requires a URL and `-o`) |
| `--clear-history` | Clear recent links and folders |
| `--install-tools` | Download yt-dlp, aria2c, ffmpeg and Deno into SNATCH's managed folder (Windows) |
| `-h, --help` | Show help |
| `-V, --version` | Show version |

## Interface language

```powershell
snatch --lang ru
snatch --lang en
snatch --lang ru --help
```

English is the default for profiles without a saved language. The GUI shares this
preference and has an EN / RU button next to the theme button in its title bar. `--lang` can also be
combined with download arguments or `--install-tools`.

## External tool paths

```powershell
$env:SNATCH_YT_DLP = "C:\tools\yt-dlp.exe"
$env:SNATCH_ARIA2C = "C:\tools\aria2c.exe"
snatch "magnet:?xt=urn:btih:..."
```

Quote URLs and paths that contain spaces. `--yt-dlp-args` exposes the downloader's
full capabilities, including command execution with `--exec`; use it deliberately.
Extra aria2 arguments are restricted to the safe allowlist.
