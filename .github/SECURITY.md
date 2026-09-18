# Security Policy

## Supported versions

Fixes land on the latest release. There are no maintained backport branches, so "upgrade to the newest version" is the answer to most security questions here.

## What is in scope

Serpentine is **not** a sandbox, and does not try to be. The [security chapter](https://serpentine.vivax.dev/security.html) of the book explains this in full, but the short version is that serpentine trusts two things completely:

* **The pipeline.** A `.snek` file is code you have chosen to run, at compile time and at runtime alike. A pipeline escaping its container or reaching the sidecar is expected behaviour, not a vulnerability.
* **The cache.** Serpentine acts on what its cache tells it. Keeping a shared cache from being written to by someone you don't trust is the CI platform's job, via whatever scoping it offers for cache entries (branch isolation on GitHub Actions, for example).

So a report that boils down to "a malicious pipeline did something bad" or "a poisoned cache entry did something bad" will be closed as working as intended.

What **is** in scope is serpentine failing to hold a boundary it does claim:

* Leaking credentials it was handed, such as registry or cache tokens, into logs, cache artifacts or container layers.
* Exposing the sidecar more widely than documented, for example binding somewhere other than localhost.
* Memory unsafety reachable from something serpentine is meant to parse defensively rather than execute.

If you are unsure which side of the line something falls on, report it and say so.

## Reporting

Please report privately through GitHub's [private vulnerability reporting](https://github.com/Serpent-Tools/serpentine/security/advisories/new) rather than opening a public issue.

Include what you need to reproduce it: the pipeline, the serpentine version (`serpentine --version`), and your container runtime.

This is a small project, so expect a reply in days rather than hours. You will get an acknowledgement that it was received, an assessment of whether it is in scope, and credit in the advisory unless you would rather not be named.

## Dependencies

Dependency advisories are tracked with `cargo-deny` and `trivy`, and updated via renovate. If you have found a known-vulnerable dependency rather than a flaw in serpentine itself, a normal public issue is fine, no need for the private channel.
