use std::{
    path::PathBuf,
    process::{Command, Output},
    time::{SystemTime, UNIX_EPOCH},
};

struct Profile(PathBuf);
impl Profile {
    fn new() -> Self {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("snatch-language-{}-{stamp}", std::process::id()));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_snatch"))
            .args(args)
            .env("LOCALAPPDATA", &self.0)
            .env("APPDATA", &self.0)
            .env("XDG_CONFIG_HOME", &self.0)
            .env("HOME", &self.0)
            .env("USERPROFILE", &self.0)
            .env("NO_COLOR", "1")
            .output()
            .unwrap()
    }
    fn config(&self) -> PathBuf {
        self.0.join("snatch/config.json")
    }
}
impl Drop for Profile {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn language_is_persisted_and_help_does_not_mutate_settings() {
    let profile = Profile::new();
    let initial = profile.run(&["--help"]);
    assert!(initial.status.success());
    let help = String::from_utf8(initial.stdout).unwrap();
    assert!(help.contains("lightweight downloader") && help.contains("--lang"));
    assert!(!profile.config().exists());

    let set = profile.run(&["--lang", "ru"]);
    assert!(
        set.status.success(),
        "{}",
        String::from_utf8_lossy(&set.stderr)
    );
    let before = std::fs::read(profile.config()).unwrap();
    let saved: serde_json::Value = serde_json::from_slice(&before).unwrap();
    assert_eq!(saved["language"], "ru");
    let russian = profile.run(&["--help"]);
    assert!(String::from_utf8(russian.stdout)
        .unwrap()
        .contains("Выбрать и сохранить язык"));
    let english = profile.run(&["--help", "--lang", "en"]);
    assert!(String::from_utf8(english.stdout)
        .unwrap()
        .contains("Set and remember"));
    assert_eq!(std::fs::read(profile.config()).unwrap(), before);
    assert!(!profile.run(&["--lang", "fr"]).status.success());
    assert_eq!(std::fs::read(profile.config()).unwrap(), before);
    assert!(profile.run(&["--lang=en"]).status.success());
    let saved: serde_json::Value =
        serde_json::from_slice(&std::fs::read(profile.config()).unwrap()).unwrap();
    assert_eq!(saved["language"], "en");
}
