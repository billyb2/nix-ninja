use anyhow::{Context, Result};
use clap::Parser;
use harmonia_store_derivation::derived_path::{OutputName, SingleDerivedPath};
use harmonia_store_path::StoreDir;
use nix_builder_rpc_client::BuilderRpcClient;
use nix_ninja_task::derived_file::{create_symlinks, DerivedFile};
use nix_ninja_task::linkage;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::str::FromStr;

#[derive(Parser)]
#[command(author, disable_version_flag = true)]
pub struct Cli {
    /// Specify the Nix store directory.
    #[arg(long = "store-dir", env = "NIX_STORE", default_value = "/nix/store")]
    pub store_dir: StoreDir,

    /// Directory prefix to recreate sources via symlinks.
    ///
    /// Defaults to `$NIX_BUILD_TOP/source/build`, the layout the surrounding
    /// derivation uses. The `/build` this used to be hardcoded to is only the
    /// Linux sandbox build root; Darwin builds use a temporary directory under
    /// `$TMPDIR`, which is why the default is derived from `$NIX_BUILD_TOP`.
    #[arg(long = "build-dir")]
    pub build_dir: Option<PathBuf>,

    /// Optional build target description.
    #[arg(long)]
    pub description: Option<String>,

    // Encoded derived files to prepare the source directory.
    #[arg(long, env = "NIX_NINJA_INPUTS", default_value = "")]
    pub inputs: String,

    // Paths to manifest derivations whose placeholders Nix has resolved.
    #[arg(long, env = "NIX_NINJA_INPUTS_FILES")]
    pub input_files: Option<String>,

    // Encoded derived files that build outputs should be copied to.
    #[arg(long, env = "NIX_NINJA_OUTPUTS", default_value = "")]
    pub outputs: String,

    // Static manifest entries begin with an output variable name.
    #[arg(long, env = "NIX_NINJA_OUTPUTS_FILE")]
    pub outputs_file: Option<PathBuf>,

    #[arg(long)]
    pub write_inputs_manifest: bool,

    // Command to run, absent only when writing a resolved input manifest.
    pub cmdline: Option<String>,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    if cli.write_inputs_manifest {
        let out = env::var_os("out").context("missing manifest output path")?;
        fs::write(out, cli.inputs.as_bytes())?;
        return Ok(());
    }
    let cmdline = cli.cmdline.as_deref().context("missing build command")?;
    let build_dir = cli.build_dir.clone().unwrap_or_else(default_build_dir);

    fs::create_dir_all(&build_dir)?;
    std::env::set_current_dir(&build_dir)?;

    let mut inputs = Vec::new();
    for encoded in cli.inputs.split_whitespace() {
        // println!("Processing input {}", encoded);
        let input = DerivedFile::from_encoded(&cli.store_dir, encoded)?;
        inputs.push(input);
    }
    if let Some(files) = &cli.input_files {
        for file in files.split_whitespace() {
            let encoded = fs::read_to_string(file)
                .with_context(|| format!("reading input manifest {file}"))?;
            for entry in encoded.split_whitespace() {
                inputs.push(DerivedFile::from_encoded(&cli.store_dir, entry)?);
            }
        }
    }

    let mut outputs = Vec::new();
    for encoded in cli.outputs.split_whitespace() {
        // println!("Processing output {}", encoded);
        let output = DerivedFile::from_encoded(&cli.store_dir, encoded)?;
        outputs.push(output);
    }
    if let Some(file) = &cli.outputs_file {
        let encoded = fs::read_to_string(file)
            .with_context(|| format!("reading output manifest {}", file.display()))?;
        for entry in encoded.split_whitespace() {
            let (name, paths) = entry
                .split_once(':')
                .context("invalid output manifest entry")?;
            let store_path =
                env::var(name).with_context(|| format!("missing Nix output variable {name}"))?;
            outputs.push(DerivedFile::from_encoded(
                &cli.store_dir,
                &format!("{store_path}:{paths}"),
            )?);
        }
    }

    // The source directory of the derivation needs to have all build inputs
    // symlinked while preserving the original directory hierarchy of the
    // sources. This ensures relative includes and other path-dependent
    // references remain valid.
    create_symlinks(&build_dir, &cli.store_dir, inputs, false)?;
    println!(
        "nix-ninja-task: Setup source directory in {}",
        build_dir.display()
    );

    // CMake emits `cd DIR && COMMAND` even when DIR has no declared file
    // inputs. Recreate that working directory before executing the command;
    // input symlinks alone do not guarantee it exists in an empty sandbox.
    create_command_working_dirs(cmdline)?;

    // Outputs are written to the same directory structure as the build
    // directory because if the output is a shared library the filename must
    // match the soname and it must be in a directory to add to the linking
    // binary's RUNPATH.
    create_output_dirs(&outputs)?;

    if let Some(desc) = cli.description {
        println!("nix-ninja-task: {desc}");
    }

    // Spawn cmdline process via sh like ninja upstream does.
    println!("nix-ninja-task: Running: /bin/sh -c \"{}\"", cmdline);
    let exit_code = spawn_process(cmdline)?;
    if exit_code != 0 {
        println!("nix-ninja-task: Failed with exit code {exit_code}");
        std::process::exit(exit_code);
    }

    // Relocatable bundles keep their original @rpath/@loader_path linkage;
    // rewriting each component to an individual store path would also force
    // ad-hoc re-signing and discard its XPC entitlements.
    match env::var("NIX_NINJA_LINKAGE") {
        Ok(mode) if mode == "none" => {}
        Ok(mode) if mode == "auto" => {
            linkage::fix_linkage(cli.store_dir.to_path(), &build_dir, &outputs)?;
        }
        Err(env::VarError::NotPresent) => {
            linkage::fix_linkage(cli.store_dir.to_path(), &build_dir, &outputs)?;
        }
        Ok(mode) => anyhow::bail!("unsupported NIX_NINJA_LINKAGE value: {mode}"),
        Err(error) => return Err(error.into()),
    }

