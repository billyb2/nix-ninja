//! Runtime-linkage repair for task outputs, selected by the platform that runs
//! the task.
//!
//! Both platforms have the same problem: a build rule links against files that
//! live in the build directory, and the resulting artifact is then copied to a
//! content-addressed store path where the build directory does not exist. Each
//! format solves it differently, so the repair lives in a per-format module.

use crate::derived_file::DerivedFile;
use crate::elf_rpath;
use crate::macho;
use crate::platform::{host_os, Os};
use anyhow::Result;
use std::path::Path;

/// Makes every output able to find its dependencies once it lives in the store.
///
/// `build_dir` is the directory the task built in; paths inside it are
/// sandbox-local and must not be relied on at runtime.
pub fn fix_linkage(store_dir: &Path, build_dir: &Path, outputs: &[DerivedFile]) -> Result<()> {
    match host_os() {
        Os::Linux => elf_rpath::fix_rpaths(store_dir, outputs),
        Os::Darwin => macho::fix_linkage(store_dir, build_dir, outputs),
    }
}
