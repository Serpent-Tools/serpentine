# Cli

> [!TIP]
> You can always refer to `serpentine --help`, `serpentine run --help` etc locally

## `run`

Run a serpentine pipeline, takes no positional arguments. By default this will work perfectly fine in most repos, but can be customized as seen below.
For example CI systems will likely want some variation on:

```bash
serpentine run --cache /tmp/cache.serpentine --clean-old --standalone-cache --ci
```

### `--pipeline` / `-p`
* default: `./main.snek`

takes the pipeline file to run, this can be any UTF-8 file that contains a snek definition, the `.snek` extensions is not a requirement.

### `--entry-point` / `-e`
* default: `DEFAULT`

Specify the (exported) label to execute, this is extremely useful for having multiple entrypoints in the same file, similar to say make/just.
One can for example have a exported label that just runs tests, while the default runs tests, linting and does a production build.

### `--jobs` / `-j`
* default: 2

Limits the number of *`Exec`/`ExecOutput` nodes* that can run in parallel, in other words it limits meaningfull CPU heavy workloads, while file copying and similar is allowed to happen fully parallel. Its recommended to only increase this value after warming caches, as a high value on cold caches will often lead to no gain due to build systems like cargo already using a large number of cores.

### `--ci`

Switches serpentines output to "CI mode", in practice this means disabling the fancy TUI and instead just emitting logs to stdout, some users might also prefer this when running locally. In addition this mode will be automatically enabled if ran in a non interactive terminal (but its preferable to specify it explicitly in ci runners).

### `--cache` / `-c`
* default: platform specific, printed to stdout at the start of a run.

Specifies the location to store caches, by default it will store it in the systems default cache location, this flag should be used in CI to store the cache file at a deterministic location.

### `--standalone-cache`

Enables serpentines standalone cache mode, making the cache fully portable between systems, needed if preserving cache on CI runners, see [Caching](./caching.md) chapter for more details.

### `--clean-old`

Delete caches left over from older runs, by default serpentine treats the cache as append only, but especially in CI you might want to clean out unused stuff.

## `clean`

Cleans out the serpentine cache.

> [!IMPORTANT]
> This is not just deleting the file on disk (while it does do that), it also cleans up any referenced data from serpentines docker volumes.

Takes a optional argument which is the cache file to clean.
