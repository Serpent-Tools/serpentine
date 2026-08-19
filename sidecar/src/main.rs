//! Side car proxy, facilitates serpentines communication with containerd.
//!
//! As well as carries out operations that need to happen on the same host as containerd.
#![expect(
    clippy::expect_used,
    reason = "The proxy runs in a known container image and should have a stable environment."
)]

#[cfg(not(target_os = "linux"))]
compile_error!(
    "the serpentine sidecar only supports linux: it relies on containerd, linux namespaces, and unix path/mount semantics"
);

use std::error::Error;
use std::ffi::OsString;
use std::net::Ipv4Addr;
use std::ops::Deref;
use std::os::unix::fs::FileTypeExt;
use std::sync::Arc;

use async_fs::unix::DirEntryExt;
use nix::mount::MsFlags;
use nix::sys::stat::Mode;
use rand::TryRng;
use rust_cni::libcni as cni;
use serpentine_internal::network::{AbstractTopology, ConcreteTopology};
use serpentine_internal::sidecar::{MAGIC_NUMBER, Mount, PORT, Request};
use serpentine_internal::{TypedPathSerdeWrapper, read_postcard_frame, write_postcard_frame};
use tokio::io::{AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net;
use tokio_stream::StreamExt;
use typed_path::{PlatformPath, PlatformPathBuf};

/// The location serpentine connects to the containerd over
const SOCKET_LOCATION: &str = "/run/containerd.sock";

/// The size of container subnets
// 30 leaves 2 ips for hosts, which is all we need for each bridge.
// Internet bridge is gateway,container.
// Inter container bridges are just the two containers.
const SUBNET_SIZE: u8 = 30;

/// Prefix to use for container subnets
const SUBNET_PREFIX: Ipv4Addr = Ipv4Addr::from_octets([198, 18, 0, 0]);

/// The length of the prefix subnet.
const SUBNET_PREFIX_LENGTH: u8 = 15;

/// Convert a `PlatformPath` to the std path, always succeeds because we are on unix.
fn platform_to_std(path: &PlatformPath) -> &std::path::Path {
    use std::os::unix::ffi::OsStrExt;

    std::path::Path::new(std::ffi::OsStr::from_bytes(path.as_bytes()))
}

fn main() -> ! {
    let _ = simple_logger::init();

    spawn_containerd();

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("Failed to start tokio")
        .block_on(async {
            let listener = net::TcpListener::bind(("0.0.0.0", PORT))
                .await
                .expect("Failed to bind address");
            loop {
                let (socket, _addr) = listener.accept().await.expect("Failed to get connection");
                log::info!("Got connection");
                tokio::spawn(async move {
                    if let Err(err) = handle_connection(socket).await {
                        log::error!("{err}");
                    }
                });
            }
        })
}

/// Spawn the containerd process as a sub-process of serpentine
#[expect(
    clippy::zombie_processes,
    reason = "This needs to run for the duration of serpentine"
)]
fn spawn_containerd() {
    log::info!("Creating containerd config");
    std::fs::create_dir_all("/etc/containerd/").expect("Failed to create directories");

    // WARN: We specifically disable the more efficient overlayfs options for now as exporting them
    // to a OCI layer requires walking the lowerdirs as well as the upperdir, which is more complex.
    std::fs::write(
        "/etc/containerd/config.toml",
        r#"
version = 3
disabled_plugins = [
    "io.containerd.grpc.v1.cri",
    "io.containerd.cri.v1.images",
    "io.containerd.cri.v1.runtime",
    "io.containerd.cri.v1.images",
    "io.containerd.snapshotter.v1.native",
    "io.containerd.snapshotter.v1.btrfs",
    "io.containerd.snapshotter.v1.devmapper",
    "io.containerd.grpc.v1.images",
    "io.containerd.nri.v1.nri", 
    "io.containerd.transfer.v1.local",
    "io.containerd.grpc.v1.transfer",
    "io.containerd.podsandbox.controller.v1.podsandbox",
    "io.containerd.sandbox.store.v1.local",
    "io.containerd.sandbox.controller.v1.shim",
    "io.containerd.grpc.v1.sandbox-controllers",
    "io.containerd.grpc.v1.sandboxes",
    "io.containerd.streaming.v1.manager",
    "io.containerd.grpc.v1.streaming",
    "io.containerd.monitor.container.v1.restart",
    "io.containerd.image-verifier.v1.bindir",
    "io.containerd.service.v1.images-service",
    "io.containerd.snapshotter.v1.blockfile",
    "io.containerd.snapshotter.v1.erofs",
    "io.containerd.snapshotter.v1.zfs",
    "io.containerd.differ.v1.erofs",
    "io.containerd.mount-handler.v1.erofs",
    "io.containerd.service.v1.introspection-service",
    "io.containerd.grpc.v1.introspection",
    "io.containerd.tracing.processor.v1.otlp",
    "io.containerd.internal.v1.tracing",
    "io.containerd.ttrpc.v1.otelttrpc",
]
"#,
    )
    .expect("Failed to create containerd config");

    log::info!("Spawning containerd");
    std::process::Command::new("/bin/containerd")
        .args(["--address", SOCKET_LOCATION])
        .args(["--root", "/var/lib/containerd"])
        .args(["--state", "/run/containerd"])
        .args(["--log-level", "trace"])
        .spawn()
        .expect("Failed to start containerd");
}

