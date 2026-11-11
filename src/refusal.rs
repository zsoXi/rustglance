//! What a tool call returns when it cannot do its job: a class the agent can
//! branch on, a reason, and the next step that is likely to work.

use serde::Serialize;

use crate::cdp::CdpError;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Class {
    /// Arguments missing, unknown or of the wrong type.
    BadArgs,
    /// No element, file or option matches.
    NotFound,
    /// The ref points to an element that is gone; the page changed.
    Stale,
    /// Another element lies over the target and would get the click.
    Covered,
    /// The site answered 401 or redirected to its login page.
    LoggedOut,
    /// The site kept answering 429 or 503 after retries.
    RateLimited,
    /// A file path outside the configured upload folder, or uploads are off.
    OutsideFolder,
    /// A wait ran out (download, condition, request).
    Timeout,
    /// The page stopped answering; the tab was replaced.
    Wedged,
    /// The page threw an exception while running the action.
    PageError,
    /// The site answered a tool's own request with an error status.
    Site,
    /// The browser refused a command or the connection broke.
    Browser,
    /// A human has taken over the browser in the live view.
    HumanControl,
    /// The target is a password field; only a human types there.
    Password,
    /// A local file could not be read or written.
    Io,
}

#[derive(Debug, Clone, Serialize, thiserror::Error)]
#[error("{class:?}: {reason}")]
pub struct Refusal {
    #[serde(rename = "error")]
    pub class: Class,
    pub reason: String,
    pub next: String,
}

impl Refusal {
    pub fn new(class: Class, reason: impl Into<String>, next: impl Into<String>) -> Self {
        Self {
            class,
            reason: reason.into(),
            next: next.into(),
        }
    }

    pub fn bad_args(reason: impl Into<String>) -> Self {
        Self::new(
            Class::BadArgs,
            reason,
            "fix the arguments; tools/list shows each tool's schema",
        )
    }

    pub fn stale(r: &str) -> Self {
        Self::new(
            Class::Stale,
            format!("ref {r} is gone; the page changed"),
            "call tree and use a fresh ref",
        )
    }

    pub fn io(what: impl std::fmt::Display, e: &std::io::Error) -> Self {
        Self::new(
            Class::Io,
            format!("{what}: {e}"),
            "check the evidence and upload folders",
        )
    }
}

// The agent sees only the message; the cause chain ends here on purpose.
impl From<CdpError> for Refusal {
    fn from(e: CdpError) -> Self {
        match e {
            CdpError::Timeout { .. } => Self::new(Class::Wedged, e.to_string(), "repeat the call"),
            CdpError::Protocol { .. } => Self::new(
                Class::Browser,
                e.to_string(),
                "check the arguments; call tree to refresh refs",
            ),
            CdpError::Closed | CdpError::Connect { .. } => Self::new(
                Class::Browser,
                e.to_string(),
                "restart rustglance; the browser connection is gone",
            ),
        }
    }
}
