# Containerd

> [!NOTE]
> Currently serpentine is linux only due to some limitations connected to docker on windows/mac running containers in a vm.
> This is planned to be worked around using a sidecar container in the future.

Serpentine uses [containerd](https://github.com/containerd/containerd) in much the same way [Buikit](https://github.com/moby/buildkit) does, this page documents the the high level flow of the code in `src/engine/containerd.rs`.

## Running containerd
Serpentine uses its own containerd image that downloads the `containerd` and `runc` binaries from github.

### Linux
On linux serpentine speaks to containerd directly over a unix socket.

```mermaid
flowchart LR
    subgraph Docker/Podman
        Containerd
    end
    Serpentine <-- "containerd.sock" --> Containerd 
```

## Snapshots
Snapshots are essentially a docker layer, they are modification to the file system.
They are generally created via the containerd api by pointing at a parent, running a action against them, then commiting them to a read-only snapshot.
These snapshots are the core part of serpentines `ContainerState` (which also holds other container data such as env vars).

Essentially, a specific `ContainerState` represents a specific layer in a dockerfile, while a snapshot represents a specific state of the file system. They are closely related, but different.
For example setting a environment variable does not require creating a new snapshot. 

## Pulling images
Pulling a image works similar to docker, serpentine will conntact a OCI compataible hub and pull the image manifest, then it will in parallel query containerd for if it contains the needed layers and if not stream them directly from the hub to the containerd (i.e serpentine never holds the entire image in memory itself). The `pull_image` function returns a `ContainerState` representing the image, to keep the abstractions consistent serpentine doesnt actually register a image with containerd, instead serpentine converts the image metadata to its generic `ContainerState` struct instead.

```mermaid
sequenceDiagram
    participant containerd
    participant serpentine
    participant docker_hub

    serpentine ->> docker_hub : get manifest
    docker_hub ->> serpentine : Image manifest

    par each layer
        serpentine ->> containerd : Get layer 
        containerd ->> serpentine : Ok or Error

        opt if missing
            serpentine ->> docker_hub : download layer
            docker_hub ->> serpentine : Layer data (Streaming)
            serpentine ->> containerd : Layer data (Streaming)

            serpentine ->> containerd : Commit layer
        end
    end

    serpentine ->> containerd : Prepare snapshot
    loop each layer
        serpentine ->> containerd : Apply layer
    end
    serpentine ->> containerd : Commit snapshot
```

## Running Containers
To run a container serpentine creates a new snapshot pointing to the given container states snapshot as it parents, queries containerd for the required setup info and creates a container and task according to the `ContainerState`.
Specifically in containerd a container is just a blueprint, essentially think of it as a image, while a task is a actual running process based on that container blueprint.

After the process is done serpentine cleans up the created container and commits the snapshot to a read-only one and constructs the new `ContainerState`.

> [!NOTE]
> Internal execs might skip the create/commit snapshot steps and instead execute multiple execs on the same snapshot, the public `exec`/`exec_output` always commits a new snapshot.

```mermaid
sequenceDiagram
    participant serpentine
    participant containerd
    participant process

    serpentine ->> containerd : Create new snapshot
    serpentine <<->> containerd : Get mounts for snapshot
    serpentine ->> containerd : Create new container
    serpentine ->> containerd : Create new task
    activate containerd
    serpentine ->> containerd : Start task
    containerd ->> process : Start process
    activate process
    serpentine ->> containerd : Wait on task
    process ->> containerd : Exit code
    deactivate process
    containerd ->> serpentine : Exit code
    deactivate containerd

    serpentine ->> containerd : Delete container
    serpentine ->> containerd : Commit Snapshot
```

### Linux
On linux uses the same mount where it mounts its socket to also mount in fifo pipes into containerd which the spawned container uses to stream its stdout/stderr to serpentine

```mermaid
sequenceDiagram
    participant containerd
    participant serpentine
    participant process

    note over serpentine : mkfifo(/tmp/serpentine/XYZ)
    serpentine ->> containerd : /run/serpentine/XYZ
    containerd ->> process : start process
    loop until done
        note over serpentine,process: using fifo pipes
        process ->> serpentine: output
    end
```
