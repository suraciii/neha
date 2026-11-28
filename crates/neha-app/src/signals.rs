//! The one stop signal the daemons share.
//!
//! `neha serve` and `neha computer run` stop on Ctrl-C, and on Unix also on
//! SIGTERM so systemd, Docker, and the desktop shell can ask for a clean stop
//! instead of killing the process. The handler streams are registered when
//! [`Stop::install`] runs, before the process advertises readiness, so a
//! signal that arrives in that window is still queued and handled.

use std::fmt;

/// Why a daemon should stop.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StopReason {
    /// The user pressed Ctrl-C.
    CtrlC,
    /// A service manager sent SIGTERM.
    Sigterm,
}

impl fmt::Display for StopReason {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CtrlC => formatter.write_str("ctrl_c"),
            Self::Sigterm => formatter.write_str("sigterm"),
        }
    }
}

/// The registered stop signals of one process.
pub struct Stop {
    #[cfg(unix)]
    interrupt: tokio::signal::unix::Signal,
    #[cfg(unix)]
    terminate: tokio::signal::unix::Signal,
}

impl Stop {
    /// Registers Ctrl-C and, on Unix, SIGTERM.
    ///
    /// The Unix streams are created here, so the signal handlers exist before
    /// the caller binds a listener or prints its address. A signal delivered
    /// between this call and [`Stop::wait`] is held by the stream.
    pub fn install() -> std::io::Result<Self> {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};
            Ok(Self {
                interrupt: signal(SignalKind::interrupt())?,
                terminate: signal(SignalKind::terminate())?,
            })
        }

        #[cfg(not(unix))]
        {
            Ok(Self {})
        }
    }

    /// Waits for the first stop request.
    pub async fn wait(&mut self) -> std::io::Result<StopReason> {
        #[cfg(unix)]
        {
            tokio::select! {
                _ = self.interrupt.recv() => Ok(StopReason::CtrlC),
                _ = self.terminate.recv() => Ok(StopReason::Sigterm),
            }
        }

        #[cfg(not(unix))]
        {
            tokio::signal::ctrl_c().await?;
            Ok(StopReason::CtrlC)
        }
    }
}
