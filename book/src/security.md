# Security

> [!CAUTION]
> Serpentine is **NOT** a sandbox. 
> ***Never run untrusted pipeliens***

While serpentine does run workflows in containers it makes no guarantee about what privliges and level of isolation it grants them.
In general it should be assumed that a malicous serpentine pipeline can escape the containers and damage your system.

## What does serpentine protect against and what does it not?

However this does not mean serpentine isnt commited to security in general, its just important to understand that treath model:

### Serpentine does not protect against
* Malicous pipelines
* Malicous processes hijacking it containerd socket. (which is always stored at the same well known location.)
* Malicous processes connecting to its `Envoy` proxy

### Serpetnien does prevent
* Browsers connecting to its `Envoy` proxy.
* Lan devices (non-`127.0.0.1`) connecting to its `Envoy` proxy.
