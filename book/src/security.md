# Security

> [!CAUTION]
> Serpentine is **NOT** a sandbox. 
> ***Never run untrusted pipelines***

While serpentine does run workflows in containers it makes no guarantee about what privileges and level of isolation it grants them.
In general it should be assumed that a malicious serpentine pipeline can escape the containers and damage your system.

In addition serpentine does expose a privileged docker container over tcp on your system (bound to localhost), it essentially has the same access as a process with access to docker has, whether this is an actual concern or not depends on your environment, but for most users this is not a concern as they either already give their non-root users docker access, or in general it's assumed you aren't running malicious code on your own machine.

## Security of serpentine itself

Serpentine employs a combination of `cargo-deny` and `trivy` to vet its dependencies, it uses each of these tools slightly differently.

> [!NOTE]
> Yes we are aware not all vulnerabilities will affect serpentine, but we elect to try and eliminate vulnerable versions as a principle, as it's often less work to just upgrade a dependency than it is to maintain justifications for why a known security hole doesn't affect us.

### `cargo-deny`
cargo-deny both vets our dependencies for *known* vulnerabilities/malware, as well as restrictive licenses. We will never publish a version of serpentine where `cargo-deny` is failing with a security warning.

Note that this only covers *known* issues, there is always a window between a bad version being published and an advisory existing. We accept this risk rather than pre-emptively auditing dependencies, which is not realistic at our team size.

> [!WARNING]
> **This does not constitute a legal guarantee of the inclusion or lack of certain licenses in our dependency tree, and if license compliance is important for your team/company you should run your own analysis**

### `trivy`
Serpentine runs `trivy` on its own sidecar image, we aim to reduce the number of active vulnerabilities, but because most of the image is third-party code there is only so much we can do (for example we currently patch containerd to use a more recent version of a vulnerable dependency.)
