//! Finding, starting and attaching to Chromium.
//!
//! No daemon: Chromium starts with `--remote-debugging-port=0` and writes the
//! port it picked to `<profile>/DevToolsActivePort`. A running browser with
//! the same profile is reused through that file.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::time::{Duration, Instant, SystemTime};

use tokio::process::{Child, Command};

use crate::cdp::{Cdp, CdpError};
use crate::config::Config;

#[derive(Debug, thiserror::Error)]
pub enum StartError {
    #[error("no Chromium found in PATH; set \"chromium\" in the config")]
    NoChromium,
    #[error("cannot create {path:?}")]
    Dir {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("cannot start {path:?}")]
    Spawn {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("Chromium exited ({status}) before opening its debugging port; is another Chromium using this profile?")]
    Exited { status: ExitStatus },
    #[error("Chromium did not write {path:?} within {secs} s")]
    NoPort { path: PathBuf, secs: u64 },
    #[error("a browser is already running with profile {0:?}; stop it first (rustglance mcp keeps it open)")]
    Busy(PathBuf),
    #[error("waiting for Chromium failed")]
    Wait(#[source] std::io::Error),
    #[error(transparent)]
    Cdp(#[from] CdpError),
}

const NAMES: [&str; 5] = [
    "chromium",
    "chromium-browser",
    "google-chrome-stable",
    "google-chrome",
    "chrome",
];
const START_WAIT: Duration = Duration::from_secs(30);

pub struct Launched {
    pub cdp: Cdp,
    /// Present when this process started the browser and should close it.
    pub child: Option<Child>,
}

fn port_file(profile: &Path) -> PathBuf {
    profile.join("DevToolsActivePort")
}

/// The browser WebSocket URL from `DevToolsActivePort`. A missing or
/// unreadable file means "no running browser", which is the normal case.
async fn read_port(profile: &Path) -> Option<String> {
    let text = tokio::fs::read_to_string(port_file(profile)).await.ok()?;
    let mut lines = text.lines();
    let port: u16 = lines.next()?.trim().parse().ok()?;
    let path = lines.next()?.trim();
    Some(format!("ws://127.0.0.1:{port}{path}"))
}

fn modified(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

fn find_chromium() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    let dirs: Vec<PathBuf> = std::env::split_paths(&path).collect();
    NAMES
        .iter()
        .flat_map(|name| dirs.iter().map(move |dir| dir.join(name)))
        .find(|p| p.is_file())
}

fn command(cfg: &Config, headless: bool) -> Result<(Command, PathBuf), StartError> {
    let exe = match &cfg.chromium {
        Some(p) => p.clone(),
        None => find_chromium().ok_or(StartError::NoChromium)?,
    };
    std::fs::create_dir_all(&cfg.profile).map_err(|source| StartError::Dir {
        path: cfg.profile.clone(),
        source,
    })?;
    let mut dir = OsString::from("--user-data-dir=");
    dir.push(&cfg.profile);
    let mut c = Command::new(&exe);
    c.arg(dir)
        .args([
            "--no-first-run",
            "--no-default-browser-check",
            "--disable-background-timer-throttling",
            "--disable-backgrounding-occluded-windows",
            "--disable-renderer-backgrounding",
        ])
        .arg(format!("--window-size={},{}", cfg.window.0, cfg.window.1))
        .args(&cfg.chromium_args)
        // stdout carries MCP; Chromium's own output must not reach it.
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if headless {
        c.arg("--headless=new");
        if !cfg.webgl {
            c.arg("--disable-3d-apis");
        }
    }
    Ok((c, exe))
}

/// Reuses a browser already running with the profile, else starts one.
pub async fn attach_or_launch(cfg: &Config) -> Result<Launched, StartError> {
    let timeout = Duration::from_millis(cfg.cdp_timeout_ms);
    if let Some(url) = read_port(&cfg.profile).await {
        // A stale file from a crashed browser fails to connect; then start fresh.
        if let Ok(cdp) = Cdp::connect(&url, timeout).await {
            return Ok(Launched { cdp, child: None });
        }
    }
    let file = port_file(&cfg.profile);
    let before = modified(&file);
    let (mut cmd, exe) = command(cfg, cfg.headless)?;
    // A started browser closes when this process drops it (error or panic);
    // a SIGKILL of this process still leaves it running.
    cmd.arg("--remote-debugging-port=0")
        .arg("about:blank")
        .kill_on_drop(true);
    let mut child = cmd.spawn().map_err(|source| StartError::Spawn { path: exe, source })?;
    let deadline = Instant::now() + START_WAIT;
    loop {
        let now = modified(&file);
        if now.is_some() && now != before {
            if let Some(url) = read_port(&cfg.profile).await {
                let cdp = Cdp::connect(&url, timeout).await?;
                return Ok(Launched {
                    cdp,
                    child: Some(child),
                });
            }
        }
        if let Some(status) = child.try_wait().map_err(StartError::Wait)? {
            return Err(StartError::Exited { status });
        }
        if Instant::now() >= deadline {
            if let Err(e) = child.kill().await {
                eprintln!("rustglance: cannot stop Chromium after a failed start: {e}");
            }
            return Err(StartError::NoPort {
                path: file,
                secs: START_WAIT.as_secs(),
            });
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Opens a visible window with the profile so a human can log in, and waits
/// until the window is closed. Agents never log in.
pub async fn login(cfg: &Config) -> Result<(), StartError> {
    let timeout = Duration::from_millis(cfg.cdp_timeout_ms);
    if let Some(url) = read_port(&cfg.profile).await {
        if Cdp::connect(&url, timeout).await.is_ok() {
            return Err(StartError::Busy(cfg.profile.clone()));
        }
    }
    let (mut cmd, exe) = command(cfg, false)?;
    if !cfg.start_url.is_empty() {
        cmd.arg(&cfg.start_url);
    }
    let mut child = cmd.spawn().map_err(|source| StartError::Spawn { path: exe, source })?;
    eprintln!("rustglance: log in in the browser window, then close it");
    child.wait().await.map_err(StartError::Wait)?;
    Ok(())
}