    // Outputs must be created in build directory and then copied out because
    // ninja build rules can have implicit outputs that we have no way of
    // knowing. For example, a custom command that doesn't leverage the `$out`
    // implicit variable in the ninja evaluation context.
    println!(
        "nix-ninja-task: Finished! Copying {} build outputs to derivation output paths",
        outputs.len(),
    );
    // Placeholder output paths are identifiers, not writable store paths.
    // Upload the finished output through the restricted builder socket, then
    // submit the actual store path returned by the daemon.
    let rpc_client = BuilderRpcClient::connect_from_env()
        .context("connecting to the builder RPC socket for task outputs")?;
    // Register output with the graph derivation that will own it, not with
    // the generic task derivation's name.
    let task_name = env::var("name")
        .or_else(|_| env::var("NIX_NINJA_OUTER_NAME"))
        .unwrap_or_else(|_| "ninja-build".to_owned());
    // A task output is a directory holding that output at its build-relative
    // path, not the bare file: `DerivedFile::absolute_path` resolves an input
    // as `<store path>/<rel_path>`, which is also the layout the placeholder
    // build wrote into `$out`. Uploading the bare output made every consumer
    // link a path that does not exist.
    let staging_root = build_dir.join(".nix-ninja-outputs");
    for output in &outputs {
        let normalized_name: String = output
            .build_path
            .to_string_lossy()
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.' | '_' | '?' | '=') {
                    c
                } else {
                    '-'
                }
            })
            .collect();
        let name = OutputName::from_str(&normalized_name)?;
        let uploaded_name = if name == OutputName::default() {
            task_name.to_owned()
        } else {
            format!("{task_name}-{name}")
        };
        let stage = staging_root.join(&normalized_name);
        fs::create_dir_all(&stage)?;
        let relative = output.rel_path.as_deref().unwrap_or(&output.build_path);
        copy_output_tree(&output.build_path, &stage.join(relative))?;
        let uploaded = rpc_client
            .add_to_store_nar(&uploaded_name, &stage)
            .with_context(|| format!("uploading task output {}", output.build_path.display()))?;
        rpc_client
            .submit_output(&SingleDerivedPath::Opaque(uploaded), &name)
            .with_context(|| format!("submitting task output {name}"))?;
    }

    Ok(())
}

/// Copies one declared output at its build-relative path, following symlinks.
///
/// CMake's `create_symlink` header aliases resolve against the task's staged
/// source tree; keeping the link itself would leave its relative target
/// dangling once the output is an immutable store path, so the alias stores
/// the file it names.
fn copy_output_tree(source: &Path, destination: &Path) -> Result<()> {
    let source = fs::canonicalize(source)
        .with_context(|| format!("resolving task output {}", source.display()))?;
    if fs::metadata(&source)?.is_dir() {
        fs::create_dir_all(destination)?;
        for entry in fs::read_dir(&source)? {
            let entry = entry?;
            copy_output_tree(&entry.path(), &destination.join(entry.file_name()))?;
        }
        return Ok(());
    }
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::copy(&source, destination).with_context(|| {
        format!(
            "copying task output {} to {}",
            source.display(),
            destination.display()
        )
    })?;
    Ok(())
}

/// The build directory the surrounding derivation uses.
///
/// Nix sets `$NIX_BUILD_TOP` to the sandbox build root on every platform; the
/// source tree is copied there under `source/` and Meson configures in
/// `source/build`.
fn default_build_dir() -> PathBuf {
    match env::var_os("NIX_BUILD_TOP") {
        Some(top) => Path::new(&top).join("source/build"),
        None => PathBuf::from("/build/source/build"),
    }
}

fn create_command_working_dirs(cmdline: &str) -> Result<()> {
    let bytes = cmdline.as_bytes();
    let mut search_start = 0;
    while let Some(relative) = cmdline[search_start..].find("cd ") {
        let start = search_start + relative;
        let preceded_by_separator =
            start == 0 || matches!(bytes[start - 1], b' ' | b'\t' | b'\n' | b'\r' | b'=' | b';');
        if preceded_by_separator {
            let directory_start = start + 3;
            let rest = &cmdline[directory_start..];
            let directory_end = rest
                .find(char::is_whitespace)
                .context("unterminated cd working directory")?;
            let directory = &rest[..directory_end];
            if rest[directory_end..].trim_start().starts_with("&&")
                && !directory.is_empty()
                && !directory.starts_with('/')
                && !directory.contains(['$', '\'', '"', ';', '|', '&'])
            {
                fs::create_dir_all(directory)
                    .with_context(|| format!("creating command working directory {directory}"))?;
            }
        }
        search_start = start + 3;
    }
    Ok(())
}

fn create_output_dirs(outputs: &Vec<DerivedFile>) -> Result<()> {
    let mut dirs: Vec<&std::path::Path> = Vec::new();
    for output in outputs {
        if let Some(parent) = output.build_path.parent() {
            if dirs.contains(&parent) {
                continue;
            }
            std::fs::create_dir_all(parent)?;
            dirs.push(parent);
        }
    }
    Ok(())
}

fn spawn_process(cmdline: &str) -> Result<i32> {
    let mut cmd = Command::new("/bin/sh");
    cmd.args(["-c", cmdline])
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .envs(env::vars());

    let output = cmd.status()?;
    Ok(output.code().unwrap_or(1))
}
