# Containerd

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

### Windows/Mac
On windows and mac, due to not being able to mount unix sockets out of the container (and containerd not supporting tcp natively) we deploy a extra sidecar container using [Envoy](https://www.envoyproxy.io/) to provide secure access to the socket using auth tokens, this is very important to prevent [Localhost Drive-by](https://en.wikipedia.org/wiki/Drive-by_download)
```mermaid
flowchart LR
    subgraph Docker
        Envoy
        Containerd
    end

    Serpentine <-- "localhost" --> Envoy
    Envoy <-- "containerd.sock" --> Containerd 
```

## Pulling images
Pulling a image works similar to docker, serpentine will conntact a OCI compataible hub and pull the image manifest, then it will in parallel query containerd for if it contains the needed layers and if not stream them directly from the hub to the containerd (i.e serpentine never holds the entire image in memory itself). The `pull_image` function returns a `ContainerState` which is a snapshot key, specificaly the snapshot key in the exact version info of the image in this case.

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
