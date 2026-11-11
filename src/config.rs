//! JSON configuration. Everything has a default; the file only names what differs.
//!
//! The browser profile (cookies, logins) lives at `profile`, outside any
//! repository. Evidence (journal, screenshots, downloads) goes to `evidence`.

use std::path::{Path, PathBuf};

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Config {
    /// Chromium user data dir. Holds the logged-in session.
    pub profile: PathBuf,
    /// Chromium binary; searched in PATH when absent.
    pub chromium: Option<PathBuf>,
    pub headless: bool,
    /// WebGL without a window. Off by default: headless Chromium draws WebGL
    /// on the CPU, and an animated canvas can keep several cores busy.
    pub webgl: bool,
    /// Opened at start when the tab is blank; base for relative paths.
    pub start_url: String,
    /// Journal, screenshots and downloads.
    pub evidence: PathBuf,
    /// The only folder `upload` may read from. Uploads are off without it.
    pub upload_dir: Option<PathBuf>,
    /// URL globs whose non-GET requests pass the write check.
    pub write_block: Vec<String>,
    /// URL globs where writes are allowed anyway.
    pub write_allow: Vec<String>,
    /// Allow every write. Off by default.
    pub writes: bool,
    /// A URL part that means "logged out", e.g. `/login`.
    pub logged_out_url: Option<String>,
    pub cdp_timeout_ms: u64,
    /// Longest wait for the page to go quiet after an action.
    pub settle_ms: u64,
    pub download_ms: u64,
    /// Retries for 429 and 503 answers.
    pub retries: u32,
    /// Largest `get` body handed to the agent, in bytes of JSON. A larger body
    /// is cut to fit (marked `truncated`) and saved whole to evidence; MCP
    /// clients cut long tool output on their own and break the JSON.
    /// 0 hands every body over whole.
    pub get_max_bytes: usize,
    /// Live view on 127.0.0.1 at this port. Off when absent.
    pub view_port: Option<u16>,
    /// Window size in CSS pixels.
    pub window: (u32, u32),
    /// Extra Chromium flags.
    pub chromium_args: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        let data = std::env::var_os("HOME")
            .map(|h| PathBuf::from(h).join(".local/share/rustglance"))
            .unwrap_or_else(|| PathBuf::from(".rustglance"));
        Self {
            profile: data.join("profile"),
            chromium: None,
            headless: true,
            webgl: false,
            start_url: String::new(),
            evidence: data.join("evidence"),
            upload_dir: None,
            write_block: vec!["*".to_string()],
            write_allow: Vec::new(),
            writes: false,
            logged_out_url: None,
            cdp_timeout_ms: 15_000,
            settle_ms: 8_000,
            download_ms: 120_000,
            retries: 4,
            get_max_bytes: 16_384,
            view_port: None,
            window: (1280, 900),
            chromium_args: Vec::new(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("cannot read config {path:?}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid config {path:?}")]
    Parse {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("invalid config: {0}")]
    Invalid(String),
}

impl Config {
    /// Reads `path`, else `$RUSTGLANCE_CONFIG`, else uses the defaults.
    pub fn load(path: Option<&Path>) -> Result<Self, ConfigError> {
        let path = path
            .map(Path::to_path_buf)
            .or_else(|| std::env::var_os("RUSTGLANCE_CONFIG").map(PathBuf::from));
        let cfg: Config = match &path {
            None => Config::default(),
            Some(p) => {
                let text = std::fs::read_to_string(p).map_err(|source| ConfigError::Read {
                    path: p.clone(),
                    source,
                })?;
                serde_json::from_str(&text).map_err(|source| ConfigError::Parse {
                    path: p.clone(),
                    source,
                })?
            }
        };
        cfg.validate()
    }

    fn validate(mut self) -> Result<Self, ConfigError> {
        let bad = |m: &str| Err(ConfigError::Invalid(m.to_string()));
        if self.cdp_timeout_ms == 0 || self.settle_ms == 0 || self.download_ms == 0 {
            return bad("cdp_timeout_ms, settle_ms and download_ms must be above 0");
        }
        // The settle wait runs inside one CDP command, so it must end first.
        if self.settle_ms.saturating_add(1_000) > self.cdp_timeout_ms {
            return bad("settle_ms must be at least 1000 below cdp_timeout_ms");
        }
        if self.window.0 == 0 || self.window.1 == 0 {
            return bad("window must be two numbers above 0");
        }
        if self.write_block.is_empty() && !self.writes {
            return bad("write_block is empty, so no write would be checked; set writes: true to allow all");
        }
        let abs = |p: &Path| std::path::absolute(p).map_err(|e| ConfigError::Invalid(format!("path {p:?}: {e}")));
        self.profile = abs(&self.profile)?;
        self.evidence = abs(&self.evidence)?;
        if let Some(dir) = &self.upload_dir {
            let real = dir
                .canonicalize()
                .map_err(|e| ConfigError::Invalid(format!("upload_dir {dir:?}: {e}")))?;
            self.upload_dir = Some(real);
        }
        // Paths travel to the browser as JSON strings, so they must be UTF-8.
        for p in [Some(&self.profile), Some(&self.evidence), self.upload_dir.as_ref()]
            .into_iter()
            .flatten()
        {
            if p.to_str().is_none() {
                return Err(ConfigError::Invalid(format!("path {p:?} is not UTF-8")));
            }
        }
        Ok(self)
    }
}
