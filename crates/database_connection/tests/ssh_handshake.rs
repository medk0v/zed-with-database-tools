#![cfg(target_os = "macos")]

use anyhow::{Context, Result, bail, ensure};
use std::ffi::{c_int, c_void};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

static INTERPOSED_CALLS: AtomicUsize = AtomicUsize::new(0);

// WebRTC exports BoringSSL symbols with OpenSSL names but incompatible curve IDs.
// A static libssh2 link can resolve to those symbols instead of its own OpenSSL.
#[allow(non_snake_case)]
#[inline(never)]
#[unsafe(no_mangle)]
pub extern "C" fn EVP_PKEY_CTX_new_id(_identifier: c_int, _engine: *mut c_void) -> *mut c_void {
    INTERPOSED_CALLS.fetch_add(1, Ordering::SeqCst);
    std::ptr::null_mut()
}

struct Sshd(Child);

impl Drop for Sshd {
    fn drop(&mut self) {
        if let Err(error) = self.0.kill() {
            if error.kind() != std::io::ErrorKind::InvalidInput {
                eprintln!("Could not stop test sshd: {error}");
            }
        }
        if let Err(error) = self.0.wait() {
            eprintln!("Could not reap test sshd: {error}");
        }
    }
}

#[test]
#[ignore = "requires macOS system sshd and ssh-keygen plus permission to bind localhost"]
fn curve25519_handshake_isolated_from_executable_crypto_symbols() -> Result<()> {
    let temporary = tempfile::tempdir()?;
    let host_key = temporary.path().join("host_ed25519");
    let generated = Command::new("/usr/bin/ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", "", "-f"])
        .arg(&host_key)
        .status()
        .context("Could not generate the temporary SSH host key")?;
    ensure!(
        generated.success(),
        "Temporary SSH host key generation failed"
    );

    let listener = TcpListener::bind("127.0.0.1:0")?;
    let address = listener.local_addr()?;
    drop(listener);
    let config_path = temporary.path().join("sshd_config");
    std::fs::write(
        &config_path,
        format!(
            "Port {}\nListenAddress 127.0.0.1\nHostKey {}\nPidFile {}\n\
             AuthorizedKeysFile none\nPasswordAuthentication no\n\
             KbdInteractiveAuthentication no\nUsePAM no\n",
            address.port(),
            host_key.display(),
            temporary.path().join("sshd.pid").display(),
        ),
    )?;
    let mut daemon = Sshd(
        Command::new("/usr/sbin/sshd")
            .args(["-D", "-e", "-f"])
            .arg(&config_path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .spawn()
            .context("Could not start the temporary SSH daemon")?,
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    let socket = loop {
        if let Some(status) = daemon.0.try_wait()? {
            bail!("Temporary SSH daemon exited before accepting a connection: {status}");
        }
        match TcpStream::connect_timeout(&address, Duration::from_millis(100)) {
            Ok(socket) => break socket,
            Err(error) if Instant::now() >= deadline => {
                return Err(error)
                    .context("Temporary SSH daemon did not start within five seconds");
            }
            Err(_) => std::thread::sleep(Duration::from_millis(25)),
        }
    };
    socket.set_read_timeout(Some(Duration::from_secs(5)))?;
    socket.set_write_timeout(Some(Duration::from_secs(5)))?;

    // Keep the conflicting symbol in this executable before measuring SSH calls.
    ensure!(EVP_PKEY_CTX_new_id(1034, std::ptr::null_mut()).is_null());
    INTERPOSED_CALLS.store(0, Ordering::SeqCst);
    let mut session = ssh2::Session::new()?;
    session.set_tcp_stream(socket);
    session.set_timeout(5_000);
    session.method_pref(ssh2::MethodType::Kex, "curve25519-sha256")?;
    session
        .handshake()
        .context("Curve25519 handshake must use libssh2's isolated crypto implementation")?;
    ensure!(
        INTERPOSED_CALLS.load(Ordering::SeqCst) == 0,
        "SSH called the executable's incompatible crypto implementation"
    );
    Ok(())
}