/// Handle a incoming connection
async fn handle_connection(mut remote_socket: net::TcpStream) -> Result<(), Box<dyn Error>> {
    let mut magic_number = [0; MAGIC_NUMBER.len()];
    remote_socket.read_exact(&mut magic_number).await?;
    if magic_number != MAGIC_NUMBER.as_bytes() {
        return Err(format!("magic number {magic_number:?} != {MAGIC_NUMBER:?}").into());
    }

    let event = read_postcard_frame(&mut remote_socket).await?;

    match event {
        Request::Proxy => proxy_containerd(remote_socket).await,
        Request::CreateFifo => setup_fifo(remote_socket).await,
        Request::CreateNetwork(network) => create_network(remote_socket, network).await,
        Request::DeleteNetwork(network) => delete_network(network).await,
        Request::ExportFiles { mounts, path } => {
            export_files(remote_socket, mounts, path.with_platform_encoding()).await
        }
        Request::ImportFiles { mounts, path } => {
            import_files(remote_socket, mounts, path.with_platform_encoding()).await
        }
        Request::ExportLayer(mounts) => export_layer(remote_socket, mounts).await,
    }
}

/// Proxy messages between the given socket and `SOCKET_LOCATION`
async fn proxy_containerd(mut remote_socket: net::TcpStream) -> Result<(), Box<dyn Error>> {
    let mut containerd_socket = net::UnixStream::connect(SOCKET_LOCATION).await?;
    log::debug!("Connected to containerd, starting proxy");

    tokio::io::copy_bidirectional(&mut remote_socket, &mut containerd_socket).await?;

    Ok(())
}

/// Setup a fifo pipe and return its path to the client, then start reading its data to the client.
async fn setup_fifo(mut remote_socket: net::TcpStream) -> Result<(), Box<dyn Error>> {
    let parent = PlatformPathBuf::from("/run/serpentine");
    tokio::fs::create_dir_all(platform_to_std(&parent)).await?;
    let file = parent.join(uuid::Uuid::new_v4().to_string());

    let file = tokio::task::spawn_blocking(move || {
        nix::unistd::mkfifo(platform_to_std(&file), Mode::S_IRWXU | Mode::S_IRWXO).map(|()| file)
    })
    .await??;

    // Open the FIFO for reading with O_NONBLOCK so it succeeds immediately without waiting
    // for a writer. This guarantees the reader is registered in the kernel before we send
    // the path to the client (who passes it to containerd-shim as the write end).
    let mut file_reader =
        tokio::net::unix::pipe::OpenOptions::new().open_receiver(platform_to_std(&file))?;

    let unix_file = file.with_unix_encoding();
    write_postcard_frame(&TypedPathSerdeWrapper(unix_file), &mut remote_socket).await?;

    tokio::io::copy(&mut file_reader, &mut remote_socket).await?;

    tokio::fs::remove_file(platform_to_std(&file)).await?;

    Ok(())
}

