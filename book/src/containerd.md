# Containerd

Serpentine uses [containerd](https://github.com/containerd/containerd) in much the same way [Buikit](https://github.com/moby/buildkit) does, this page documents the the high level flow of the code in `src/engine/containerd.rs`.

## Running containerd
Serpentine uses its own containerd image that downloads the `containerd` and `runc` binaries from github, and a few other supporting tools.
It also loads up a small `sidecar` proxy that primarly proxies a tcp port to the containerd unix socket, but also executes a few smaller tasks in the container, for example setting up and streaming stdout and stderr pipes.

```mermaid
flowchart LR
    subgraph Docker 
        Sidecar <-- containerd.sock --> Containerd
    end
    Serpentine <-- 8000/tcp --> Sidecar
```

> [!NOTE]
> The blow diagrams leave out the proxy where it isnt relevant.

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
        opt if missing
            serpentine ->> docker_hub : download layer
            docker_hub ->> serpentine : Layer data (Streaming)
            serpentine ->> containerd : Layer data (Streaming)

            serpentine ->> containerd : Commit layer
        end
    end

    loop each layer
        opt if mising 
            serpentine ->> containerd : Prepare snapshot
            serpentine ->> containerd : Apply layer
            serpentine ->> containerd : Commit snapshot
        end
    end
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
    note over serpentine : Setup networking (see below)
    serpentine ->> containerd : Create new container

    note over serpentine : Set up stdout streaming (see below)

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

    serpentine ->> containerd : Commit Snapshot
```

### Getting Stdout/Stderr

Containerd only exposes stdout/stderr via unix fifo pipe files, hence the sidecar will construct the files, and return the internal file path to serpentine (which will then inform containerd of them), and then start streaming any data on the pipes to serpentine.

```mermaid
sequenceDiagram
    participant containerd
    participant serpentine
    participant sidecar
    participant process

    note over serpentine,containerd: over sidecard proxy

    serpentine ->>+ sidecar : Create stdout stream
    note over sidecar : mkfifo(/run/serpentine/XYZ)
    sidecar ->> serpentine : /run/serpentine/XYZ
    serpentine ->> containerd : CreateTask(stdout=/run/serpentine/XYZ)
    serpentine ->> containerd : StartTask
    containerd ->> process : start process
    loop until done
        note over sidecar,process: using fifo pipes
        process ->> sidecar: output
        sidecar ->> serpentine: output
    end
    deactivate sidecar
```

### Nettwork access
To provide isolated network access to containers we use [CNI](https://www.cni.dev/) to attach a loopback and a bridge adapter to it. The sidecar will construct a network namespace and use CNI to setup the needed adapters.
Serpetine will only create a new name-space when required, and will re-use once not actively in use by a container when running steps, on exit a serpentine process will instruct the sidecar to clean up the network namespaces created this run.

```mermaid
sequenceDiagram
    participant linux
    participant sidecar
    participant serpentine
    participant containerd
    participant process
    participant lan

    note over serpentine,containerd: over sidecard proxy
    opt If no free network
        serpentine ->>+ sidecar : Create network
        sidecar ->> linux : Create namespace
        sidecar ->> linux : (via cni) setup adapters
        sidecar ->>- serpentine : /run/serpentine/XYZ
    end

    serpentine ->> containerd : CreateTask(namespace=/run/serpetine/XYZ)
    serpentine ->> containerd : StartTask
    containerd ->> process : start process in namespace
    loop until done
        process <<->> lan : network traffic over bridge plugin
    end
```

## Export
