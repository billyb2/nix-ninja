{ lib
, coreutils
, meson
, nix
, nix-ninja
, nix-ninja-task
, patchelf
  # Ad-hoc re-signing, needed after nix-ninja-task rewrites Mach-O load
  # commands. Only provided on Darwin.
, sigtool ? null
, stdenv
}:

{ name ? "${args'.pname}-${args'.version}"
, src
, target
, nativeBuildInputs ? [ ]
, ...
}@args':

let
  normalizedTarget = builtins.replaceStrings ["/"] ["-"] target;

  ninjaDrv = stdenv.mkDerivation (args' // {
    name = "${name}.drv";

    # Unfortunately stdenv's `genericBuild` assumes the `out` variable is set.
    # That is generally a reasonable assumption as it is handled by nix,
    # but it is intentionally left unset when running with `builder-rpc-v0`.
    # For basic programs it is possible to avoid this hack by running `builtins.derivation`
    # directly, without nixpkgs.
    # For more complex programs, however, stdenv is necessary to run hooks, such as from `pkg-config`.
    out = "/nonexistent";

    nativeBuildInputs = [
      coreutils
      meson
      nix
      nix-ninja
      nix-ninja-task
    ]
    # nix-ninja-task repairs each output's runtime linkage with the tool of the
    # platform's object format: patchelf for ELF RPATHs on Linux, and
    # install_name_tool (from cctools, in the stdenv path) plus an ad-hoc
    # re-signer for Mach-O load commands on Darwin.
    ++ lib.optionals stdenv.hostPlatform.isLinux [ patchelf ]
    ++ lib.optional (stdenv.hostPlatform.isDarwin && sigtool != null) sigtool
    ++ nativeBuildInputs;

    requiredSystemFeatures = [ "builder-rpc-v0" ];

    preConfigure = ''
      export NIX_NINJA_DRV="true"
      export NINJA="${nix-ninja}/bin/nix-ninja"
      export NIX_CONFIG="extra-experimental-features = nix-command ca-derivations dynamic-derivations"
    '' + lib.optionalString stdenv.hostPlatform.isDarwin ''
      # After linking, nix-ninja-task rewrites Mach-O install names to full
      # /nix/store paths, which are longer than the @rpath names Meson emits.
      # The linker must reserve room for the grown load commands; nixpkgs sets
      # this flag per-package for exactly the same reason.
      export NIX_LDFLAGS="''${NIX_LDFLAGS:-} -headerpad_max_install_names"
    '';

    buildPhase = ''
      runHook preBuild
      nix-ninja ${target}
      runHook postBuild
    '';

    dontUseMesonInstall = true;
    dontUseMesonCheck = true;

    # stdenv adds a -rpath with a self reference but self references are not
    # allowed by text output.
    NIX_NO_SELF_RPATH = true;

    __contentAddressed = true;
    outputHashMode = "text";
    outputHashAlgo = "sha256";

    passthru = {
      target = builtins.outputOf ninjaDrv.outPath normalizedTarget;
    };
  });

in ninjaDrv
