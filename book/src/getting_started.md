# Getting Started

## Installation

Serpentine requires a docker or podman daemon installed on your system to run its daemon process.

You can install it with binstall
```bash
cargo binstall serpentine
```
alternatively you can install from source with `cargo install`
```bash
cargo install serpentine
```

You can also download the release binaries yourself from github: <https://github.com/Serpent-Tools/serpentine/releases>

### Nix flake
Serpentine also comes as a nix flake, which you can install as follows:
```bash
nix profile add github:Serpent-Tools/serpentine/v1.0.0
```

Or run a pipeline without installing anything:
```bash
nix run github:Serpent-Tools/serpentine/v1.0.0 -- run
```

> [!WARNING]
> Always point the flake at a release tag, never at `main`. A release build pulls its engine image by
> the version in `Cargo.toml`, so a binary built from `main` asks for the image published for that
> version while the sidecar it was built against is whatever `main` currently holds.

Or use in your own dev shells like:
```nix
{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    serpentine.url = "github:Serpent-Tools/serpentine/v1.0.0";
  };

  outputs =
    { nixpkgs, serpentine, ... }:
    let
      pkgs = import nixpkgs {
        system = "x86_64-linux";
        overlays = [ serpentine.overlays.default ];
      };
    in
    {
      devShells.x86_64-linux.default = pkgs.mkShell {
        packages = [ pkgs.serpentine ];
      };
    };
}
```

Serpentine is exposed as both `overlays.default` and `packages.<system>.serpentine`.

## Hello ~World~ Cargo

While this section can't explain everything in the following snippets, it hopes to give you a taste of the basics of serpentine.
Serpentine uses a custom DSL called snek to define workflows, by default serpentine will look for the `DEFAULT` entrypoint in the `./main.snek` file,
Lets write a simple pipeline to check that our rust code compiles:

```snek
export DEFAULT = Image("rust:latest")
    > WorkingDir("/app")
    > With(FromHost("."), ".")
    > Exec("cargo check");
```

Saving this to `main.snek` and running `serpentine run` should download the rust image, copy your source code into a container and run `cargo check`.
Further chapters in the book will show patterns for making this play nicer with caching and doing more complex stuff. 

## Further Reading

* [Snek](./snek.md) - documentation of the snek language.
* [Builtins/Prelude](./prelude.md) - documentation of the most important nodes/functions.
