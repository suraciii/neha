//! The Computer's integration tests, one binary.

// The local runtime's tests build a runtime for the machine they run on: they
// use unix symlinks, permissions, and sandbox helpers, so they only exist on
// unix. Windows has its own cases in `windows_shell`.
#[cfg(unix)]
mod local;

#[cfg(windows)]
mod windows_shell;
