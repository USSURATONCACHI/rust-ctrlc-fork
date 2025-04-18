// Copyright (c) 2017 CtrlC developers
// Licensed under the Apache License, Version 2.0
// <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT
// license <LICENSE-MIT or http://opensource.org/licenses/MIT>,
// at your option. All files in the project carrying such
// notice may not be copied, modified, or distributed except
// according to those terms.

use crate::block_outcome::BlockOutcome;
use crate::error::Error as CtrlcError;
use nix::sys::signal::SigAction;
use nix::sys::signal::SigHandler;
use nix::unistd;
use std::os::fd::BorrowedFd;
use std::os::fd::IntoRawFd;
use std::os::unix::io::RawFd;

static mut PIPE: (RawFd, RawFd) = (-1, -1);

/// Platform specific error type
pub type Error = nix::Error;

/// Platform specific signal type
pub type Signal = nix::sys::signal::Signal;

extern "C" fn os_handler(_: nix::libc::c_int) {
    // Assuming this always succeeds. Can't really handle errors in any meaningful way.
    unsafe {
        let fd = BorrowedFd::borrow_raw(PIPE.1);
        let _ = unistd::write(fd, &[0u8]);
    }
}

// pipe2(2) is not available on macOS, iOS, AIX, Haiku, etc., so we need to use pipe(2) and fcntl(2)
#[inline]
#[cfg(any(
    target_vendor = "apple",
    target_os = "haiku",
    target_os = "aix",
    target_os = "nto",
))]
fn pipe2(flags: nix::fcntl::OFlag) -> nix::Result<(RawFd, RawFd)> {
    use nix::fcntl::{fcntl, FcntlArg, FdFlag, OFlag};

    let pipe = unistd::pipe()?;
    let pipe = (pipe.0.into_raw_fd(), pipe.1.into_raw_fd());

    let mut res = Ok(0);

    if flags.contains(OFlag::O_CLOEXEC) {
        res = res
            .and_then(|_| fcntl(pipe.0, FcntlArg::F_SETFD(FdFlag::FD_CLOEXEC)))
            .and_then(|_| fcntl(pipe.1, FcntlArg::F_SETFD(FdFlag::FD_CLOEXEC)));
    }

    if flags.contains(OFlag::O_NONBLOCK) {
        res = res
            .and_then(|_| fcntl(pipe.0, FcntlArg::F_SETFL(OFlag::O_NONBLOCK)))
            .and_then(|_| fcntl(pipe.1, FcntlArg::F_SETFL(OFlag::O_NONBLOCK)));
    }

    match res {
        Ok(_) => Ok(pipe),
        Err(e) => {
            let _ = unistd::close(pipe.0);
            let _ = unistd::close(pipe.1);
            Err(e)
        }
    }
}

#[inline]
#[cfg(not(any(
    target_vendor = "apple",
    target_os = "haiku",
    target_os = "aix",
    target_os = "nto",
)))]
fn pipe2(flags: nix::fcntl::OFlag) -> nix::Result<(RawFd, RawFd)> {
    let pipe = unistd::pipe2(flags)?;
    Ok((pipe.0.into_raw_fd(), pipe.1.into_raw_fd()))
}

unsafe fn close_pipe() {
    // Try to close the pipes. close() should not fail,
    // but if it does, there isn't much we can do
    let _ = unistd::close(PIPE.1);
    let _ = unistd::close(PIPE.0);
    PIPE = (-1, -1);
}

