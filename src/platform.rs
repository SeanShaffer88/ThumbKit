//! Platform layer: the only place Windows APIs are touched.
//! On non-Windows targets everything returns ERR_NOT_SUPPORTED so the
//! pure logic and FFI contract stay testable anywhere.
//!
//! Every extern block MUST carry #[link(name = "...")] naming its import
//! library (advapi32 / shell32 / kernel32 / user32 …). Without it the
//! build fails at LINK time with LNK2019 — check and clippy cannot
//! catch it; CI's real build can.

use crate::ERR_NOT_SUPPORTED;

pub type PResult<T> = Result<T, (i32, String)>;

pub fn not_supported<T>(what: &str) -> PResult<T> {
    Err((
        ERR_NOT_SUPPORTED,
        format!("{what} is only available on Windows"),
    ))
}

#[cfg(windows)]
pub fn example() -> PResult<u32> {
    // Replace with a real call. Pattern for a hand-declared binding:
    #[link(name = "kernel32")]
    extern "system" {
        fn GetCurrentProcessId() -> u32;
    }
    // SAFETY: GetCurrentProcessId takes no arguments and cannot fail.
    Ok(unsafe { GetCurrentProcessId() })
}

#[cfg(not(windows))]
pub fn example() -> PResult<u32> {
    not_supported("example")
}
