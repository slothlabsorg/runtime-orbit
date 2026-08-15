//! The detached `forward` worker (hidden). Runs only the reconciler loop; the
//! parent `runtime-orbit up` has already switched context and opened the SSH master.

use anyhow::Result;

use crate::config;
use crate::forwarder;

pub async fn run() -> Result<()> {
    let cfg = config::require_linked()?;
    let socket = config::local_docker_socket()?;
    // Loop body owns reconnect-on-stream-end via the caller restarting if needed;
    // here we run until the event stream ends or the socket dies.
    forwarder::run(&cfg, &socket).await
}
