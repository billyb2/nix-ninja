//! Mach-O runtime linkage repair for Darwin build outputs.
//!
//! The Linux counterpart ([`crate::elf_rpath`]) fixes `DT_RPATH`, which is an
//! indirection: a binary finds its libraries by searching a list of
//! directories. Mach-O has no such indirection. A dylib records its own
//! install name in `LC_ID_DYLIB`, and every dependent records the exact path it
//! was linked against in an `LC_LOAD_DYLIB`. Meson emits build-tree values for
//! both, e.g. `@rpath/libword1.dylib` plus an `LC_RPATH` pointing at the build
//! directory, and neither survives being copied to `$out`.
//!
//! So before an output leaves the build directory we rewrite:
//!
//! * `LC_ID_DYLIB` — a build-directory path is replaced by the output's
//!   build-tree-relative name, which dependents resolve through the build
//!   directory without needing an rpath of their own. Names that are already
//!   `@rpath`/`@loader_path`-relative, plain relative, or absolute outside the
//!   build directory are kept. A store path is deliberately never introduced
//!   here: Nix rewrites occurrences of an output's placeholder hash inside the
//!   output bytes when it registers a content-addressed result, and any such
//!   rewrite after this point would invalidate the signature we apply below;
//! * `@rpath/…`, `@loader_path/…` and relative loads — to the store path the
//!   build-directory symlink resolves to (this is the same resolution the
//!   Linux path performs with `patchelf --print-needed` + `$ORIGIN` expansion).
//!   Those paths belong to *inputs*, which are already concrete store paths, so
//!   Nix does not rewrite them;
//! * `LC_RPATH` entries that point inside the build directory — dropped, since
//!   they only ever name a sandbox path that will not exist at runtime.
//!
//! Editing load commands invalidates the ad-hoc code signature the Darwin
//! linker attaches, and arm64 macOS refuses to map or execute code with an
//! invalid signature, so edited files are re-signed (`codesign -f -s -`).

use crate::derived_file::DerivedFile;
use anyhow::{anyhow, bail, Context as _, Result};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const MH_MAGIC_64: u32 = 0xfeed_facf;
const MH_CIGAM_64: u32 = 0xcffa_edfe;
const FAT_MAGIC: u32 = 0xcafe_babe;
const FAT_CIGAM: u32 = 0xbeba_feca;

const MH_EXECUTE: u32 = 0x2;
const MH_DYLIB: u32 = 0x6;
const MH_BUNDLE: u32 = 0x8;

const LC_LOAD_DYLIB: u32 = 0xc;
const LC_ID_DYLIB: u32 = 0xd;
const LC_LOAD_WEAK_DYLIB: u32 = 0x8000_0018;
const LC_REEXPORT_DYLIB: u32 = 0x8000_001f;
const LC_LAZY_LOAD_DYLIB: u32 = 0x20;
const LC_LOAD_UPWARD_DYLIB: u32 = 0x8000_0023;
const LC_RPATH: u32 = 0x8000_001c;
const LC_CODE_SIGNATURE: u32 = 0x1d;

/// Size of `mach_header_64`.
const MACH_HEADER_64_SIZE: usize = 32;

/// Load commands that name a dylib dependency.
fn is_dylib_load(cmd: u32) -> bool {
    matches!(
        cmd,
        LC_LOAD_DYLIB
            | LC_LOAD_WEAK_DYLIB
            | LC_REEXPORT_DYLIB
            | LC_LAZY_LOAD_DYLIB
            | LC_LOAD_UPWARD_DYLIB
    )
}

/// The parts of a Mach-O header that linkage repair needs.
struct MachO {
    filetype: u32,
    /// Current `LC_ID_DYLIB` install name, if the file is a dylib.
    id: Option<String>,
    /// Absolute, `@rpath`-relative or `@loader_path`-relative load paths.
    deps: Vec<String>,
    /// `LC_RPATH` search paths, unexpanded.
    rpaths: Vec<String>,
    /// Whether the linker left an `LC_CODE_SIGNATURE` behind.
    has_code_signature: bool,
}

