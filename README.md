<div align="center">

# nix-ninja

Incremental compilation of [Ninja build files][ninja-build] using
[Nix Dynamic Derivations][dynamic-derivations].

Choosing ninja as the build graph representation lets us support any build
system that outputs ninja like CMake, meson, premake, gn, etc.

[![Demo](docs/demo.gif)](https://asciinema.org/a/711344)

[Key features](#key-features) •
[Getting started](#getting-started) •
[Design notes][design notes] •
[Contributing](CONTRIBUTING.md)

</div>

## Key features

> [!IMPORTANT]
> There are still major todos, and depends on experimental features from an
> unreleased version of Nix. Come help us get nix-ninja to be useful day-to-day
> and working with an official Nix release!

- Parses `ninja.build` files and generates a derivation per compilation unit.
- Stores build inputs & outputs in content-addressed derivations for granular
  and Nix-native incrementality.
- Compatible CLI for ninja, so if you set `$NINJA` to `nix-ninja` then meson
  just works.
- Supports running locally (which runs `nix build` on your behalf), or inside a
  Nix derivation (which creates dynamic derivations).

## Getting started

First you need a Nix that implements the `builder-rpc-v0` sandbox interface,
which is newer than any released Nix: the daemon must support `AddToStoreScanning`
and `SubmitOutput`, and derivations must be able to request it through
`requiredSystemFeatures`. This flake pins such a Nix as its `nix` input; a stock
Nix (2.32.4 at the time of writing) does not implement these operations.

Enabling experimental features on a stock Nix is *not* sufficient: the daemon
ignores client-supplied `experimental-features`, so `nix-command`,
`dynamic-derivations` and `ca-derivations` must come from the daemon's own
configuration, and the daemon must also be the pinned build.

```sh
experimental-features = ["nix-command" "dynamic-derivations" "ca-derivations" "recursive-nix"]
```

The same feature set must be available where derivations are instantiated, e.g.
`NIX_CONFIG="extra-experimental-features = nix-command ca-derivations dynamic-derivations"`.

nix-ninja drives native builds on `x86_64-linux`, `aarch64-linux`, `x86_64-darwin`
and `aarch64-darwin`. Task derivations take the platform of the machine running
`nix-ninja`; on Darwin each output's Mach-O install names are repaired with
`install_name_tool` and re-signed ad hoc (`codesign` from `darwin.sigtool`),
mirroring how ELF RPATHs are repaired with `patchelf` on Linux.

On Darwin, a CMake-generated graph with two shared libraries and an executable
has been exercised with the pinned daemon. Commandless `phony` aliases can be
dependencies of concrete targets, including CMake's object-order aliases;
the `: &&` link prefix is also supported. Request a concrete output: an
aggregate `phony` target is not yet a supported result. This bounded check
does not establish compatibility with a full WebKit graph.

Then you can try building the examples:

```sh
# Builds a basic main.cpp.
nix build github:pdtpartners/nix-ninja#example-hello

# Builds a basic main.cpp with dependency inference for its header.
nix build github:pdtpartners/nix-ninja#example-header

# Builds Nix 2.27.1.
nix build github:pdtpartners/nix-ninja#example-nix
```

You can also try running `nix-ninja` outside of Nix, but you'll need both
`nix-ninja` and `nix-ninja-task` to be in your `$PATH`. Make sure
`nix-ninja-task` is from the `/nix/store` as it is needed inside derivations
`nix-ninja` generates.

```sh
export NIX_NINJA=$(nix build --print-out-paths)
export PATH="${NIX_NINJA}/bin:$PATH"
# Meson respects this environment variable and uses it as if its ninja.
export NINJA="${NIX_NINJA}/bin/nix-ninja"
```

Then you can go to an example and run it like so:
```sh
$ nix-shell
$ cd examples/hello
$ meson setup build
$ cd build
$ meson compile hello
$ ./hello
Hello Nix dynamic derivations!
```

## Contributing

We still have major TODOs, so would appreciate any help. We've organize them
under two [GitHub milestones][milestones]:

- `0.1.0` - The first release of `nix-ninja` aiming for correctness.
- `0.2.0` - Major performance features to make incremental builds productive.

Regardless, pull requests are welcome for any changes. Consider opening an issue
to discuss larger changes first, especially when the design space is large.

Please read [CONTRIBUTING](CONTRIBUTING.md) and the [design notes] so you
understand the big picture and prior art.

## License

The source code developed for nix-ninja is licensed under MIT License.

[design notes]: docs/design.md
[dynamic-derivations]: docs/dynamic-derivations.md
[milestones]: https://github.com/pdtpartners/nix-ninja/milestones
[ninja-build]: https://ninja-build.org/
