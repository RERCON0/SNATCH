//! Built-in English/Russian UI strings; no runtime catalogs or dependencies.
use std::sync::atomic::{AtomicU8, Ordering};

use serde::Serialize;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Language {
    #[default]
    En,
    Ru,
}

impl Language {
    pub fn parse(value: &str) -> Result<Self, &'static str> {
        match value {
            "en" => Ok(Self::En),
            "ru" => Ok(Self::Ru),
            _ => Err("expected en or ru"),
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::En => "English",
            Self::Ru => "Русский",
        }
    }
}

static LANGUAGE: AtomicU8 = AtomicU8::new(0);

pub fn set_language(language: Language) {
    LANGUAGE.store(u8::from(language == Language::Ru), Ordering::Relaxed);
}

pub fn is_russian() -> bool {
    #[cfg(test)]
    if let Some(language) = TEST_LANGUAGE.with(|slot| slot.get()) {
        return language == Language::Ru;
    }
    LANGUAGE.load(Ordering::Relaxed) == 1
}

#[cfg(test)]
thread_local! {
    static TEST_LANGUAGE: std::cell::Cell<Option<Language>> = const { std::cell::Cell::new(None) };
}

#[cfg(test)]
pub(crate) fn with_language<T>(language: Language, action: impl FnOnce() -> T) -> T {
    struct Restore(Option<Language>);
    impl Drop for Restore {
        fn drop(&mut self) {
            TEST_LANGUAGE.with(|slot| slot.set(self.0));
        }
    }
    let _restore = Restore(TEST_LANGUAGE.with(|slot| slot.replace(Some(language))));
    action()
}

#[macro_export]
macro_rules! tr {
    ($en:expr, $ru:expr) => {
        if $crate::i18n::is_russian() {
            $ru
        } else {
            $en
        }
    };
}

#[macro_export]
macro_rules! tr_format {
    ($en:literal, $ru:literal $(, $args:expr)* $(,)?) => {
        if $crate::i18n::is_russian() {
            format!($ru $(, $args)*)
        } else {
            format!($en $(, $args)*)
        }
    };
}

#[macro_export]
macro_rules! tr_write {
    ($dst:expr, $en:literal, $ru:literal $(, $args:expr)* $(,)?) => {
        if $crate::i18n::is_russian() {
            write!($dst, $ru $(, $args)*)
        } else {
            write!($dst, $en $(, $args)*)
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_languages_preserve_validation_and_progress_details() {
        for language in [Language::En, Language::Ru] {
            with_language(language, || {
                let error = crate::engines::validate_url("--exec").unwrap_err();
                assert!(error.contains(if language == Language::En {
                    "cannot start"
                } else {
                    "не может начинаться"
                }));
                let status =
                    crate::engines::aria2_stat("[#abcdef 880KiB/0.9MiB(88%) CN:1 DL:812KiB SD:4]")
                        .unwrap();
                assert!(status.contains("88%") && status.contains("812KiB"));
                assert!(status.contains(if language == Language::En {
                    "seeds 4"
                } else {
                    "сиды 4"
                }));
                assert!(crate::engines::formats()
                    .iter()
                    .all(|(key, _)| key.is_ascii()));
                assert_eq!(
                    crate::setup::SetupStage::Verifying.text(),
                    if language == Language::En {
                        "verifying checksum…"
                    } else {
                        "проверяю контрольную сумму…"
                    }
                );
            });
        }
    }
}