/// Type that the cni config is stored as
type CniConfig = Arc<Box<dyn cni::api::CNI + Send + Sync + 'static>>;

/// A definition of a bridge connection between two namespaces
#[derive(Debug)]
struct BridgeDefinition {
    /// The name of the bridge, for example "cni-1234"
    name: String,
    /// The static ip address to assign to this side of the bridge
    ip: Ipv4Addr,
}

/// Create a cni based network namespace
async fn create_network(
    mut remote_socket: net::TcpStream,
    topology: AbstractTopology,
) -> Result<(), Box<dyn Error>> {
    log::debug!("Creating topology: {topology:?}");
    let topology = tokio::task::spawn_blocking(move || {
        realize_topology(topology, None).map_err(|err| err.to_string())
    })
    .await??;

    write_postcard_frame(&topology, &mut remote_socket).await?;

    Ok(())
}

/// Create a concrete topology from the given abstract one, with a optional parent bridge.
fn realize_topology(
    topology: serpentine_internal::network::AbstractTopology,
    parent_bridge: Option<BridgeDefinition>,
) -> Result<ConcreteTopology, Box<dyn Error>> {
    let ((), children) = topology.into_parts();

    let my_ip = if let Some(parent_bridge) = &parent_bridge {
        parent_bridge.ip
    } else {
        Ipv4Addr::LOCALHOST
    };

    let mut bridges = Vec::with_capacity(children.len());
    if let Some(parent_bridge) = parent_bridge {
        bridges.push(parent_bridge);
    }

    let mut new_children = Vec::with_capacity(children.len());

    for child in children {
        let mut bridge_name = uuid::Uuid::new_v4().to_string();
        bridge_name.truncate(15);
        let (_subnet, my_side, child_side) = pick_random_subnet()?;

        bridges.push(BridgeDefinition {
            name: bridge_name.clone(),
            ip: my_side,
        });

        let child = realize_topology(
            child,
            Some(BridgeDefinition {
                name: bridge_name,
                ip: child_side,
            }),
        )?;
        new_children.push(child);
    }

    let (namespace_path, adapters) = create_network_namespace(&bridges)?;
    let namespace = serpentine_internal::network::Namespace {
        path: namespace_path.into_boxed_str(),
        ip: my_ip,
        adapters,
    };

    let mut result = serpentine_internal::network::ConcreteTopology::new(namespace);
    for child in new_children {
        result.add_child(child);
    }

    Ok(result)
}

