use super::*;

/// Failure status for a finished batch job. The .aria2 and DRM cases get
/// their own explanation; everything else is the generic code line.
pub(super) fn failure_summary(code: i32, missing_control: bool, drm: bool) -> String {
    if missing_control {
        tr!(
            "✘ Files exist without .aria2 — choose an empty folder",
            "✘ Файлы уже есть, но нет .aria2 — выберите пустую папку"
        )
        .to_string()
    } else if drm {
        format!("✘ {}", engines::drm_protected_short())
    } else {
        tr_format!(
            "✘ Error (code {}). See the messages above.",
            "✘ Ошибка (код {}). См. сообщения выше.",
            code
        )
    }
}

pub(super) fn should_offer_cookies(plan: &Plan, code: i32, auth_hint: bool) -> bool {
    code == RETRY_CODE && auth_hint && plan.engine == Engine::YtDlp
}

pub(super) fn retry_with_cookies(
    plan: Plan,
    result: RunResult,
    cfg: &mut Config,
    tc: &Toolchain,
) -> RunResult {
    retry_with_cookies_inner(
        plan,
        result,
        cfg,
        |question, default| Confirm::new(question).with_default(default).prompt().ok(),
        || {
            let mut browsers: Vec<&str> = COOKIES_BROWSERS.to_vec();
            browsers.sort_unstable();
            Select::new(tr!("Browser:", "Браузер:"), browsers)
                .with_help_message(select_help())
                .prompt()
                .ok()
                .map(|b| b.to_string())
        },
        |p, c| download(p, c, Some(tc)),
    )
}

/// `confirm_fn`/`select_browser_fn`/`download_fn` are injected so the retry
/// loop (declined / no browser chosen / gives-up-after-second-failure) can be
/// exercised in tests without a real terminal - the Rust equivalent of
/// Python's `monkeypatch.setattr(cli.questionary, "confirm", ...)`.
pub(super) fn retry_with_cookies_inner(
    mut plan: Plan,
    mut result: RunResult,
    cfg: &mut Config,
    mut confirm_fn: impl FnMut(&str, bool) -> Option<bool>,
    mut select_browser_fn: impl FnMut() -> Option<String>,
    mut download_fn: impl FnMut(&Plan, &mut Config) -> RunResult,
) -> RunResult {
    while should_offer_cookies(&plan, result.code, result.auth_hint) {
        let had_cookies = plan.cookies_browser.is_some();
        let question = if had_cookies {
            tr_format!(
                "\nDownload failed (code {}). Try cookies from another browser?",
                "\nНе скачалось (код {}). Попробовать с куками другого браузера?",
                result.code
            )
        } else {
            tr_format!("\nDownload failed (code {}). This site may require sign-in or rate-limit the requests: retry with your browser cookies?", "\nНе скачалось (код {}). Возможно, нужен вход или сайт ограничил запросы: можно взять \
                 куки из браузера и повторить. Попробовать?",
                result.code
            )
        };
        let Some(true) = confirm_fn(&question, !had_cookies) else {
            return result;
        };
        let Some(browser) = select_browser_fn() else {
            return result;
        };
        plan.cookies_browser = Some(browser);
        result = download_fn(&plan, cfg);
    }
    result
}