/// Reads the Mach-O headers of `data`.
///
/// Returns `None` for files that are not thin 64-bit Mach-O. Universal ("fat")
/// binaries are reported as skipped rather than rewritten, because the build
/// products of a single-arch Nix build are always thin.
fn parse(data: &[u8], path: &Path) -> Result<Option<MachO>> {
    if data.len() < 4 {
        return Ok(None);
    }

    let magic = u32::from_le_bytes(data[..4].try_into().unwrap());
    if magic == FAT_MAGIC || magic == FAT_CIGAM {
        println!(
            "nix-ninja-task: Skipping universal Mach-O output {} (only thin binaries are rewritten)",
            path.display()
        );
        return Ok(None);
    }
    if magic == MH_CIGAM_64 {
        bail!(
            "{} is a big-endian Mach-O binary, which nix-ninja cannot rewrite",
            path.display()
        );
    }
    if magic != MH_MAGIC_64 {
        return Ok(None);
    }
    if data.len() < MACH_HEADER_64_SIZE {
        bail!("{} is truncated: incomplete mach_header_64", path.display());
    }

    let filetype = u32::from_le_bytes(data[12..16].try_into().unwrap());
    let ncmds = u32::from_le_bytes(data[16..20].try_into().unwrap());
    let sizeofcmds = u32::from_le_bytes(data[20..24].try_into().unwrap()) as usize;

    let commands_end = MACH_HEADER_64_SIZE
        .checked_add(sizeofcmds)
        .filter(|end| *end <= data.len())
        .ok_or_else(|| {
            anyhow!(
                "{}: load commands extend past the end of the file",
                path.display()
            )
        })?;

    let mut macho = MachO {
        filetype,
        id: None,
        deps: Vec::new(),
        rpaths: Vec::new(),
        has_code_signature: false,
    };

    let mut offset = MACH_HEADER_64_SIZE;
    for _ in 0..ncmds {
        if offset + 8 > commands_end {
            bail!(
                "{}: truncated load command at offset {offset}",
                path.display()
            );
        }
        let cmd = u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap());
        let cmdsize = u32::from_le_bytes(data[offset + 4..offset + 8].try_into().unwrap()) as usize;
        if cmdsize < 8 || offset + cmdsize > commands_end {
            bail!(
                "{}: malformed load command (cmd {cmd:#x}, size {cmdsize})",
                path.display()
            );
        }

        if is_dylib_load(cmd) || cmd == LC_ID_DYLIB {
            let name = lc_str(data, offset, cmdsize, path)?;
            if cmd == LC_ID_DYLIB {
                macho.id = Some(name);
            } else {
                macho.deps.push(name);
            }
        } else if cmd == LC_RPATH {
            macho.rpaths.push(lc_str(data, offset, cmdsize, path)?);
        } else if cmd == LC_CODE_SIGNATURE {
            macho.has_code_signature = true;
        }

        offset += cmdsize;
    }

    macho.deps.sort();
    macho.deps.dedup();

    Ok(Some(macho))
}

/// Reads the `lc_str` at offset 8 of a `dylib_command`/`rpath_command`.
fn lc_str(data: &[u8], cmd_offset: usize, cmdsize: usize, path: &Path) -> Result<String> {
    if cmdsize < 12 {
        bail!(
            "{}: load command is too small to hold a string offset (size {cmdsize})",
            path.display()
        );
    }
    let str_offset =
        u32::from_le_bytes(data[cmd_offset + 8..cmd_offset + 12].try_into().unwrap()) as usize;
    if str_offset >= cmdsize {
        bail!(
            "{}: load command string offset {str_offset} is outside the command",
            path.display()
        );
    }
    let start = cmd_offset + str_offset;
    let raw = &data[start..cmd_offset + cmdsize];
    let end = raw.iter().position(|byte| *byte == 0).unwrap_or(raw.len());
    Ok(String::from_utf8_lossy(&raw[..end]).into_owned())
}

/// The load commands that need rewriting for one output file.
#[derive(Default)]
struct Edits {
    /// New `LC_ID_DYLIB` install name.
    id: Option<String>,
    /// `(old, new)` pairs for `install_name_tool -change`.
    changes: Vec<(String, String)>,
    /// `LC_RPATH` entries that only ever named the build directory.
    removed_rpaths: Vec<String>,
}