/// Create a cni network namespace with a random subnet.
///
/// Also creates the inter namespace bridges as defined by the `bridges` parameters
///
/// Returns the namespace path and the list of adapters created.
fn create_network_namespace(
    bridges: &[BridgeDefinition],
) -> Result<(String, Vec<serpentine_internal::network::Adapter>), Box<dyn Error + 'static>> {
    log::info!("Creating namespace");
    let ns_name = uuid::Uuid::new_v4().to_string();
    let raw_namespace = netns_rs::NetNs::new(ns_name.clone())?;
    let namespace_path = raw_namespace.path().to_string_lossy();
    let namespace =
        rust_cni::namespace::Namespace::new(ns_name.clone(), namespace_path.to_string());
    log::debug!("Created namespace {namespace_path}");

    let cni_config = cni::api::CNIConfig {
        path: vec!["/cni".to_owned()],
        ..Default::default()
    };
    let cni_config: CniConfig = Arc::new(Box::new(cni_config));

    let mut adapters = Vec::new();

    let loopback_json = r#"{
            "cniVersion": "1.1.0",
            "name": "loopback",
            "plugins": [{
              "type": "loopback"
            }]
        }"#;
    let loopback = cni::conf::ConfigFile::config_from_bytes(loopback_json.as_bytes())?;
    apply_network(
        Arc::clone(&cni_config),
        &namespace,
        "lo".to_owned(),
        loopback,
    )?;
    adapters.push(serpentine_internal::network::Adapter {
        ifname: "lo".into(),
        config_json: loopback_json.into(),
    });

    let (subnet, gateway, _ip2) = pick_random_subnet()?;
    let internet_bridge_json = format!(
        r#"{{
            "cniVersion": "1.1.0",
            "name": "bridge",
            "plugins": [{{
              "type": "bridge",
              "isGateway": true,
              "ipMasq": true,
              "bridge": "cni-{}",
              "ipam": {{
                "type": "host-local",
                "subnet": "{subnet}",
                "gateway": "{gateway}",
                "routes": [
                    {{ "dst": "0.0.0.0/0" }}
                ]
              }}
            }}]
        }}"#,
        // CNI spec requires bridge names to be 15 characters or less.
        ns_name.get(0..8).ok_or("Uuid wasnt pure ascii")?
    );
    let internet_bridge =
        cni::conf::ConfigFile::config_from_bytes(internet_bridge_json.as_bytes())?;
    apply_network(
        Arc::clone(&cni_config),
        &namespace,
        "eth0".to_owned(),
        internet_bridge,
    )?;
    adapters.push(serpentine_internal::network::Adapter {
        ifname: "eth0".into(),
        config_json: internet_bridge_json.into(),
    });

    for bridge in bridges {
        let bridge_json = format!(
            r#"{{
                "cniVersion": "1.1.0",
                "name": "bridge",
                "plugins": [{{
                  "type": "bridge",
                  "bridge": "{}",
                  "ipam": {{
                    "type": "static",
                    "addresses": [
                        {{
                            "address": "{}/{}"
                        }}
                    ]
                  }}
                }}]
            }}"#,
            bridge.name, bridge.ip, SUBNET_SIZE
        );
        let bridge_config = cni::conf::ConfigFile::config_from_bytes(bridge_json.as_bytes())?;
        apply_network(
            Arc::clone(&cni_config),
            &namespace,
            bridge.name.clone(),
            bridge_config,
        )?;
        adapters.push(serpentine_internal::network::Adapter {
            ifname: bridge.name.clone().into(),
            config_json: bridge_json.into(),
        });
    }

    Ok((namespace_path.into_owned(), adapters))
}

/// Apply the given network config to the given namespace.
fn apply_network(
    cni_config: CniConfig,
    namespace: &rust_cni::namespace::Namespace,
    adapter_name: String,
    config: cni::api::NetworkConfigList,
) -> Result<(), Box<dyn Error>> {
    log::debug!("Applying adapter {adapter_name}");
    let network = rust_cni::namespace::Network {
        cni: cni_config,
        config,
        ifname: adapter_name,
    };
    network.attach(namespace)?;

    Ok(())
}

// FIX: This is thread local while we have moved to a multi threading runtime
thread_local! {
    /// A hashset of the subnets that have already been used, to avoid collisions.
    #[expect(
        clippy::disallowed_types,
        reason = "(RefCell vs tokio::Mutex) This is a thread local variable used in non-async code, so we dont need the overhead of a mutex, and RefCell is much easier to use."
    )]
    static USED_SUBNETS: std::cell::RefCell<std::collections::HashSet<u32>> = std::cell::RefCell::default();
}

/// Pick a random non-internet subnet that's unlikely to be used already on the LAN/Host
///
/// Returns the subnet definition, as well as two usable ips in it.
fn pick_random_subnet() -> Result<(String, Ipv4Addr, Ipv4Addr), Box<dyn Error>> {
    const {
        assert!(
            SUBNET_SIZE > SUBNET_PREFIX_LENGTH,
            "subnet must be sub-set of prefix."
        );
        assert!(SUBNET_SIZE <= 30, "Subnet is too small to be usable");
    }

    let prefix_mask = subnet_mask(SUBNET_PREFIX_LENGTH);
    let target_mask = subnet_mask(SUBNET_SIZE);
    let random_mask = target_mask ^ prefix_mask;

    let subnet = loop {
        let random_ip: u32 = rand::rngs::SysRng.try_next_u32()?;
        let candidate = SUBNET_PREFIX.to_bits() | (random_ip & random_mask);
        let was_new = USED_SUBNETS.with_borrow_mut(|used_subnets| used_subnets.insert(candidate));
        if was_new {
            break candidate;
        }
        log::warn!("Subnet collision detected for {candidate}, retrying");
    };

    let ip1 = subnet | 0b01;
    let ip2 = subnet | 0b10;

    let subnet = Ipv4Addr::from_bits(subnet);
    let ip1 = Ipv4Addr::from_bits(ip1);
    let ip2 = Ipv4Addr::from_bits(ip2);

    Ok((format!("{subnet}/{SUBNET_SIZE}"), ip1, ip2))
}

