use super::*;

/// Optional yt-dlp/aria2 behaviour carried in from the CLI/GUI. Deliberately
/// not part of `Job`: locks, owner marks and pause/resume key on the job
/// alone, so toggling these never changes a job's identity.
#[derive(Clone, Default)]
pub struct RunExtras {
    /// Download subtitles next to the video (manual + automatic captions).
    pub subs: bool,
    /// yt-dlp `--sub-langs` value; empty means [`DEFAULT_SUB_LANGS`].
    pub sub_langs: String,
    /// Follow playlists instead of refusing them.
    pub playlist: bool,
    /// Extra yt-dlp arguments, already split into argv tokens.
    pub extra_ytdlp: Vec<String>,
    /// Extra aria2c arguments, placed before the `--` terminator.
    pub extra_aria2: Vec<String>,
}

/// Subtitle languages requested when the user leaves the field empty.
pub const DEFAULT_SUB_LANGS: &str = "ru,en";

impl RunExtras {
    /// The `--sub-langs` value that is actually used (and reported).
    pub fn effective_sub_langs(&self) -> &str {
        let langs = self.sub_langs.trim();
        if langs.is_empty() {
            DEFAULT_SUB_LANGS
        } else {
            langs
        }
    }
}

/// Splits a user-supplied "extra arguments" string into argv tokens: quotes
/// group, whitespace separates, nothing is expanded. The tokens go straight
/// to `Command::args` (no shell), so only what the user typed can run.
pub fn split_cli_args(s: &str) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    // Which quote opened the current span: a `'` inside double quotes is a
    // literal apostrophe (`"D:\Bob's Files"`), not a closing quote.
    let mut quote: Option<char> = None;
    let mut token = false;
    for c in s.chars() {
        if c == '\0' {
            return Err(tr!(
                "extra arguments contain NUL",
                "дополнительные аргументы содержат NUL"
            )
            .into());
        }
        match quote {
            Some(q) if c == q => {
                quote = None;
                token = true;
            }
            Some(_) => {
                cur.push(c);
                token = true;
            }
            None if c == '"' || c == '\'' => {
                quote = Some(c);
                token = true;
            }
            None if c.is_whitespace() => {
                if token || !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                    token = false;
                }
            }
            None => {
                cur.push(c);
                token = true;
            }
        }
    }
    if quote.is_some() {
        return Err(tr!(
            "unclosed quote in extra arguments",
            "незакрытая кавычка в дополнительных аргументах"
        )
        .into());
    }
    if token || !cur.is_empty() {
        out.push(cur);
    }
    Ok(out)
}

/// aria2c accepts any unambiguous prefix of a long option, so a blacklist of
/// full names is bypassable (`--ou=`, `--conti=true`). Only a whitelist is
/// safe: flags that cannot change WHAT and WHERE aria2 saves - rate/split/
/// connection limits, proxies, HTTP headers, timeouts, retries. Everything
/// else is rejected, because the lock/origin records were computed for
/// SNATCH's own pins (name, dir, `-c`, config, input files).
pub(super) const ARIA2_EXTRA_LONG: &[&str] = &[
    "max-connection-per-server",
    "split",
    "min-split-size",
    "max-download-limit",
    "max-overall-download-limit",
    "max-upload-limit",
    "max-overall-upload-limit",
    "max-tries",
    "retry-wait",
    "timeout",
    "connect-timeout",
    "lowest-speed-limit",
    "max-file-not-found",
    "all-proxy",
    "http-proxy",
    "https-proxy",
    "ftp-proxy",
    "no-proxy",
    "all-proxy-user",
    "all-proxy-passwd",
    "http-user",
    "http-passwd",
    "header",
    "user-agent",
    "referer",
    "load-cookies",
];
pub(super) const ARIA2_EXTRA_SHORT: &[char] = &['x', 's', 'k', 'j', 'm', 't', 'u', 'U'];

pub(super) fn aria2_extra_allowed(arg: &str) -> bool {
    if let Some(rest) = arg.strip_prefix("--") {
        let name = rest.split('=').next().unwrap_or(rest);
        if name.is_empty() {
            return false;
        }
        // A prefix must match exactly one whitelisted flag: if it is also a
        // prefix of some other aria2 option, aria2 itself treats it as
        // ambiguous and refuses, so accepting it cannot enable anything else.
        if ARIA2_EXTRA_LONG.contains(&name) {
            return true;
        }
        let mut matches = ARIA2_EXTRA_LONG.iter().filter(|f| f.starts_with(name));
        return matches.next().is_some() && matches.next().is_none();
    }
    if let Some(rest) = arg.strip_prefix('-') {
        return rest
            .chars()
            .next()
            .is_some_and(|c| ARIA2_EXTRA_SHORT.contains(&c));
    }
    false
}

/// Validates the whole extras list, not just single tokens: a whitelisted
/// flag may take its value as the next token (`-x 16`, `--max-tries 3`,
/// `--header "A: b"`), and that token is a value, not a bare URI. A bare
/// token anywhere else is refused so a second URL cannot sneak in.
pub(super) fn validate_aria2_extras(args: &[String]) -> Result<(), String> {
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if !aria2_extra_allowed(a) {
            let why = if a.starts_with('-') {
                tr_format!(
                    "extra aria2 argument `{a}` is not allowed",
                    "доп. аргумент aria2 `{a}` вне разрешённого набора"
                )
            } else {
                tr_format!(
                    "a positional argument `{a}` is not allowed in extras (an extra URL?)",
                    "голый аргумент `{a}` в extras не разрешён (лишний URL?)"
                )
            };
            return Err(tr_format!("{why} — SNATCH controls filenames, folders, resume mode, config and input files; only limits, proxies, headers, timeouts and retries are allowed", "{why} — имя, папку, режим докачки, конфиг и входные файлы задаёт сам SNATCH; \
                 разрешены только лимиты, прокси, заголовки, таймауты и повторы"
            ));
        }
        // `--flag value` and `-x value` consume the next token as the value;
        // `--flag=value`/`-x16` carry it themselves. All whitelisted flags
        // take a value, so a missing one is a user error worth naming.
        let expects_value = !a.contains('=') && (a.starts_with("--") || a.len() == 2);
        if expects_value {
            i += 1;
            if i >= args.len() {
                return Err(tr_format!("extra aria2 argument `{a}` needs a value (e.g. `{a}=...` or `{a} ...`)", "доп. аргумент aria2 `{a}` требует значение (например, `{a}=...` или `{a} ...`)"
                ));
            }
        }
        i += 1;
    }
    Ok(())
}
