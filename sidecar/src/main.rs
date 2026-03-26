//! Side car proxy, facilitates serpentines communication with containerd.
//!
//! As well as carries out operations that need to happen on the same host as containerd.
#![expect(
    clippy::expect_used,
    reason = "The proxy runs in a known container image and should have a stable environment."
)]

use std::error::Error;
use std::net::Ipv4Addr;
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use nix::mount::MsFlags;
use nix::sys::stat::Mode;
use rand::TryRngCore;
use rust_cni::libcni as cni;
use serpentine_internal::WireFormat;
use serpentine_internal::sidecar::{MAGIC_NUMBER, Mount, PORT, RequestKind};
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufWriter};
use tokio::net;

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

fn main() -> ! {
    let _ = simple_logger::init();

    spawn_containerd();

    tokio::runtime::Builder::new_current_thread()
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
async fn handle_connection(remote_socket: net::TcpStream) -> Result<(), Box<dyn Error>> {
    let mut remote_socket = BufWriter::new(remote_socket);
    let mut magic_number = vec![0; MAGIC_NUMBER.len()];
    remote_socket.read_exact(&mut magic_number).await?;
    if magic_number != MAGIC_NUMBER.as_bytes() {
        return Err(format!("magic number {magic_number:?} != {MAGIC_NUMBER:?}").into());
    }

    let event = remote_socket.read_u8().await?;
    let event = RequestKind::try_from(event).map_err(|()| "Invalid event")?;

    match event {
        RequestKind::Proxy => proxy_containerd(remote_socket.into_inner()).await,
        RequestKind::CreateFifo => setup_fifo(remote_socket).await,
        RequestKind::CreateNetwork => create_network(remote_socket).await,
        RequestKind::DeleteNetwork => delete_network(remote_socket).await,
        RequestKind::ExportFiles => export_files(remote_socket).await,
        RequestKind::ImportFiles => import_files(remote_socket).await,
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
async fn setup_fifo(mut remote_socket: BufWriter<net::TcpStream>) -> Result<(), Box<dyn Error>> {
    let parent = PathBuf::from("/run/serpentine");
    tokio::fs::create_dir_all(&parent).await?;
    let file = parent.join(uuid::Uuid::new_v4().to_string());

    nix::unistd::mkfifo(&file, Mode::S_IRWXU | Mode::S_IRWXO)?;

    // Start opening the FIFO for reading in a blocking thread BEFORE sending the path.
    // File::open on a FIFO blocks until a writer appears, but starting it early ensures the
    // reader is registered by the time containerd-shim tries to open the write end.
    let file_for_open = file.clone();
    let open_handle = tokio::task::spawn_blocking(move || std::fs::File::open(file_for_open));

    let file_bytes = file.to_string_lossy();
    let file_bytes = file_bytes.as_bytes();

    serpentine_internal::write_length_prefixed(&mut remote_socket, file_bytes).await?;
    remote_socket.flush().await?;

    let mut file_reader = tokio::fs::File::from_std(open_handle.await??);
    tokio::io::copy(&mut file_reader, &mut remote_socket).await?;
    remote_socket.flush().await?;

    tokio::fs::remove_file(file).await?;

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
    mut remote_socket: BufWriter<net::TcpStream>,
) -> Result<(), Box<dyn Error>> {
    let topology = serpentine_internal::network::AbstractTopology::read(&mut remote_socket).await?;
    log::debug!("Creating topology: {topology:?}");
    let topology = realize_topology(topology, None)?;
    topology.write(&mut remote_socket).await?;
    remote_socket.flush().await?;

    Ok(())
}

/// Create a concrete topology from the given abstract one, with a optional parent bridge.
fn realize_topology(
    topology: serpentine_internal::network::AbstractTopology,
    parent_bridge: Option<BridgeDefinition>,
) -> Result<serpentine_internal::network::ConcreteTopology, Box<dyn Error>> {
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

thread_local! {
    /// A hashset of the subnets that have already been used, to avoid collisions.
    #[expect(
        clippy::disallowed_types,
        reason = "(RefCell vs tokio::Mutex) This is a thread local variable used in non-async code, so we dont need the overhead of a mutex, and RefCell is much easier to use."
    )]
    static USED_SUBNETS: std::cell::RefCell<std::collections::HashSet<u32>> = std::cell::RefCell::default();
}

/// Pick a random non-internet subnet thats unlikely to be used already on the LAN/Host
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

    let random_ip: u32 = rand::rngs::OsRng.try_next_u32()?;

    let prefix_mask = subnet_mask(SUBNET_PREFIX_LENGTH);
    let target_mask = subnet_mask(SUBNET_SIZE);
    let random_mask = target_mask ^ prefix_mask;

    let subnet = SUBNET_PREFIX.to_bits() | (random_ip & random_mask);
    let was_new = USED_SUBNETS.with_borrow_mut(|used_subnets| used_subnets.insert(subnet));
    if !was_new {
        log::warn!("Subnet collision detected for {subnet}, retrying");
        return pick_random_subnet();
    }

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
async fn delete_network(
    mut remote_socket: BufWriter<net::TcpStream>,
) -> Result<(), Box<dyn Error>> {
    let network = serpentine_internal::network::ConcreteTopology::read(&mut remote_socket).await?;

    for namespace in network {
        delete_namespace(&namespace)
            .map_err(|err| format!("Failed to delete namespace {}: {err}", namespace.path))?;
    }

    Ok(())
}

/// Tear down a single namespace, removing all CNI adapters before deleting the namespace itself.
fn delete_namespace(
    ns: &serpentine_internal::network::Namespace,
) -> Result<(), Box<dyn Error + 'static>> {
    let path = Path::new(&*ns.path);
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
async fn export_files(mut remote_socket: BufWriter<net::TcpStream>) -> Result<(), Box<dyn Error>> {
    log::debug!("Exporting files");
    let mount_folder =
        PathBuf::from("/run/serpentine/mounts/").join(uuid::Uuid::new_v4().to_string());
    let mount_folder = DemountOnDrop(mount_folder);

    let mount_count = serpentine_internal::read_u64_length_encoded(&mut remote_socket).await?;
    for _ in 0..mount_count {
        let mount = Mount::read(&mut remote_socket).await?;
        mount_containerd(&mount, &mount_folder).await?;
    }

    let path_to_export =
        serpentine_internal::read_length_prefixed_string(&mut remote_socket).await?;
    log::debug!("Exporting {path_to_export}");

    let full_path = mount_folder.join(path_to_export.strip_prefix("/").unwrap_or(&path_to_export));

    if let Err(err) = tokio::fs::metadata(&full_path).await {
        log::error!(
            "Export pre-flight failed for {}: {err}",
            full_path.display()
        );
        remote_socket.write_u8(1).await?; // Error status
        remote_socket
            .write_u8(err.raw_os_error().unwrap_or(0).try_into().unwrap_or(0))
            .await?;
        serpentine_internal::write_length_prefixed(&mut remote_socket, err.to_string().as_bytes())
            .await?;
        remote_socket.flush().await?;
        return Ok(());
    }

    remote_socket.write_u8(0).await?;
    serpentine_internal::read_disk_to_filesystem_stream(
        &full_path,
        Path::new(""),
        &mut remote_socket,
        |_path, _is_dir| true,
    )
    .await?;
    remote_socket.flush().await?;

    Ok(())
}

/// Write files from the socket into a mount given on the socket.
async fn import_files(mut remote_socket: BufWriter<net::TcpStream>) -> Result<(), Box<dyn Error>> {
    log::debug!("Importing files");

    let mount_folder =
        PathBuf::from("/run/serpentine/mounts/").join(uuid::Uuid::new_v4().to_string());
    let mount_folder = DemountOnDrop(mount_folder);

    let mount_count = serpentine_internal::read_u64_length_encoded(&mut remote_socket).await?;
    for _ in 0..mount_count {
        let mount = Mount::read(&mut remote_socket).await?;
        mount_containerd(&mount, &mount_folder).await?;
    }

    let destination_path =
        serpentine_internal::read_length_prefixed_string(&mut remote_socket).await?;
    log::debug!("Importing files into {destination_path}");
    serpentine_internal::read_filesystem_stream_to_disk(
        &mount_folder.join(
            destination_path
                .strip_prefix('/')
                .unwrap_or(&destination_path),
        ),
        &mut remote_socket,
        true,
    )
    .await?;

    Ok(())
}

/// Call `nix::mount::unmount` on drop
struct DemountOnDrop(PathBuf);

impl Deref for DemountOnDrop {
    type Target = Path;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl Drop for DemountOnDrop {
    fn drop(&mut self) {
        let res = nix::mount::umount(&*self.0);
        if let Err(err) = res {
            log::error!("Failed to unmount {}: {err}", self.0.display());
        }
    }
}

/// Mount the provided containerd mount at the given location
async fn mount_containerd(mount: &Mount, target: &Path) -> Result<(), Box<dyn Error>> {
    let (flags, data) = parse_containerd_mount_options(&mount.options);
    let fstype = if &*mount.type_ == "bind" {
        None
    } else {
        Some(&*mount.type_)
    };

    let target = target.join(mount.target.strip_prefix("/").unwrap_or(&mount.target));
    tokio::fs::create_dir_all(&target).await?;

    nix::mount::mount(Some(&*mount.source), &target, fstype, flags, Some(&*data))?;

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
