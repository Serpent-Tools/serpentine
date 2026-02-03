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
use serpentine_internal::sidecar::{MAGIC_NUMBER, Mount, PORT, RequestKind};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net;

/// The location serpentine connects to the containerd over
const SOCKET_LOCATION: &str = "/run/containerd.sock";

/// The size of container subnets
const SUBNET_SIZE: u8 = 28;

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
async fn handle_connection(mut remote_socket: net::TcpStream) -> Result<(), Box<dyn Error>> {
    let mut magic_number = vec![0; MAGIC_NUMBER.len()];
    remote_socket.read_exact(&mut magic_number).await?;
    if magic_number != MAGIC_NUMBER.as_bytes() {
        return Err(format!("magic number {magic_number:?} != {MAGIC_NUMBER:?}",).into());
    }

    let event = remote_socket.read_u8().await?;
    let event = RequestKind::try_from(event).map_err(|()| "Invalid event")?;

    match event {
        RequestKind::Proxy => proxy_containerd(remote_socket).await,
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
async fn setup_fifo(mut remote_socket: net::TcpStream) -> Result<(), Box<dyn Error>> {
    let parent = PathBuf::from("/run/serpentine");
    tokio::fs::create_dir_all(&parent).await?;
    let file = parent.join(uuid::Uuid::new_v4().to_string());

    nix::unistd::mkfifo(&file, Mode::S_IRWXU | Mode::S_IRWXO)?;

    let file_bytes = file.to_string_lossy();
    let file_bytes = file_bytes.as_bytes();

    serpentine_internal::write_length_prefixed(&mut remote_socket, file_bytes).await?;

    let mut file_reader = tokio::fs::File::open(&file).await?;
    tokio::io::copy(&mut file_reader, &mut remote_socket).await?;

    tokio::fs::remove_file(file).await?;

    Ok(())
}

/// Type that the cni config is stored as
type CniConfig = Arc<Box<dyn cni::api::CNI + Send + Sync + 'static>>;

/// Create a cni based network namespace
async fn create_network(mut remote_socket: net::TcpStream) -> Result<(), Box<dyn Error>> {
    log::info!("Creating network");
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

    let loopback = cni::conf::ConfigFile::config_from_bytes(
        r#"{
            "cniVersion": "1.1.0",
            "name": "loopback",
            "plugins": [{
              "type": "loopback"
            }]
        }"#
        .as_bytes(),
    )?;

    let (subnet, gateway) = pick_random_subnet()?;
    let bridge = cni::conf::ConfigFile::config_from_bytes(
        format!(
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
            ns_name.get(0..8).ok_or("Uuid wasnt pure ascii")?
        )
        .as_bytes(),
    )?;

    apply_network(
        Arc::clone(&cni_config),
        &namespace,
        "lo".to_owned(),
        loopback,
    )?;
    apply_network(cni_config, &namespace, "eth0".to_owned(), bridge)?;

    serpentine_internal::write_length_prefixed(&mut remote_socket, namespace_path.as_bytes())
        .await?;

    Ok(())
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

/// Pick a random non-internet subnet thats unlikely to be used already on the LAN/Host
fn pick_random_subnet() -> Result<(String, Ipv4Addr), Box<dyn Error>> {
    let random_ip: u32 = rand::rngs::OsRng.try_next_u32()?;

    const {
        assert!(
            SUBNET_SIZE > SUBNET_PREFIX_LENGTH,
            "subnet must be sub-set of prefix."
        );
    }

    let prefix_mask = subnet_mask(SUBNET_PREFIX_LENGTH);
    let target_mask = subnet_mask(SUBNET_SIZE);
    let random_mask = target_mask ^ prefix_mask;

    let subnet = SUBNET_PREFIX.to_bits() | (random_ip & random_mask);
    let gateway = subnet | 0b1;
    let subnet = Ipv4Addr::from_bits(subnet);
    let gateway = Ipv4Addr::from_bits(gateway);

    Ok((format!("{subnet}/{SUBNET_SIZE}"), gateway))
}

/// Generate a subnet mask from a subnet length, for example `18` -> `11111111 11111111 11000000 00000000`
fn subnet_mask(mask_length: u8) -> u32 {
    u32::MAX << (32_u8.saturating_sub(mask_length))
}

/// Delete the given network interface.
async fn delete_network(mut remote_socket: net::TcpStream) -> Result<(), Box<dyn Error>> {
    let path = serpentine_internal::read_length_prefixed_string(&mut remote_socket).await?;
    let path = PathBuf::from(path);

    let namespace = path.file_name().ok_or("No filename in path")?;
    let namespace = namespace.to_string_lossy();
    let namespace = netns_rs::NetNs::get(namespace)?;
    log::info!("Removing network namespace: {namespace}");
    namespace.remove()?;

    Ok(())
}

/// Export files from a given mount to the path
async fn export_files(mut remote_socket: net::TcpStream) -> Result<(), Box<dyn Error>> {
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

    Ok(())
}

/// Write files from the socket into a mount given on the socket.
async fn import_files(mut remote_socket: net::TcpStream) -> Result<(), Box<dyn Error>> {
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
    type Target = PathBuf;

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
    let fstype = if mount.type_ == "bind" {
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
fn parse_containerd_mount_options(options: &[String]) -> (MsFlags, String) {
    let mut flags = MsFlags::empty();
    let mut data = Vec::new();

    for opt in options {
        match opt.as_str() {
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
