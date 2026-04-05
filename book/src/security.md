# Security

> [!CAUTION]
> Serpentine is **NOT** a sandbox. 
> ***Never run untrusted pipelines***

While serpentine does run workflows in containers it makes no guarantee about what privilieges and level of isolation it grants them.
In general it should be assumed that a malicious serpentine pipeline can escape the containers and damage your system.

In addition serpentine does expose a privileged docker container over tcp on your system (bound to localhost), it essential has the same access as a process with access to docker has, whether this is a actual concern or not depends on your environment, but for most users this is not a concern as they either already give their non-root users docker access, or in general its assumed you arent running malicious code on your own machine.

## Security of serpentine itself

Serpentine employs a combination of `cargo-vet`, `carg-deny` and `trivy` to vet ist dependencies, it uses each of these tools slightly differently. 

> [!NOTE]
> Yes we are aware not all vulnurabilities will affect serpentine, but we elect to try and elminate vulnurabilitie versions as a princible, as its often less work to just upgrade a dependency than it is to maintain justitications for why a known security hole doesnt affect us.

### `cargo-deny`
cargo-deny both vets our dependencies for *known* vulnurabilities/malware, as well as restrictive licenses. We will never publish a version of serpentine where `cargo-deny` is failling with a security warning.

> [!WARNING]
> **This does not constitute a legal guarantee of the inclusion or lack of certain licenses in our dependency tree, and if license compliance is important for your team/company you should run your own analysis**

### `trivy`
Serpentine runs `trivy` on its own sidecar image, we aim to reduce the number of active vulnurabilities, but because most of the image is third-party code there is only so much we can do (for example we currently patch containerd to use a more recent version of a vulnurable dependency.)

### `carg-vet`
We employ cargo-vet, *partially*, ideally we would be able to establish audits for all dependencies, and while we do import audits for a wide range of organizations, and even explicitly trust the more popular crates we still have around 100 exempt crates (but around 100 fully audited.)

This is a needed compromise as we are a small team and cant spend hours upon hours auditing thousands of lines of dependencies (the current "backlog" is 794k lines of code). 
we instead use `cargo-vet` more to set a new baseline, moving forward we will not upgrade dependencies or add new ones without them passing cargo-vet, in other words we will not add more exemptions, and ideally not add more trust entries.

In general we assume that ideally `cargo-deny` will catch any malware/vulnurabilities in a reasonable, and ultimately serpentine only requires "safe-to-run", as we are a dev tool binary and as noted above do not expect to be run on evil input, as such the only issues we aim for `cargo-vet` to reduce are panics/segfaults and straight up malware.
