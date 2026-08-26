//! Shared fixtures for the frame-host test modules.
//!
//! `frame_host_tests.rs` outgrew the 1000-line file ceiling, so it is split by
//! what each test needs: pure functions over a temp directory, document/policy
//! shape, and tests that drive a live listener. These helpers are the pieces
//! all three need, so they live here rather than being duplicated.

use std::fs;
use std::io::Write;
use std::net::Ipv4Addr;

/// An installed package containing `files`, under a fresh extensions base dir.
pub(crate) fn installed(files: &[(&str, &[u8])]) -> tempfile::TempDir {
    let base = tempfile::tempdir().expect("tempdir");
    let root = base.path().join("demo");
    for (name, body) in files {
        let path = root.join(name);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("mkdir");
        }
        let mut file = fs::File::create(&path).expect("create");
        file.write_all(body).expect("write");
    }
    base
}

/// Does anything accept a TCP connection on this port?
pub(crate) async fn is_listening(port: u16) -> bool {
    tokio::net::TcpStream::connect((Ipv4Addr::LOCALHOST, port))
        .await
        .is_ok()
}

/// Wait for the listener to stop, so a graceful shutdown is not read as a leak.
pub(crate) async fn wait_until_closed(port: u16) -> bool {
    for _ in 0..50 {
        if !is_listening(port).await {
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    false
}
