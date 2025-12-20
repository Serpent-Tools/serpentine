# Security

> [!CAUTION]
> Serpentine is **NOT** a sandbox. 
> ***Never run untrusted pipeliens***

While serpentine does run workflows in containers it makes no guarantee about what privliges and level of isolation it grants them.
In general it should be assumed that a malicous serpentine pipeline can escape the containers and damage your system.

In addition serpentine does expose a priviliged docker container over tcp on your system (bound to localhost), it essential has the same access as a process with access to docker has, wether this is a actual concern or not depends on your environment, but for most users this is not a concern as they either already give their non-root users docker access, or in general its assumed you arent running malicous code on your own machine.
