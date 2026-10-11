# SNATCH command line

[English](CLI.md) · [Русский](CLI.ru.md)

| Flag | Purpose |
|---|---|
| `--lang <en|ru>` | Set and remember the interface language; with `--help`, preview help without saving |
| `-o, --output <folder>` | Download folder |
| `-j, --jobs <1..16>` | Concurrent downloads for multiple links (default: 3) |
| `-e, --engine {yt-dlp,aria2}` | Engine (default: automatic) |
| `-f, --format {best,2160p,1440p,1080p,720p,480p,audio,audio-src,subs}` | yt-dlp format; `audio` converts to mp3, `audio-src` keeps the original audio, `subs` downloads subtitles only (`.srt`, no video) |
| `--subs` | Download subtitles alongside the video (yt-dlp) |
| `--sub-langs <ru,en>` | Subtitle languages (default: ru,en; independent of interface language) |
| `--playlist` | Download the entire playlist instead of a single video |
| `--yt-dlp-args <ARGS>` | Unrestricted yt-dlp arguments, including its config, output naming and `--exec`; SNATCH's safety guarantees apply to its own flags and the aria2 allowlist |
| `--aria2-args <ARGS>` | Allowed aria2c options: speed/connection limits, proxies, headers, User-Agent/Referer, a cookie file (`--load-cookies`), timeouts and retries. Values accept `=` or a space (`--max-tries=3`, `-x 16`). Options affecting filenames, folders, resume, config or input files, including abbreviations, are rejected |
| `--no-continue` | Restart an interrupted yt-dlp download from scratch (useful for corrupt `.part` files; aria2c verifies its own parts; the GUI retries automatically) |
| `--cookies-from-browser <browser[:profile]>` | Pass a browser session to yt-dlp; e.g. `chrome:Profile 1` |
| `-y, --yes` | Skip prompts (requires a URL and `-o`) |
| `--clear-history` | Clear recent links and folders |
| `--install-tools` | Download yt-dlp, aria2c, ffmpeg and Deno into SNATCH's managed folder (Windows) |
| `-h, --help` | Show help |
| `-V, --version` | Show version |

Audio modes embed the source thumbnail and available metadata by default. `audio-src` extracts AAC/Opus without re-encoding (WebM/Opus becomes `.opus`). This needs ffmpeg from `snatch --install-tools`. If the source has no thumbnail, audio still downloads. To omit artwork, pass `--yt-dlp-args "--no-embed-thumbnail"`.

The `subs` format downloads subtitles only, without the video: manual and automatic captions in the `--sub-langs` languages (default `ru,en`), converted to `.srt`. This needs ffmpeg from `snatch --install-tools`; without it yt-dlp keeps the source `.vtt`. If the video has none of the requested subtitles, the job is reported as failed with a clear message: nothing is written, and the video itself is never downloaded in this mode.

When a yt-dlp download fails in a way that cookies usually fix — sign-in and bot checks, age restrictions, HTTP 429 rate limiting — the interactive CLI offers to retry with browser cookies and asks which browser to read them from. The `-y` mode prints a tip instead, since it cannot ask.

## Link detection and formats

SNATCH checks HTTP(S) links without a recognizable file name using response headers and redirects before recommending an engine. Attachments and direct file responses favor aria2, even if the original URL has no extension; links whose path already ends in a known file extension skip the check. The filename comes from a safe `Content-Disposition` name, then the final URL; it is pinned before download so file locks and resume checks use the same name. Discovery uses no browser cookies or response body, has a six-second timeout, follows at most five redirects and retries once on a network failure; a 5xx or 429 answer is treated as "not checked" rather than a verdict, and the CLI says so instead of pretending the guess was verified. If discovery fails, URL-based detection and naming remain the fallback. An explicit `--engine` choice is always respected.

Downloads refused because the stream is DRM-encrypted (the SoundCloud Go+ catalog, the occasional YouTube format) are reported as such: yt-dlp does not remove DRM, so SNATCH does not offer a cookies retry for them. On SoundCloud such tracks expose a plain 30-second preview only, sometimes with a fresh attempt: SNATCH cannot get the full track, and neither can any tool that does not break DRM.

Threads post and share links are handled by a bundled yt-dlp extractor plugin (`plugins/snatch_threads.py`), which SNATCH installs into its own plugin folder and passes via `--plugin-dirs`; yt-dlp's default plugin folders stay disabled. On Windows, Instagram, Threads and their CDN hosts go through yt-dlp as well: aria2c's TLS client cannot complete a handshake with Meta's servers, and if another host turns aria2c away (that TLS error, or HTTP 403/429 that yt-dlp does not get), the job is retried with yt-dlp and the host is remembered for the rest of the run.

The CLI format menu and GUI dropdown offer best-available video and audio extraction for Instagram, without resolution caps or subtitles. Audio sites such as SoundCloud, Bandcamp and Mixcloud offer original audio and extraction/conversion modes. These are site presets, not a list of formats detected in a particular item. Instagram photo downloads are not supported by yt-dlp; audio extraction needs an audible video and ffmpeg. Other sites keep all format choices, including subtitles. Explicit `--format`, `--subs` and `--yt-dlp-args` remain available.

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