/// Generate a subnet mask from a subnet length, for example `18` -> `11111111 11111111 11000000 00000000`
fn subnet_mask(mask_length: u8) -> u32 {
    u32::MAX << (32_u8.saturating_sub(mask_length))
}

/// Delete the given network interface.
async fn delete_network(network: ConcreteTopology) -> Result<(), Box<dyn Error>> {
    for namespace in network {
        let path = namespace.path.clone();
        tokio::task::spawn_blocking(move || {
            delete_namespace(&namespace).map_err(|err| err.to_string())
        })
        .await?
        .map_err(|err| format!("Failed to delete namespace {path}: {err}"))?;
    }

    Ok(())
}

/// Tear down a single namespace, removing all CNI adapters before deleting the namespace itself.
fn delete_namespace(
    ns: &serpentine_internal::network::Namespace,
) -> Result<(), Box<dyn Error + 'static>> {
    let path = std::path::Path::new(&*ns.path);
    let ns_name = path.file_name().ok_or("No filename in path")?;
    let ns_name = ns_name.to_string_lossy();
    let raw_namespace = netns_rs::NetNs::get(&*ns_name)?;
    let namespace_path = raw_namespace.path().to_string_lossy();
    let cni_namespace =
        rust_cni::namespace::Namespace::new(ns_name.to_string(), namespace_path.to_string());

    let cni_config: CniConfig = Arc::new(Box::new(cni::api::CNIConfig {
        path: vec!["/cni".to_owned()],
        ..Default::default()
    }));

    for adapter in ns.adapters.iter().rev() {
        let config = cni::conf::ConfigFile::config_from_bytes(adapter.config_json.as_bytes())?;
        let network = rust_cni::namespace::Network {
            cni: Arc::clone(&cni_config),
            config,
            ifname: adapter.ifname.to_string(),
        };
        if let Err(err) = network.remove(&cni_namespace) {
            log::error!("Failed to remove adapter {}: {err}", adapter.ifname);
        }
    }

    log::info!("Removing network namespace: {raw_namespace}");
    raw_namespace.remove()?;

    Ok(())
}

/// Export files from a given mount to the path
async fn export_files(
    mut remote_socket: net::TcpStream,
    mounts: Box<[Mount]>,
    path_to_export: PlatformPathBuf,
) -> Result<(), Box<dyn Error>> {
    log::debug!("Exporting files");
    let id = uuid::Uuid::new_v4().to_string();
    let mount_folder = PlatformPath::new("/run/serpentine/mounts/").join(&id);
    let mount_folder = DemountOnDrop(mount_folder);

    for mount in mounts {
        mount_containerd(mount, &mount_folder).await?;
    }

    log::debug!("Exporting {}", path_to_export.display());

    let relative = &path_to_export
        .strip_prefix("/")
        .unwrap_or(path_to_export.as_path());
    let full_path = mount_folder.join(relative);

    if let Err(err) = tokio::fs::metadata(&platform_to_std(&full_path)).await {
        log::error!(
            "Export pre-flight failed for {}: {err}",
            full_path.display()
        );
        remote_socket.write_u8(1).await?; // Error status
        write_postcard_frame(&err.to_string(), &mut remote_socket).await?;
        return Ok(());
    }

    remote_socket.write_u8(0).await?;
    serpentine_internal::read_disk_to_filesystem_stream(
        &full_path,
        PlatformPath::new(""),
        &mut remote_socket,
        |_path, _is_dir| true,
    )
    .await?;

    Ok(())
}

