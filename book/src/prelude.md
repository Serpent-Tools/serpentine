# Builtins/Prelude

These nodes are available at all times in the global scope.

> [!IMPORTANT]
> Some of these docs refer to "first argument", "second argument" in a way that ignores their actual first argument, usually a container value.
> In other words in the expression `A > Foo(B, C)`, "first argument" will usually refer to `B`, while it is actually `A`.
> (hopefully the examples in each section make it clear.)

## Pure data nodes

### `Join`

This node takes any number of string arguments and join them together.
For example `Join("hello ", "world")` results in `"hello world"`

## Container nodes

"Containers" in serpentine, or more accurately containerd snapshots (essentially docker layers) are immutable values, the below nodes take in a container and return a new one.

### `Image`

This node will pull a image from docker hub, or similar, and construct a serpentine "container" from them.
For example `Image("rust:latest")` is similar to the dockerfile `FROM rust:latest`

### `Exec`

Exec takes a command to execute, and executes it, the command is ran with `/bin/sh -c`, meaning you can use shell syntax, 
for example `... > Exec("echo hello > foo.txt")`

### `ExecOutput`

Similar to `Exec` but instead of returning the resulting container instead returns the stdout/stderr of the command.

### `WorkingDir`

Sets the working directory that `Exec`/`ExecOutput` are relative to, as well as `Export` and `With`.

This call itself is relative, so `... > WorkingDir("./foo") > WorkingDir("./bar")` will put the working directory as `./foo/bar`

### `Env`

Set a environment variable in the container, for example `PATH`, `HOME`, etc.
Takes the variable to set as the first argument, and the value as the second, 
for example `... > Env("PATH", "/bin")`

### `GetEnv`

Gets a environment variable from a container, for example `> GetEnv("PATH")`, this is useful for when modifying them, for example:
```snek
def AddPath(container, folder) {
    new_path = Join(GetEnv(container, "PATH"), ":", folder);
    return container > Env("PATH", new_path);
}
```

### `Export`

This node exports a file/folder from a container, it takes a path relative to the working directory, and returns the **content** of the file or folder.

### `With`

This copies the given file/folder into the container with the given name.

Files are copied into containers with "permissive" permissions, i.e `0o777`.

#### Files

Files are simply their content:
```snek
foo = ... > Export("foo.txt");
bar = ... > With(foo, "bar.txt") > Exec("cat ./bar.txt");
```

#### Folders
Folders are also their *content*, which means doing `With(..., ".")` will copy the contents into the current folder, while say `With(..., "./bar")` will create a new folder with the contents (if `bar` didnt exist).

```snek
foo = ... > Export("foo");

bar1 = ... > With(foo, "bar") > Exec("cat ./bar/inner.txt");
bar2 = ... > With(foo, ".") > Exec("cat ./inner.txt");
```

## Host nodes

### `FromHost`

Reads the given file/folder from the *host* file system, a common pattern will be reading and using source code:
```snek
source_code = FromHost(".");
build = ... > With(source_code, ".") > Exec("...");
```

This also respects gitignores, meaning you do not need to maintain a dedicated "serpentine-ignore".

### `ToHost`

Copies the given file or folder onto the host system, for example:
```snek
export DEFAULT = ... > Export("./build/foo") > ToHost("./out/foo");
```

> [!CAUTION]
> The permissions (and other metadata) set on the exported files is set to the "platform defaults", it's therefore extremely important to be careful when exporting big directories to replace content that was imported, for example if you run `cargo fmt` in serpentine.

## Service nodes

Services in serpentine work differently than one would expect from other ci products, specifically while services are attached to a container and stay with the container, the actual processes live only for the duration of one node.

Specifically serpentine will spin up and down services at each `Exec`, this is to ensure warm and cold cache runs behave the same and is a nice compromise to ensure services can still be cached. In practice we have found it's rare you need a service for more than one `Exec`.
Another benefit is that you get all the benefits of containers in serpentine for services as well, specifically forking the graph.

> [!TIP]
> Services act very much like a subclass of containers, and hence all the container nodes work on services as well.

### `ImageService`

This is the service version of `Image`, it returns a service and reads `ENTRYPOINT`/`CMD` from the pulled image.

> [!NOTE]
> Since technically speaking `HEALTHCHECK` is not part of the oci-spec serpentine does not read it.

### `WithService`

This attaches a service to a container, the first argument is the service to attach and the second is the hostname to attach it under:

```snek
postgres = ImageService("docker.io/library/postgres:16")
    > Env("POSTGRES_PASSWORD", "test");

foo = Image("docker.io/library/postgres:16")
    > Env("PGPASSWORD", "test")
    > WithService(postgres, "db")
    > Exec("psql -h db -U postgres -c 'CREATE TABLE users (id SERIAL PRIMARY KEY, name TEXT)'");
```

### `HealthCheck`

Since healthchecks don't come with images its often very important to define a healthcheck so that your CI doesnt become flaky due to running on slower hardware etc.

The first argument is the healthcheck command, and the second is the number of seconds to wait on the service maximum.

```snek
postgres = ImageService("docker.io/library/postgres:16")
    > Env("POSTGRES_PASSWORD", "test")
    > HealthCheck("pg_isready -h localhost -U postgres", 30);
```

> [!WARNING]
> The healthcheck command runs in the same namespaces and mutable filesystem as the main service command, take care to not modify the container state with it.

### `ToService`

This node converts a container to a service, It takes the entrypoint as an argument:
```snek
base = Image("quay.io/toolbx-images/alpine-toolbox:3.21") > Exec("apk add curl");

python_server = base
    > Exec("apk add python3")
    > ToService("python3 -m http.server 8000")
    > HealthCheck("curl -f http://localhost:8000", 5);

ping_python = base 
    > WithService(python_server, "server")
    > Exec("curl -f http://server:8000");
```