impl Edits {
    fn is_empty(&self) -> bool {
        self.id.is_none() && self.changes.is_empty() && self.removed_rpaths.is_empty()
    }
}

/// How a recorded dependency path resolves inside the build directory.
enum Resolution {
    /// The build-directory symlink resolves to this path in the Nix store.
    Store(PathBuf),
    /// A platform or store path that needs no rewriting.
    Keep,
    /// An `@rpath` reference that no recorded rpath resolved.
    Unresolved,
}

/// Rewrites Mach-O runtime linkage for every Darwin output of a ninja task.
///
/// `build_dir` is the task's build directory and the process working directory:
/// paths inside it are sandbox-local and must not survive into the output.
pub fn fix_linkage(store_dir: &Path, build_dir: &Path, outputs: &[DerivedFile]) -> Result<()> {
    let store_dir_str = store_dir.to_string_lossy().into_owned();
    // Output paths are relative to the build directory the task ran in. Make it
    // absolute without resolving symlinks, so the expansion of `@loader_path`
    // rpaths produces the same absolute paths the linker recorded.
    let build_dir = std::path::absolute(build_dir)
        .with_context(|| format!("making build directory {} absolute", build_dir.display()))?;

    for output in outputs {
        let data = fs::read(&output.build_path).with_context(|| {
            format!(
                "reading output {} to inspect its Mach-O headers",
                output.build_path.display()
            )
        })?;

        let Some(macho) = parse(&data, &output.build_path)? else {
            continue;
        };

        if !matches!(macho.filetype, MH_EXECUTE | MH_DYLIB | MH_BUNDLE) {
            continue;
        }

        let edits = plan_edits(&macho, &output.build_path, &build_dir, &store_dir_str)?;

        if edits.is_empty() {
            continue;
        }

        apply_edits(&edits, &output.build_path)?;

        if macho.has_code_signature {
            resign(&output.build_path)?;
        }

        println!(
            "nix-ninja-task: Fixed Mach-O linkage for {}",
            output.build_path.display()
        );
    }

    Ok(())
}

fn plan_edits(
    macho: &MachO,
    build_path: &Path,
    build_dir: &Path,
    store_dir: &str,
) -> Result<Edits> {
    let mut edits = Edits::default();

    let image_dir = build_path
        .parent()
        .map_or_else(|| build_dir.to_path_buf(), |parent| build_dir.join(parent));
    let search_dirs: Vec<PathBuf> = macho
        .rpaths
        .iter()
        .map(|rpath| expand_rpath(rpath, &image_dir))
        .collect();

    // A dylib must advertise a name its dependents can resolve, without naming
    // the store path we are about to be copied to: that path contains our own
    // content hash, and Nix rewrites those bytes after the builder exits.
    if macho.filetype == MH_DYLIB {
        if let Some(id) = install_name(macho.id.as_deref(), build_path, build_dir) {
            edits.id = Some(id);
        }
    }

    for dep in &macho.deps {
        if dep.starts_with(store_dir) {
            continue;
        }
        match resolve_dep(dep, &image_dir, &search_dirs, build_dir, store_dir)? {
            Resolution::Store(resolved) => {
                let resolved = resolved.to_string_lossy().into_owned();
                if resolved != *dep {
                    edits.changes.push((dep.clone(), resolved));
                }
            }
            Resolution::Keep => {}
            Resolution::Unresolved => {
                bail!(
                    "Unable to resolve Mach-O dependency {dep} of {} to a store path. \
                     The build directory is not a valid runtime location, so the output would \
                     be unusable; refusing to write it.",
                    build_path.display()
                );
            }
        }
    }

    // Every remaining load path is absolute now, so an rpath that names a
    // directory which will not exist at runtime is dead weight. That is any
    // rpath inside the build directory, and any absolute rpath that is already
    // dangling (it can only have pointed into the sandbox).
    for rpath in &macho.rpaths {
        let expanded = expand_rpath(rpath, &image_dir);
        if expanded.is_absolute() && (expanded.starts_with(build_dir) || !expanded.exists()) {
            edits.removed_rpaths.push(rpath.clone());
        }
    }

    edits.changes.sort();
    edits.changes.dedup();

    Ok(edits)
}