/// Register os signal handler.
///
/// Must be called before calling [`block_ctrl_c()`](fn.block_ctrl_c.html)
/// and should only be called once.
///
/// # Errors
/// Will return an error if a system error occurred.
///
#[inline]
pub unsafe fn init_os_handler(overwrite: bool) -> Result<(), Error> {
    use nix::fcntl;
    use nix::sys::signal;
    
    PIPE = pipe2(fcntl::OFlag::O_CLOEXEC)?;

    // Make sure we never block on write in the os handler.
    if let Err(e) = fcntl::fcntl(PIPE.1, fcntl::FcntlArg::F_SETFL(fcntl::OFlag::O_NONBLOCK)) {
        close_pipe();
        return Err(e);
    }

    let handler = signal::SigHandler::Handler(os_handler);
    let new_action = sig_handler_to_sig_action(handler);

    let sigint_old = match signal::sigaction(signal::Signal::SIGINT, &new_action) {
        Ok(old) => old,
        Err(e) => {
            close_pipe();
            return Err(e)
        }
    };
    if !overwrite && sigint_old.handler() != signal::SigHandler::SigDfl {
        signal::sigaction(signal::Signal::SIGINT, &sigint_old).unwrap();
        close_pipe();
        return Err(nix::Error::EEXIST);
    }

    #[cfg(feature = "termination")]
    {
        let sigterm_old = match signal::sigaction(signal::Signal::SIGTERM, &new_action) {
            Ok(old) => old,
            Err(e) => {
                signal::sigaction(signal::Signal::SIGINT, &sigint_old).unwrap();
                close_pipe();
                return Err(e);
            }
        };
        if !overwrite && sigterm_old.handler() != signal::SigHandler::SigDfl {
            signal::sigaction(signal::Signal::SIGINT, &sigint_old).unwrap();
            signal::sigaction(signal::Signal::SIGTERM, &sigterm_old).unwrap();
            close_pipe();
            return Err(nix::Error::EEXIST);
        }
        let sighup_old = match signal::sigaction(signal::Signal::SIGHUP, &new_action) {
            Ok(old) => old,
            Err(e) => {
                signal::sigaction(signal::Signal::SIGINT, &sigint_old).unwrap();
                signal::sigaction(signal::Signal::SIGTERM, &sigterm_old).unwrap();
                close_pipe();
                return Err(e);
            }
        };
        if !overwrite && sighup_old.handler() != signal::SigHandler::SigDfl {
            signal::sigaction(signal::Signal::SIGINT, &sigint_old).unwrap();
            signal::sigaction(signal::Signal::SIGTERM, &sigterm_old).unwrap();
            signal::sigaction(signal::Signal::SIGHUP, &sighup_old).unwrap();
            close_pipe();
            return Err(nix::Error::EEXIST);
        }
    }

    Ok(())
}

#[allow(dead_code)]
pub unsafe fn deinit_os_handler() -> Result<(), Error> {
    use nix::sys::signal;
    if !is_handler_init() {
        return Err(nix::Error::ENOENT);
    }

    let new_action = sig_handler_to_sig_action(signal::SigHandler::SigDfl);

    let _ = signal::sigaction(signal::Signal::SIGINT, &new_action);

    #[cfg(feature = "termination")]
    {
        let _ = signal::sigaction(signal::Signal::SIGTERM, &new_action);
        let _ = signal::sigaction(signal::Signal::SIGHUP, &new_action);
    }
    close_pipe();

    Ok(())
}

#[allow(dead_code)]
pub unsafe fn is_handler_init() -> bool {
    return PIPE.0 != -1 && PIPE.1 != -1;
}

unsafe fn sig_handler_to_sig_action(handler: SigHandler) -> SigAction {
    use nix::sys::signal;

    #[cfg(not(target_os = "nto"))]
    let action = signal::SigAction::new(
        handler,
        signal::SaFlags::SA_RESTART,
        signal::SigSet::empty(),
    );
    
    // SA_RESTART is not supported on QNX Neutrino 7.1 and before
    #[cfg(target_os = "nto")]
    let action = signal::SigAction::new(handler, signal::SaFlags::empty(), signal::SigSet::empty());

    action
}

/// Blocks until a Ctrl-C signal is received.
///
/// Must be called after calling [`init_os_handler()`](fn.init_os_handler.html).
///
/// # Errors
/// Will return an error if a system error occurred.
///
#[inline]
pub unsafe fn block_ctrl_c() -> Result<BlockOutcome, CtrlcError> {
    let mut buf = [0u8];

    // TODO: Can we safely convert the pipe fd into a std::io::Read
    // with std::os::unix::io::FromRawFd, this would handle EINTR
    // and everything for us.
    loop {
        let pipe = std::ptr::read_volatile(&raw const PIPE);
        match unistd::read(pipe.0, &mut buf[..]) {
            Ok(1) => break,

            Ok(_) |
            Err(nix::errno::Errno::EBADF)
                => return Ok(BlockOutcome::HandlerRemoved),

            Err(nix::errno::Errno::EINTR) => {}
            Err(e) => return Err(e.into()),
        }
    }

    Ok(BlockOutcome::Awaited)
}
