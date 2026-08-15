//! Which kind of machine the donor is — it changes how its runtime socket is
//! reachable, and therefore what we can promise about port forwarding.
//!
//! Socket *discovery* lives in [`crate::engines`], which probes for every runtime
//! we support. This module is only the classification that gets persisted in the
//! config, because it decides whether automatic port forwarding is available.

use serde::{Deserialize, Serialize};
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostKind {
    /// macOS or Linux exposing a unix domain socket. Fully supported.
    Unix,
    /// Windows with the runtime reachable inside a WSL2 distro.
    ///
    /// Detected and recorded so `doctor` can be honest about it. Bridging the
    /// WSL-internal socket out to the Windows OpenSSH server (which is what our
    /// port reconciler needs) isn't automated: run `runtime-orbit donor setup`
    /// *inside* the WSL distro and it looks like a normal unix donor.
    WindowsWsl,
}

impl fmt::Display for HostKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HostKind::Unix => write!(f, "unix (macOS/Linux)"),
            HostKind::WindowsWsl => write!(f, "windows-wsl2"),
        }
    }
}

impl HostKind {
    /// Whether forwarding the donor's socket as a unix→unix `-L` tunnel works
    /// for this kind of machine in the current build.
    pub fn supports_socket_forward(&self) -> bool {
        matches!(self, HostKind::Unix)
    }
}
