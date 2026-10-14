//! What every Unix answers the same way.

#![allow(unsafe_code)]

/// The user this process acts as, the owner its files get.
#[must_use]
pub fn effective_uid() -> u32 {
    // SAFETY: geteuid takes no arguments, touches no memory and cannot fail.
    unsafe { libc::geteuid() }
}
