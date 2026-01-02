//! Side car proxy, facilities serpentines communication with containerd.
//!
//! As well as carries out operations that need to happen on the same host as containerd.
#![expect(
    clippy::expect_used,
    reason = "The proxy runs in a known container image and should have a stable environment."
)]

use std::error::Error;
use std::path::PathBuf;

use nix::sys::stat::Mode;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net;

/// The location serpentine connects to the containerd over
const SOCKET_LOCATION: &str = "/run/containerd.sock";

/// The port serpentine listend on
const PORT: u16 = 8000;

fn main() -> ! {
    let _ = simple_logger::init();

    spawn_containerd();
    setup_networking();

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
                log::info!("Got connection, proxying");
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
    log::info!("Spawning containerd");
    std::process::Command::new("containerd")
        .args(["--address", SOCKET_LOCATION])
        .args(["--root", "/var/lib/containerd"])
        .args(["--state", "/run/containerd"])
        .args(["--log-level", "trace"])
        .spawn()
        .expect("Failed to start containerd");
}

/// Setup the default networking config for containers
fn setup_networking() {
    log::info!("Creating networking config");

    std::fs::create_dir_all("/etc/cni/net.d").expect("Failed to create directories");

    std::fs::write(
        "/etc/cni/net.d/00-loopback",
        r#"
{
  "cniVersion": "1.0.0",
  "name": "lo",
  "type": "loopback"
}
"#,
    )
    .expect("Failed to create network config");
}

/// Handle a incoming connection
async fn handle_connection(mut remote_socket: net::TcpStream) -> Result<(), Box<dyn Error>> {
    let magic_should_be = "danger noodle".as_bytes();
    let mut magic_number = vec![0; magic_should_be.len()];
    remote_socket.read_exact(&mut magic_number).await?;
    if magic_number != magic_should_be {
        return Err(format!("magic number {magic_number:?} != {magic_should_be:?}").into());
    }

    let event = remote_socket.read_u8().await?;

    match event {
        0 => proxy_containerd(remote_socket).await,
        1 => setup_fifo(remote_socket).await,
        _ => Err(format!("Unknown event kind {event}").into()),
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

    remote_socket
        .write_u8(
            file_bytes
                .len()
                .try_into()
                .expect("The file path should never be above 255"),
        )
        .await?;
    remote_socket.write_all(file_bytes).await?;

    let mut file_reader = tokio::fs::File::open(&file).await?;
    tokio::io::copy(&mut file_reader, &mut remote_socket).await?;

    tokio::fs::remove_file(file).await?;

    Ok(())
}
