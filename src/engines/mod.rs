use crate::{tr, tr_format};
use std::borrow::Cow;
use std::ffi::OsString;
use std::io::BufRead;
use std::path::{Path, PathBuf};

use fs4::{FileExt, TryLockError};
use sha2::{Digest, Sha256};

use crate::config::{config_dir, home_dir};
use crate::tools::{which, Toolchain};
use crate::ui::clip;

/// Downloads running at once unless the user asks for another number: the
/// CLI's `-j` default and the GUI's concurrency limit are this one value.
pub const DEFAULT_JOBS: u8 = 3;

mod aria2;
mod command;
mod extras;
mod kinds;
mod output;
mod preflight;
mod url;
pub use aria2::*;
pub use command::*;
pub use extras::*;
pub use kinds::*;
pub use output::*;
pub use preflight::*;
pub use url::*;

#[derive(Clone)]
pub struct Job {
    pub engine: Engine,
    pub url: String,
    /// Resolved once before locking. Queues/retries/resume retain this exact
    /// target; ownership still fingerprints the original URL.
    pub download_name: Option<crate::links::DownloadName>,
    pub out_dir: PathBuf,
    pub fmt: Format,
    pub cookies_browser: Option<String>,
}

#[cfg(test)]
mod tests;
