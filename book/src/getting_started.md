# Getting Started

## Installation

Serpentine requires a docker or podman daemon installed on your system to run its daemon process.

> [!CAUTION]
> Serpentine currently is not distributed anywhere, this section will ofc be updated once thats the case.
> For now please following instructions in `CONTRIBUTING.md` to run Serpentine from source.

## Hello ~World~ Clippy

While this section cant explain everything in the following snippets, it hopes to give you a taste of the basics of serpentine.
Serpentine uses a custom DSL called snek to define workflows, by default serpentine will look for the `DEFAULT` entrypoint in the `./main.snek` file,
Lets write a simple pipeline to check that our rust code compiles:

```snek
export DEFAULT = Image("rust:latest")
    > WorkingDir("/app")
    > With(FromHost("."), ".")
    > Exec("cargo check");
```

Saving this to `main.snek` and running `serpentine run` should download the rust image, copy your source code into a container and run `cargo check`.
Further chapthers in the book (and the examples page) will show patterns for making this play nicer with caching and doing more complex stuff. 

## Further Reading

* [Snek](./snek.md) - documentation of the snek language.
* [Builtins/Prelude](./prelude.md) - documentation of the most important nodes/functions.
* [Examples](./examples.md) - If you prefer to jump straight into some code the example page has a lot of nice patterns and snippets you can use to get started quickly.
