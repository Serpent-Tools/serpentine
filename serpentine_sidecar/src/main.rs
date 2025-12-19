//! Side car proxy, facilities serpentines communication with containerd.
//!
//! As well as carries out operations that need to happen on the same host as containerd.
#![expect(
    clippy::expect_used,
    reason = "The proxy runs in a known container image and should have a stable environment."
)]

use std::net;
use std::os::unix::net::UnixStream;

/// The location serpentine connects to the containerd over
const SOCKET_LOCATION: &str = "/run/containerd.sock";

/// The port serpentine listens to
const PORT: u16 = 8000;

fn main() -> ! {
    let _ = simple_logger::init();

    spawn_containerd();

    let listener = net::TcpListener::bind(("0.0.0.0", PORT)).expect("Failed to bind address");
    loop {
        let (socket, _addr) = listener.accept().expect("Failed to get connection");
        log::info!("Got connection, proxying");
        std::thread::spawn(move || proxy_containerd(socket));
    }
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

/// Proxy messages between the given socket and `SOCKET_LOCATION`
fn proxy_containerd(remote_socket: net::TcpStream) {
    let containerd_socket =
        UnixStream::connect(SOCKET_LOCATION).expect("Failed to connect to containerd");

    let (mut remote_write, mut remote_read) = (
        remote_socket.try_clone().expect("Failed to clone socket"),
        remote_socket,
    );
    let (mut containerd_send, mut containerd_read) = (
        containerd_socket
            .try_clone()
            .expect("Failed to clone socket"),
        containerd_socket,
    );

    log::debug!("Connected to containerd, starting proxy");
    let remote_to_containerd =
        std::thread::spawn(move || std::io::copy(&mut remote_read, &mut containerd_send));
    let containerd_to_remote =
        std::thread::spawn(move || std::io::copy(&mut containerd_read, &mut remote_write));

    remote_to_containerd
        .join()
        .expect("Thread paniced")
        .expect("Copy failed");
    containerd_to_remote
        .join()
        .expect("Thread paniced")
        .expect("Copy failed");
}
