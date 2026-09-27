pub mod config;
pub mod engines;
pub mod setup;
pub mod tools;
pub mod ui;

pub const BANNER: &str = r#"
 ____    __  __  ______  ______  ____     __  __
/\  _`\ /\ \/\ \/\  _  \/\__  _\/\  _`\  /\ \/\ \
\ \,\L\_\ \ `\\ \ \ \L\ \/_/\ \/\ \ \/\_\\ \ \_\ \
 \/_\__ \\ \ , ` \ \  __ \ \ \ \ \ \ \/_/_\ \  _  \
   /\ \L\ \ \ \`\ \ \ \/\ \ \ \ \ \ \ \L\ \\ \ \ \ \
   \ `\____\ \_\ \_\ \_\ \_\ \ \_\ \ \____/ \ \_\ \_\
    \/_____/\/_/\/_/\/_/\/_/  \/_/  \/___/   \/_/\/_/
                     yt-dlp + aria2c ultimate combine
                             by rercon prod.
"#;

pub const TELEGRAM_URL: &str = "https://t.me/rercon";

pub fn outln(s: impl AsRef<str>) {
    println!("{}", s.as_ref());
}

pub fn errln(s: impl AsRef<str>) {
    eprintln!("{}", s.as_ref());
}
