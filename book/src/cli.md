# CLI

> [!TIP]
> You can always refer to `serpentine --help`, `serpentine run --help` etc locally

## `run`

Run a serpentine pipeline, takes no positional arguments. By default this will work perfectly fine in most repos, but can be customized as seen below.
For example CI systems will likely want some variation on:

```bash
serpentine run --cache /tmp/serpentine_cache --clean-old --standalone-cache
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

Limits the number of *`Exec`/`ExecOutput` nodes* that can run in parallel, in other words it limits meaningful CPU heavy workloads, while file copying and similar is allowed to happen fully parallel. Its recommended to only increase this value after warming caches, as a high value on cold caches will often lead to no gain due to build systems like cargo already using a large number of cores.

### `--output`
* default: `auto`

Selects how serpentine renders a run:

| value | behaviour |
| --- | --- |
| `auto` | Picks from the environment, as described below. |
| `tui` | The fancy live progress view. |
| `plain` | Log lines and captured command output straight to stdout. |
| `github` | Stdout, with each command folded into a collapsible [github actions log group](https://docs.github.com/en/actions/reference/workflows-and-actions/workflow-commands). Requires `--jobs 1`, serpentine errors out otherwise. |
| `none` | Renders nothing, the run still writes its log file. |

Under `auto` serpentine uses `github` when it detects github actions with `--jobs 1`, `plain` under any other CI runner or when stdout is not an interactive terminal, and `tui` otherwise. This means CI runners generally need no flag at all, though setting it explicitly is harmless.

### `--cache-folder`
* default: platform specific, printed to stdout at the start of a run.

Specifies the directory to store caches in, by default it will store it in the systems default cache location, this flag should be used in CI to store the cache at a deterministic location.
The directory is created if it doesn't exist.

### `--cache-backend`/`-c`
The caching backend to use


| value | behaviour |
| --- | --- |
| `auto` | Picks from the environment, as described below. |
| `fs` | Cache to the directory specified by `--cache-folder` |
| `github` | Cache to github actions cache, only avaialbe in github actions. |
| `none` | disables the cache. |


Under `auto` serpentine:
* Uses `fs` if `--cache-folder` is set
* otherwise uses `github` if running in github actions.
* otherwise uses `fs`

### `--containerd-namespace`
* default: `serpentine`

The containerd namespace to scope every snapshot, layer and lease to. Serpentine already makes concurrent instances cooperate within a namespace, so the main use for this is temporarily throwing away the snapshot caches by running under a fresh name (note that with `--standalone-cache` the layers are still recoverable from the cache directory).

### `--standalone-cache`

Enables serpentines standalone cache mode, making the cache fully portable between systems, needed if preserving cache on CI runners, see [Caching](./caching.md) chapter for more details.

### `--clean-old`

Delete caches left over from older runs, by default serpentine treats the cache as append only, but especially in CI you might want to clean out unused stuff.

> [!NOTE]
> Serpentine will do its best to also cleanout the containerd state in a similar manner, but may sometimes miss stuff. In CI with empherial runners this is not a concern, but locally you might want to run a `serpentine clean` once in a while.

## `clean`

Cleans out the serpentine cache.

> [!IMPORTANT]
> This is not just deleting the directory on disk (while it does do that), it also deletes serpentines docker volume for its containerd side state.

Takes an optional argument which is the cache directory to clean.
