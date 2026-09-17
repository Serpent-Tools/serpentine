# Standard library Design goals

The general goal of the standard library is two fold:

* The primary goal is to provide abstractions over more verbose, but deseriable, patterns as `cargo-chef`. This way users dont have to re-create these in every project and can simply use say `rust::WithChef("cargo clippy")`.

* Secondary, the standard library aims to contain some minimal and quick to get started with CI workflows, such as `cargo fmt --check` that can be dropped straight into more simple projects. These dont need to be super flexible and aim mainly to get a project up and running with serpentine quickly, as well as act as a good examples for users who want to write their own versions for more complex projects (for example serpentine's own CI uses few of these).

## Core patterns

These are functions that abstract over more complex ecosystem patterns, even if the patterns arent widely used they can be included if the reason they arent used much is existing ergonomics.

For example if all `WithBinstall` was to run `cargo binstall` for you it wouldnt be very useful, but the fact it specifically builds/downloads the crate in a separate container in order to provide better caching and paralism.
In general the patterns that fit this category best are the ones that make use of multiple containers and specific ordering for caching, essentially most stuff that would be multiple targets in a dockerfile.

## Quick starters 

These are often pretty straight forward functions wrapping 1 or 2 commands that provide a quick and easy way to do something, for example `export DEFAULT = rust::Clippy(FromHost("."))`. They are not meant to be super flexible and are more a onboarding tactic. 

These should not try to cover every linter/tool in the ecosystem, but should instead provide the "defacto" CI setup of the ecosystem (for example clippy + rustfmt + nextest). And should take care to not be too oppionated, like while serpentine and its authors very much subscribe to denying all clippy warnings in CI, the `rust::Clippy` function does not.


