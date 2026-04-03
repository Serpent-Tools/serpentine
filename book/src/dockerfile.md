# Comparison with Dockerfile

This document can be seen as essentially a migration guide from Dockerfiles to serpentine. It will document both language features and builtin nodes, as well as patterns.

> [!IMPORTANT]
> serpentine does not attempt to have feature parity, or fully replace Dockerfiles, but considering they are both ways of describing a series of steps to run in a container it might be useful to see a comparison.

## Core mental model difference

While Dockerfiles are structured as a linear set of steps to execute, a serpentine pipeline is fundamentally a DAG (graph) with heavy similarity to functional languages.

A serpentine pipeline at its core isn't "do A, B, then C", it's "C is the result of doing B to the result of doing A to ...", this unlocks a lot of cool optimizations and ergonomic patterns we will get to at the end of this page.

## `FROM`

```dockerfile
FROM rustlang/rust:nightly as rust
```

Base images in serpentine are pulled and constructed using `Image`, which takes an image reference using the same syntax as docker.

```snek
rust = Image("rustlang/rust:nightly");
```

## `RUN`

```dockerfile
FROM rustlang/rust:nightly as app
RUN rustup component add clippy
```
Serpentine uses a node called `Exec`, which takes in a container and a command to execute, steps are executed with `/bin/sh -c ...` (so shell syntax works as expected.)

```snek
rust = Image("rustlang/rust:nightly")
    > Exec("rustup component add clippy");
```

> [!NOTE]
> Serpentine does not support specifying an argument array, but a combination of `Join` and `ShellEscape` can be used instead.

## `USER`
Serpentine has a node called `User`, which uses the same syntax as the docker `USER` directive.

## `COPY`
```dockerfile
FROM rustlang/rust:nightly as base
COPY . .
```

An important distinction in serpentine is that folders/files are values, meaning `COPY` is a two step operation, but it also means files can be returned from functions, be saved to labels etc.

`With` is used to copy into a container, it takes the files/folder to copy in and the destination. `FromHost` is used to read files/folders from the host system.

```snek
base = Image("rustlang/rust:nightly") > With(FromHost("."), ".");
```

## Multi stage builds
```dockerfile
FROM rustlang/rust:nightly as base
COPY . .

FROM base as lint
RUN cargo clippy

FROM base as test
RUN cargo test
```

Multi stage builds are both not really a feature in serpentine and at the same time its core strength. What I mean is there is no dedicated syntax for it, because how you do it is just by having a split in the graph, usually done via labels.

```snek
base = Image("rustlang/rust:nightly") > With(FromHost("."), ".");
lint = base > Exec("cargo clippy");
test = base > Exec("cargo test");
```

## `COPY --from`
```dockerfile
FROM alpine as download
RUN apk add curl
RUN curl -fsSL https://example.com/foo.txt

FROM alpine as final
COPY --from=download /foo.txt /foo.txt
```

Copying between containers is done using `Export`, which takes the file path to export.

```snek
foo = Image("alpine")
    > Exec("apk add curl")
    > Exec("curl -fsSL https://example.com/foo.txt")
    > Export("/foo.txt");

final = Image("alpine") > With(foo, "foo.txt");
```

## `ENV`
```dockerfile
FROM alpine
ENV FOO=bar
RUN echo $FOO
```

Serpentine has an `Env` node.

```snek
result = Image("alpine")
    > Env("FOO", "bar")
    > Exec("echo $FOO");
```

## Summary
| Dockerfile | Serpentine | Notes |
| --- | --- | --- |
| `FROM` | `Image` | For base images |
| `FROM` | `=` | For multi stage builds |
| `RUN` | `Exec` |  |
| `USER` | `User` |  |
| `COPY` | `With` + `FromHost` |  |
| `COPY --from` | `With` + `Export` |  |
| `ENV` | `Env` |  |


# Notable pros over Dockerfiles

This section details some of the cool features / design choices serpentine has in comparison to docker.

> [!TIP]
> For a full explanation of these features please read the language/builtin docs

## Functions and modules

Functions allow you to write reusable patterns, that can be updated from one location, and be used across an organization.

For example this function uses [cargo-chef](https://github.com/LukeMathWalker/cargo-chef), and allows its cache-friendly pattern to be re-used for multiple paths in the pipeline.

```snek
export def Cooked(container, build_kind) {
    recipe = rust
        > Binstall("cargo-chef", "cargo-chef")
        > With(source_code, ".")
        > Exec("cargo chef prepare --recipe-path recipe.json")
        > Export("recipe.json");

    return container
        > Binstall("cargo-chef", "cargo-chef")
        > With(recipe, "recipe.json")
        > Exec(Join("cargo chef cook ", build_kind, " --recipe-path recipe.json"))
        > With(source_code, ".");
}
```

For an example of a call site take this part of serpentine's own CI:
```snek
clippy_hack = lib::rust
    > Exec("rustup component add clippy")
    > lib::Cooked("--clippy")
    > lib::Binstall("cargo-hack", "cargo-hack")
    > Exec("cargo hack --locked --feature-powerset --all clippy -- -Dwarnings");
```

Which shows how complex CI/Container patterns can be encoded and used as ergonomic functions, as this CI step now reads nicely as "install clippy, pre-cook clippy on dependencies, install cargo-hack, run cargo-hack", but internally this is spawning up multiple parallel sub graphs.

## Values/Labels aren't just containers
Another neat thing is that labels/arguments/return values aren't just for containers, but any value, this seems obvious, but it gives a similar power to what `ENV` in dockerfiles are sometimes used for, avoiding magic numbers.
```snek
tool_version = "0.26.2";
```

### Files/Folders as values
This is another big change from Dockerfiles which plays really nice into the above, essentially you can store a file or folder in a label:
```snek
source_code = FromHost(".");
```

and now you can just do `With(source_code, ".")`, or maybe even cooler return files/folders from functions:
```snek
def Binstall(container, crate, bin_name) {
    binary = Image("rust:latest")
        > Exec("cargo install cargo-binstall")
        > Exec(Join("cargo binstall ", crate, " --install-path out"))
        > Export(Join("out/", bin_name));
    return binary;
}
```

And now you can do `With(Binstall("cargo-chef", "cargo-chef"), "/bin/custom_name")`, and I really want you to notice something there, you specify the file name in the destination. Because the value returned is the file/folder contents, this means that if a function returns a file you don't have to know where in the original container it lived to copy it into yours.

Unlike docker `COPY --from` where you need to know the destination and *source* location at the "call site".
