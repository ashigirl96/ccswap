//! Finding a program on `PATH`.

use std::path::{Path, PathBuf};

use crate::env::Environment;

/// `shutil.which`: the first executable regular file named `name` on `PATH`.
pub fn which(env: &Environment, name: &str) -> Option<PathBuf> {
    let path = env.get("PATH")?.clone();
    std::env::split_paths(&path)
        .filter(|dir| !dir.as_os_str().is_empty())
        .map(|dir| dir.join(name))
        .find(|candidate| is_executable_file(candidate))
}

fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    let Ok(c_path) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
    // SAFETY: c_path is a valid NUL-terminated string for the call.
    unsafe { libc::access(c_path.as_ptr(), libc::X_OK) == 0 }
}