/// The install name a dylib should carry after the task finishes.
///
/// Returns `None` when the existing name is already acceptable. Only a name that
/// points inside the build directory has to change: that path stops existing
/// once the file is copied out, while a graph-relative name keeps working for
/// every dependent that links against this file through the build tree. The
/// replacement is never a store path, because it is embedded in the output bytes
/// and Nix rewrites the output's own placeholder hash there (see module docs).
fn install_name(current: Option<&str>, build_path: &Path, build_dir: &Path) -> Option<String> {
    if let Some(current) = current {
        if !Path::new(current).starts_with(build_dir) {
            // `@rpath/…`, `@loader_path/…`, a relative name and an absolute
            // name outside the build directory all stay as the link step left
            // them.
            return None;
        }
    }

    Some(build_path.to_string_lossy().into_owned())
}

/// Expands `@loader_path`/`@executable_path` in an `LC_RPATH` entry.
///
/// Both are relative to the file's own directory for build outputs: dyld treats
/// an executable's own directory as `@executable_path`, and a dylib loaded from
/// the same tree has `@loader_path` there too.
fn expand_rpath(rpath: &str, image_dir: &Path) -> PathBuf {
    let image_dir = image_dir.to_string_lossy();
    PathBuf::from(
        rpath
            .replace("@loader_path", &image_dir)
            .replace("@executable_path", &image_dir),
    )
}

fn resolve_dep(
    name: &str,
    image_dir: &Path,
    search_dirs: &[PathBuf],
    build_dir: &Path,
    store_dir: &str,
) -> Result<Resolution> {
    let candidates: Vec<PathBuf> = if let Some(rel) = name.strip_prefix("@rpath/") {
        search_dirs
            .iter()
            .map(|dir| dir.join(rel))
            .chain(std::iter::once(image_dir.join(rel)))
            .collect()
    } else if let Some(rel) = name
        .strip_prefix("@loader_path/")
        .or_else(|| name.strip_prefix("@executable_path/"))
    {
        vec![image_dir.join(rel)]
    } else if Path::new(name).is_absolute() {
        vec![PathBuf::from(name)]
    } else {
        // A relative name is resolved by dyld against the process working
        // directory (or `DYLD_LIBRARY_PATH`, which a Nix build does not set).
        // The link ran with the build directory as its working directory, so
        // anchor the name there as well as at the image and the recorded
        // rpaths; anything that resolves is rewritten to a store path.
        search_dirs
            .iter()
            .map(|dir| dir.join(name))
            .chain([build_dir.join(name), image_dir.join(name)])
            .collect()
    };

    for candidate in candidates {
        if !candidate.exists() {
            continue;
        }
        let Ok(canonical) = fs::canonicalize(&candidate) else {
            continue;
        };
        if canonical.starts_with(store_dir) {
            return Ok(Resolution::Store(canonical));
        }
    }

    // Only an absolute path outside the build directory belongs to the
    // platform and can be left alone. Everything else was written by the link
    // step for the build tree (`@rpath/…`, `@loader_path/…`, a relative name, or
    // a build-directory path), so if it did not resolve to a store path the
    // output would be broken once copied out.
    let path = Path::new(name);
    if path.is_absolute() && !path.starts_with(build_dir) {
        Ok(Resolution::Keep)
    } else {
        Ok(Resolution::Unresolved)
    }
}

