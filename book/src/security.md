# Security

> [!CAUTION]
> Serpentine is **NOT** a sandbox. 
> ***Never run untrusted pipelines***

While serpentine does run workflows in containers it makes no guarantee about what privilieges and level of isolation it grants them.
In general it should be assumed that a malicious serpentine pipeline can escape the containers and damage your system.

In addition serpentine does expose a privileged docker container over tcp on your system (bound to localhost), it essential has the same access as a process with access to docker has, whether this is a actual concern or not depends on your environment, but for most users this is not a concern as they either already give their non-root users docker access, or in general its assumed you arent running malicious code on your own machine.
