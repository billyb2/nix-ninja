//! Platform identity and executable-format selection.
//!
//! Every task derivation declares the platform that must build it. nix-ninja
//! only supports native builds today: the derivations it writes are executed by
//! the same machine that evaluated the Ninja graph, so the platform is the one
//! nix-ninja itself was built for.

/// The operating-system family of a Nix system string.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Os {
    /// ELF objects, `patchelf -rpath` for runtime linking.
    Linux,
    /// Mach-O objects, `install_name_tool` for runtime linking.
    Darwin,
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
compile_error!("nix-ninja only supports Linux and Darwin");

/// The OS this binary was built for.
pub fn host_os() -> Os {
    if cfg!(target_os = "macos") {
        Os::Darwin
    } else {
        Os::Linux
    }
}

/// The Nix system string for this binary, e.g. `aarch64-darwin`.
pub fn host_system() -> String {
    let arch = std::env::consts::ARCH;
    let os = match host_os() {
        Os::Linux => "linux",
        Os::Darwin => "darwin",
    };
    format!("{arch}-{os}")
}