/// Write files from the socket into a mount given on the socket.
async fn import_files(
    mut remote_socket: net::TcpStream,
    mounts: Box<[Mount]>,
    destination_path: PlatformPathBuf,
) -> Result<(), Box<dyn Error>> {
    log::debug!("Importing files");

    let id = uuid::Uuid::new_v4().to_string();
    let mount_folder = PlatformPath::new("/run/serpentine/mounts/").join(&id);
    let mount_folder = DemountOnDrop(mount_folder);

    for mount in mounts {
        mount_containerd(mount, &mount_folder).await?;
    }

    log::debug!("Importing files into {}", destination_path.display());

    let relative = destination_path
        .strip_prefix("/")
        .unwrap_or(destination_path.as_path());
    let target: PlatformPathBuf = mount_folder.join(relative);
    serpentine_internal::read_filesystem_stream_to_disk(&target, &mut remote_socket, true).await?;

    Ok(())
}

/// Export a overlayfs upperdir to a tar stream.
///
/// # Based on
/// * Linux overlayfs: <https://www.kernel.org/doc/html/latest/filesystems/overlayfs.html>
/// * OCI spec: <https://github.com/opencontainers/image-spec/blob/main/layer.md>
async fn export_layer(
    remote_socket: impl AsyncWrite + Unpin + Send + Sync,
    mount: Mount,
) -> Result<(), Box<dyn Error>> {
    let upperdir_path = extract_overlayfs_uppder(mount)?;
    let mut tar = async_tar::Builder::new(remote_socket);

    tar.follow_symlinks(false);
    tar.mode(async_tar::HeaderMode::Complete);

    let mut walker = async_walkdir::WalkDir::new(platform_to_std(&upperdir_path));
    while let Some(Ok(entry)) = walker.next().await {
        let absolute_path = entry.path();
        let relative_path = absolute_path.strip_prefix(platform_to_std(&upperdir_path))?;

        if is_whiteout(&entry).await? {
            if let Some(original_name) = relative_path.file_name() {
                let mut whiteout_name = OsString::from(".wh.");
                whiteout_name.push(original_name);

                let whiteout_path = relative_path.with_file_name(whiteout_name);
                tar.append_data(
                    &mut async_tar::Header::new_gnu(),
                    whiteout_path,
                    tokio::io::empty(),
                )
                .await?;
            }
        } else if is_opaque(&entry).await? {
            let whiteout_path = relative_path.join(".wh..wh..opq");
            tar.append_data(
                &mut async_tar::Header::new_gnu(),
                whiteout_path,
                tokio::io::empty(),
            )
            .await?;
        } else {
            tar.append_path_with_name(&absolute_path, relative_path)
                .await?;
        }
    }

    tar.finish().await?;

    Ok(())
}

/// Check if the given entry is a whiteout
///
/// Docs: <https://www.kernel.org/doc/html/latest/filesystems/overlayfs.html#whiteouts-and-opaque-directories>
#[expect(
    clippy::filetype_is_file,
    reason = "We are explicitly checking for 'regular file' here"
)]
async fn is_whiteout(entry: &async_walkdir::DirEntry) -> Result<bool, Box<dyn Error>> {
    let file_type = entry.file_type().await?;
    if file_type.is_char_device() {
        let inode = entry.ino();
        let is_whiteout = nix::sys::stat::minor(inode) == 0 && nix::sys::stat::major(inode) == 0;

        Ok(is_whiteout)
    } else if file_type.is_file() {
        let path = entry.path();
        let is_whiteout = tokio::task::spawn_blocking(move || {
            let result = xattr::get(path, "trusted.overlay.whiteout").map_err(|x| x.to_string())?;
            Ok::<_, String>(result.is_some())
        })
        .await??;

        Ok(is_whiteout)
    } else {
        Ok(false)
    }
}

