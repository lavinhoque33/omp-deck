//! Shared harness of the live gateway tests; each test binary uses part of it.
#![allow(dead_code)]

pub mod device;
pub mod fake_model;

/// Make a selected real binary visible to fixture admission and gateway Omp::detect.
pub fn test_omp_override() -> Option<tempfile::TempDir> {
    let binary = std::env::var_os("OMPDECK_TEST_OMP")?;
    let binary = std::fs::canonicalize(binary).expect("OMPDECK_TEST_OMP must name a real binary");
    assert!(binary.is_file(), "OMPDECK_TEST_OMP must name a file");
    let dir = tempfile::tempdir().expect("omp override directory");
    let target = dir
        .path()
        .join(if cfg!(windows) { "omp.exe" } else { "omp" });
    #[cfg(unix)]
    std::os::unix::fs::symlink(&binary, &target).expect("link selected omp");
    #[cfg(windows)]
    std::fs::copy(&binary, &target).expect("copy selected omp");
    let mut paths = vec![dir.path().to_path_buf()];
    paths.extend(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    ));
    // SAFETY: each fixture's sole test calls this before creating any threads.
    unsafe {
        std::env::set_var("PATH", std::env::join_paths(paths).expect("override PATH"));
    }
    Some(dir)
}