fn apply_edits(edits: &Edits, path: &Path) -> Result<()> {
    let mut cmd = Command::new("install_name_tool");

    if let Some(id) = &edits.id {
        cmd.arg("-id").arg(id);
    }
    for (old, new) in &edits.changes {
        cmd.arg("-change").arg(old).arg(new);
    }
    for rpath in &edits.removed_rpaths {
        cmd.arg("-delete_rpath").arg(rpath);
    }
    cmd.arg(path);

    let output = cmd.output().map_err(|err| {
        anyhow!(
            "Failed to execute install_name_tool for {}: {err}. \
             Mach-O outputs need install_name_tool (cctools) on $PATH inside the task derivation.",
            path.display()
        )
    })?;

    if !output.status.success() {
        bail!(
            "install_name_tool failed for {}: {}",
            path.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    Ok(())
}

/// Re-signs an edited binary ad-hoc, the way nixpkgs' `signing-utils` hook does.
///
/// The signature is copied away and back so that `codesign` never has to
/// rewrite a file in place that it has touched before.
fn resign(path: &Path) -> Result<()> {
    let tmp = path.with_extension("nix-ninja-codesign");
    let _ = fs::remove_file(&tmp);
    fs::copy(path, &tmp)
        .with_context(|| format!("copying {} aside for re-signing", path.display()))?;

    let output = Command::new("codesign")
        .arg("-f")
        .arg("-s")
        .arg("-")
        .arg(&tmp)
        .output()
        .map_err(|err| {
            anyhow!(
                "Failed to execute codesign for {}: {err}. Editing Mach-O load commands \
                 invalidates the linker's ad-hoc signature, which arm64 macOS requires, so the \
                 output cannot be used unless it is re-signed. Provide sigtool (nixpkgs \
                 darwin.sigtool) on $PATH inside the task derivation.",
                path.display()
            )
        })?;

    if !output.status.success() {
        let _ = fs::remove_file(&tmp);
        bail!(
            "codesign failed for {}: {}",
            path.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    fs::rename(&tmp, path)
        .with_context(|| format!("moving re-signed {} back into place", path.display()))?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    /// A plain Ninja graph can name a dylib relatively (`-install_name
    /// lib/libfoo.dylib`) and link `bin/main` against `lib/libfoo.dylib`
    /// without any rpath. Nothing about that graph points into the store, so the
    /// install name must be left alone and the consumer's relative load must
    /// still be resolvable through the build directory.
    #[test]
    fn relative_install_name_resolves_through_build_dir() {
        let tmp = std::env::temp_dir().join(format!("nix-ninja-macho-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);

        let build_dir = tmp.join("build");
        let store_dir = tmp.join("store");
        let stored = store_dir
            .join("hash-libfoo")
            .join("lib")
            .join("libfoo.dylib");
        fs::create_dir_all(stored.parent().unwrap()).unwrap();
        fs::write(&stored, b"stored").unwrap();
        fs::create_dir_all(build_dir.join("lib")).unwrap();
        fs::create_dir_all(build_dir.join("bin")).unwrap();
        fs::write(build_dir.join("bin").join("main"), b"main").unwrap();
        // The dependency reaches the build directory as a symlink into the store.
        symlink(&stored, build_dir.join("lib").join("libfoo.dylib")).unwrap();

        let store_dir = fs::canonicalize(&store_dir).unwrap();
        let build_dir = std::path::absolute(&build_dir).unwrap();
        let store_dir_str = store_dir.to_string_lossy().into_owned();

        // A relative install name is kept as-is.
        assert_eq!(
            install_name(
                Some("lib/libfoo.dylib"),
                Path::new("lib/libfoo.dylib"),
                &build_dir
            ),
            None
        );

        // A build-directory install name becomes the graph-relative one.
        let absolute_id = build_dir
            .join("lib")
            .join("libfoo.dylib")
            .to_string_lossy()
            .into_owned();
        assert_eq!(
            install_name(
                Some(absolute_id.as_str()),
                Path::new("lib/libfoo.dylib"),
                &build_dir
            ),
            Some("lib/libfoo.dylib".to_string())
        );

        // `bin/main`'s recorded relative load resolves to the dependency's store
        // path, so it can be rewritten to an absolute one.
        match resolve_dep(
            "lib/libfoo.dylib",
            &build_dir.join("bin"),
            &[],
            &build_dir,
            &store_dir_str,
        )
        .unwrap()
        {
            Resolution::Store(resolved) => {
                assert_eq!(resolved, fs::canonicalize(&stored).unwrap());
            }
            _ => panic!("relative dependency did not resolve to the store path"),
        }

        fs::remove_dir_all(&tmp).unwrap();
    }
}
