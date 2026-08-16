#![allow(unsafe_code)]

use std::os::fd::AsRawFd as _;
use std::path::Path;

use crate::PlatformError;

unsafe extern "C" {
    fn hd_commoncrypto_sha256_fd(
        fd: libc::c_int,
        digest: *mut u8,
        error_number: *mut libc::c_int,
    ) -> libc::c_int;
}

pub(crate) fn sha256_regular_nofollow(path: &Path) -> Result<[u8; 32], PlatformError> {
    let file = crate::open_regular_read_nofollow(path)?;
    let mut digest = [0_u8; 32];
    let mut error_number = 0;
    // SAFETY: `file` owns a valid open descriptor for the whole synchronous call, `digest` points
    // to 32 writable bytes, and `error_number` points to one writable C int. The C adapter only
    // reads from the descriptor and writes within those two fixed output buffers.
    let result = unsafe {
        hd_commoncrypto_sha256_fd(file.as_raw_fd(), digest.as_mut_ptr(), &raw mut error_number)
    };
    if result == 1 {
        Ok(digest)
    } else {
        let source = if error_number == 0 {
            std::io::Error::other("CommonCrypto SHA-256 failed")
        } else {
            std::io::Error::from_raw_os_error(error_number)
        };
        Err(PlatformError::Io {
            operation: "hash regular file with CommonCrypto",
            path: path.to_owned(),
            source,
        })
    }
}
