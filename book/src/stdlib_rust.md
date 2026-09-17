# Rust

The stdlib rust module provides functions and labels for when writing pipelines for a rust project, its imported as follows:
```snek
import "@/rust.snek" as rust;
```

## Core 

These are the functions/labels we expect most projects to use, firstly we have images for stable and nightly rust. `rust::stable` and `rust::nightly`.

### `Chef`/`WithChef`

The `Chef`/`WithChef` function will use [cargo-chef](https://github.com/lukemathwalker/cargo-chef) to ensure that changes to your source code doesnt cause serpentine to re-compile all your dependencies.

They take the base container and your source code, and then pre-warms the dependencies using your provided command. `WithChef` in addition also executes your command.
```snek
export def Chef(container, source_code, command) { /* ... */ }
export def WithChef(container, source_code, command, cook_command = Join(command, " || true")) { /* .. */ }
```

This lets you use them like this:
```snek
source_code = FromHost(".");
export DEFAULT = rust::stable
    > rust::WithChef(source_code, "cargo build --release --features my_feature");
```

### `Binstall`/`WithBinstall`

These functions uses [cargo-binstall](https://github.com/cargo-bins/cargo-binstall) to download a crates pre-built binaries, `Binstall` returns the binary, while `WithBinstall` will also copy it into the given container under `/bin`

```snek
export def Binstall(crate, bin_name = crate, build_container = stable) { /* ... */ }
export def WithBinstall(target_container, crate, bin_name = crate, build_container = stable) { /* ... */ }
```

If the crates binary name differs from its crate name you can specify the third argument, if the crate doesnt publish pre-built binaries it will be compiled from source (using the `build_container`). This function is structured to cache the binary separate from the target_container, and also between calls. Meaning multiple calls to `WithBinstall`, or even a call after copying in your source code, will still cache well.

```snek
export DEFAULT = rust::stable
    > With(FromHost("."), ".")
    > rust::WithBinstall("cargo-deny")
    > Exec("cargo deny all");
```

## Quickstart functions

These functions are included for quickly getting a rust project setup with serpentine, it is fully expected for you to implement them yourself with the core primitives when your needs grow, like adding more linters, or `cargo-hack`, multiple targets, etc. Hence instead of providing detailed docs for each one we have elected to instead just show you the source code:
```snek
export def Clippy(source_code, container = stable) {
    return container
        > Exec("rustup component add clippy")
        > WithChef(source_code, "cargo clippy --all-targets --all-features");
}

export def Nextest(source_code, container = stable) {
    return container
        > WithBinstall("cargo-nextest")
        > WithChef(source_code, "cargo nextest run --all-features --no-fail-fast");
}

export def Doctests(source_code, container = stable) {
    return container
        > WithChef(source_code, "cargo test --doc --all-features");
}

export def Tests(source_code, container = stable) {
    return All(Nextest(source_code, container), Doctests(source_code, container));
}

export def FmtCheck(source_code, container = stable) {
    return container
        > Exec("rustup component add rustfmt")
        > With(source_code, ".")
        > Exec("cargo fmt --check");
}

export def BasicCI(source_code, container = stable) {
    return All(
        Clippy(source_code, container),
        Tests(source_code, container),
        FmtCheck(source_code, container)
    );
}
```
