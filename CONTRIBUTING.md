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

* `just test [filter]` — run integration tests, optionally with a filter argument.
* `just run` — run a small linting stage.
* `just run FULL` — run the full linting stage.
* `just check` — closely matches CI (tests + full lint), with the exception of only testing on the current platform.

## `cargo-vet`
When updating/adding a dependency you might run into a `cargo-vet` error. When this happens we have a few options in order of preference:

* Downgrade to an audited version (you can use the `ci/cargo_vet_downgrade.sh` script to do this for all failing/exempt crates).
* Audit the diff yourself if it's small enough and reasonable.
* Trust the crate. This should rarely be done, but is generally accepted if there are no other options and the crate is popular.

### Cutting down on trusts and exemptions
If you wish to help us cut down on exemptions and trust entries we have two scripts for this:

* `ci/cargo_vet_improvements` — Sort current exemptions by line count, allowing you to quickly see what might be easy pickings.
* `ci/cargo_vet_unneeded_trusts.sh` — Attempt to remove a trust entry and report the effect. Any trust entries marked as `REDUNDANT` can/should be removed. Other entries will require line review; you can consider removing the trust entry and doing the audit (though in general spending audit time on the current exempt crates is more impactful).