/// Check if the given entry is a opaque directory.
///
/// Docs: <https://www.kernel.org/doc/html/latest/filesystems/overlayfs.html#whiteouts-and-opaque-directories>
async fn is_opaque(entry: &async_walkdir::DirEntry) -> Result<bool, Box<dyn Error>> {
    let file_type = entry.file_type().await?;

    if file_type.is_dir() {
        let path = entry.path();
        let is_opaque = tokio::task::spawn_blocking(move || {
            let value = xattr::get(path, "trusted.overlay.opaque").map_err(|x| x.to_string())?;
            let is_opaque = value.is_some_and(|value| value == b"y");
            Ok::<_, String>(is_opaque)
        })
        .await??;

        Ok(is_opaque)
    } else {
        Ok(false)
    }
}

/// Extract the path to the mounts upperdir
fn extract_overlayfs_uppder(mount: Mount) -> Result<PlatformPathBuf, Box<dyn Error>> {
    for option in mount.options {
        if let Some(path) = option.strip_prefix("upperdir=") {
            return Ok(PlatformPathBuf::from(path));
        }
    }

    Err("upperdir option not found".into())
}

/// Call `nix::mount::unmount` on drop
struct DemountOnDrop(PlatformPathBuf);

impl Deref for DemountOnDrop {
    type Target = PlatformPath;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl Drop for DemountOnDrop {
    fn drop(&mut self) {
        let res = nix::mount::umount(platform_to_std(&self.0));
        if let Err(err) = res {
            log::error!("Failed to unmount {}: {err}", self.0.display());
        }
    }
}

/// Mount the provided containerd mount at the given location
async fn mount_containerd(mount: Mount, target: &PlatformPath) -> Result<(), Box<dyn Error>> {
    let (flags, data) = parse_containerd_mount_options(&mount.options);
    let fstype: Option<String> = if &*mount.type_ == "bind" {
        None
    } else {
        Some(mount.type_.to_string())
    };

    let relative_target = mount
        .target
        .strip_prefix("/")
        .unwrap_or(mount.target.as_path())
        .with_platform_encoding();

    let target = target.join(relative_target);
    tokio::fs::create_dir_all(platform_to_std(&target)).await?;
    tokio::task::spawn_blocking(move || {
        let target = target;
        let source = mount.source.with_platform_encoding();

        nix::mount::mount(
            Some(platform_to_std(&source)),
            platform_to_std(&target),
            fstype.as_deref(),
            flags,
            Some(&*data),
        )
    })
    .await??;

    Ok(())
}

/// Parse the `options` array given in containerd mounts into low level linux mount flags and mount
/// data strings
fn parse_containerd_mount_options(options: &[Box<str>]) -> (MsFlags, String) {
    let mut flags = MsFlags::empty();
    let mut data = Vec::new();

    for option in options {
        match &**option {
            "ro" => flags |= MsFlags::MS_RDONLY,
            "rw" => {} // default
            "bind" => flags |= MsFlags::MS_BIND,
            "rbind" => flags |= MsFlags::MS_BIND | MsFlags::MS_REC,
            "nosuid" => flags |= MsFlags::MS_NOSUID,
            "nodev" => flags |= MsFlags::MS_NODEV,
            "noexec" => flags |= MsFlags::MS_NOEXEC,
            "remount" => flags |= MsFlags::MS_REMOUNT,
            "private" => flags |= MsFlags::MS_PRIVATE,
            "rprivate" => flags |= MsFlags::MS_PRIVATE | MsFlags::MS_REC,
            "shared" => flags |= MsFlags::MS_SHARED,
            "rshared" => flags |= MsFlags::MS_SHARED | MsFlags::MS_REC,
            "slave" => flags |= MsFlags::MS_SLAVE,
            "rslave" => flags |= MsFlags::MS_SLAVE | MsFlags::MS_REC,
            other => data.push(other),
        }
    }

    (flags, data.join(","))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subnet_mask_works() {
        assert_eq!(subnet_mask(16), 0b1111_1111_1111_1111_0000_0000_0000_0000);
        assert_eq!(subnet_mask(18), 0b1111_1111_1111_1111_1100_0000_0000_0000);
    }
}
