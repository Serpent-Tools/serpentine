# Contribution

Thanks for showing interest in contributing to serpentine!

> [!NOTE]
> While serpentine itself is designed to work on most platforms, some of the code is designed to only run on linux (in a container), and as such `clippy`, `rust-analyzer` etc might complain when working on it on other platforms.

## Contributing
* Ask to be assigned to an issue and ask for any clarifications on the functionality wanted.
    * I will be very upfront and say that some of the issues could be better worded etc, as some were mostly written as personal todos.
* Make your change in a new branch/fork using whichever workflow you prefer.
* Run linting and tests locally, this is strictly optional as they can be a bit slow, but running a subset of them is recommended.
* Make a pull request and await review / CI.

## Project Structure

Serpentine largely consists of 3 crates, the `sidecar` is meant to be run in a Linux Docker container, `serpentine` is the main binary and contains most of the logic, `serpentine_internal` contains shared code, mostly related to the sidecar and hosts communication.

In addition the serpentine crate is roughly split between the compiler and the runtime.

> [!TIP]
> The mdbook in `/book` contains chapters about the project's more complex internals, as well as the user-facing documentation, both of which are recommended for contributors to read. Note that some chapters are still being written.

## Running tests / lints
Serpentine employs a [`justfile`](https://just.systems/man/en/) for running tasks locally, which calls out to `cargo`, `docker`/`podman` and serpentine itself.

Running basic linting just requires an installation of clippy, and can be run with `cargo clippy`.
And simple non-integration tests can be run with `cargo test` (scoping to `cargo test -p serpentine -p serpentine_internal` on non-linux platforms).

### Integration tests and deeper linting

> [!WARNING]
> The justfile assumes a Linux-like environment and you might have varying degrees of success on Windows/macOS

These tests require being able to build the local Docker image. By default the `build_container` target doesn't pull to be nicer on rate limits, run `just pull_images` at least once first.

Serpentine's integration tests and linting can be run locally:

* `just test [filter]` — run the whole test suite including the integration tests, optionally narrowed to test names containing `filter`.
* `just update_snapshots` — review and accept changed [`insta`](https://insta.rs/) snapshots.
* `just run` — the quick tests plus the light lint set.
* `just run FULL` — the full lint set, the fuzzers and the security checks.
* `just run LINTS` — the full lint set on its own.
* `just run SECURITY` — `cargo-deny` plus a `trivy` scan of the sidecar image.
* `just run FUZZ` — smoke-fuzz every [`bolero`](https://github.com/camshaft/bolero) property test under libfuzzer for a bounded time.
* `just check` — `just test` followed by `just run FULL`, the closest local equivalent to CI.
* `just sidecar_logs` — dump the containerd sidecar's logs, useful when an integration test fails.
* `just clean` — drop serpentine's cache along with its containerd volume.

