# Caching

Serpentine uses a [content-addressable cache](https://en.wikipedia.org/wiki/Content-addressable_storage) keyed on the inputs to a node. This means that if a node gets inputs it has seen before it will skip the operation and just load the stored value.

For example given this pipeline:
```snek
rust = Image("rust") > Exec("cargo install cargo-nextest");
```
The image pull and `cargo install` steps will only ever be run once.

Now for example if we have:
```snek
rust = Image("rust") 
    > Exec("cargo install cargo-nextest")
    > Exec("cargo install cargo-hack");
```
If we run this, and then say swap `cargo-hack` for `cargo-deny` the nextest step will still be skipped, and naturally `cargo-deny` will be installed.
An important point is that the cache is keyed by node type and inputs, not its exact position in the graph. This means that if a node gets inputs it has seen before, but in a completely different context it is still cached.

### How is this different from the compiler de-duplicator?
You may recall that the compiler de-duplicates identical graphs? This might sound similar to the cache, and in fact if you removed the compiler optimizer the cache would be able to cover some of its usecase. Where it breaks down however is in race conditions, if you have two identical graphs they will likely both start to execute as neither has committed its result to the cache, while with the compiler optimizer it would have combined the two branches before runtime.

Another important difference is that the compiler is structural matching, while the cache is semantic matching. What we mean is that the compiler folds together what amounts to copy and pasted *code*, while the cache handles all cases where a node might be asked to perform an operation it has already done, an example of this is:

```snek
foo = Image("rust")
    > Exec("cargo install cargo-nextest");

bar = Image("rust")
    > Exec(Noop("cargo install cargo-nextest"));
```
The compiler will not merge these together, because they are fundamentally different graph shapes, but (assuming the ordering works out) the cache will re-use the result of the earlier one because at runtime both exec nodes see "rust image + cargo install cargo-nextest"

And crucially the cache sticks around between runs, meaning work is also skipped on further runs.

## How caching works for specific values

> [!NOTE]
> This section assumes some familiarity with the core builtin nodes.

In general the cache simply saves the output value of the node and hashes the data of the inputs, but there are some nuances here.

### Files/Folders
Files/folders are generally not actually passed around in memory, instead a lightweight handle to the file location is passed around, for example `FromHost` really just returns an object telling `With`/`ToHost` "please read files from here", same for `Export`.

When hashing a file/folder all contents are hashed, this requires reading the data. Serpentine uses a streaming hasher, such that it never has to read an entire folder into memory. This does however mean that for example `With(FromHost("."), ".")` will read the source files twice.

> [!NOTE]
> After the first hash of a file/folder the hash is itself cached, meaning it's only calculated once per run, but copying a file/folder into a container still reads the contents from disk.

Notably we do not transmit or hash the file metadata (except names of files in folders etc), this means that file copy operations are one of the cases where you can get a cache "re-hit".

For example if you have
```snek
some_file = long_chain > Exec("...") > Export("foo.txt");
other_chain = Image("...") > With(some_file, "foo.txt") > Exec("expensive_command");
```
If the long_chain is invalidated then it will all re-run, but if the final contents of the `foo.txt` file are the same, once that's exported the `With` node will get a cache hit and lead to the `expensive_command` being skipped!

### Container States

You would think container layers are cached in a similar way, but in fact they are not. In fact container states only hash their configuration and the *snapshot (layer) names*.
This means that effectively a container operation can never re-trigger-cache-hits. 

The reason for this is a pragmatic one, it's extremely unlikely that the file system deltas will perfectly be re-constructed, even if you ran the same operation on the same base files, builds, etc contain timestamps, not to mention all the other metadata and files Linux systems generate when they run. This closely matches how Docker layer caching works in practice.

### Services

Services are cached the exact same as containers, containers simply hash the services they contain as well.

## Best practices for caching

> [!TIP]
> A lot of these tips also apply to making the graph more friendly for the optimizer! Because both ultimately care about when in the graph stuff diverges from static inputs.

In practice the thing that will be invalidating your caches is `FromHost`, i.e source code changes, the general advice is to just introduce the source code as late as possible, and use file copying to build artifacts in independent chains. 

For example this `Binstall` function doesn't use the input container until the last step, meaning the entire tool install is cached:
```snek
export def Binstall(container, crate, bin_name) {
    binary = rust 
        > Exec("cargo install cargo-binstall")
        > Exec(Join("cargo binstall --root /out ", crate))
        > Export(Join("/out/bin/", bin_name));

    return container > With(binary, Join("/bin/", bin_name));
}
```

Similarly this `cargo-chef` function uses the standard cargo-chef pattern, when the source code changes, but not the `Cargo.toml`s the recipe generation chain will be invalidated, but then the exported file stays the same so we get a cache hit on the `cargo chef cook` step:
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

## `--clean-old`
By default the serpentine cache is purely additive, but this can result in a lot of space use over time. Running with the `--clean-old` flag will only keep values produced (or read) this run. There is also the `clean` sub-command which will delete all data from the cache.

## `--standalone-cache`

As noted above serpentine simply caches snapshot names, this means the cache cannot be moved between machines as the containerd volumes would not be moved along with it.

This is where the "standalone" mode comes in, it will write the actual layer diffs to the cache file which can then be moved between systems.

> [!IMPORTANT]
> The hash is still just based on the names, this simply makes it so the cache is portable between systems and should not change any runtime semantics.
