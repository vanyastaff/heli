//! Unix sockets with mode 0600 in an owner-only directory.

use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;

unsafe extern "C" {
    safe fn geteuid() -> u32;
}

/// The daemon's listening socket.
pub struct Listener {
    inner: UnixListener,
}

impl Listener {
    /// Listens at `path`. The caller holds the daemon lock, so a socket left
    /// there is stale (its daemon died) and is replaced.
    pub fn bind(path: &Path) -> io::Result<Self> {
        if let Some(dir) = path.parent() {
            fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(dir)?;
        }
        match fs::symlink_metadata(path) {
            Ok(metadata) if metadata.file_type().is_socket() => fs::remove_file(path)?,
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!("{} exists and is not a socket", path.display()),
                ));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        let inner = UnixListener::bind(path)?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
        Ok(Self { inner })
    }

    /// Waits for the next client.
    pub fn accept(&mut self) -> io::Result<Stream> {
        let (stream, _) = self.inner.accept()?;
        Ok(Stream(stream))
    }
}

/// One connection. Dropping it closes it; the other side still reads what
/// was written before.
pub struct Stream(UnixStream);

impl Stream {
    /// Whether the other side still has the connection open; asks without
    /// reading or blocking.
    pub fn connected(&self) -> bool {
        use rustix::net::{RecvFlags, recv};
        let mut byte = [0u8];
        match recv(
            &self.0,
            &mut byte[..],
            RecvFlags::PEEK | RecvFlags::DONTWAIT,
        ) {
            // End of file: the other side has closed.
            Ok((0, _)) => false,
            Ok(_) => true,
            Err(error) => error == rustix::io::Errno::AGAIN || error == rustix::io::Errno::INTR,
        }
    }
}

impl Read for Stream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.0.read(buf)
    }
}

impl Write for Stream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

/// Connects to the daemon at `path`, if the socket there belongs to this
/// user.
pub fn connect(path: &Path) -> io::Result<Stream> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.file_type().is_socket() || metadata.uid() != geteuid() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "{} is not a socket of this user (uid {})",
                path.display(),
                geteuid()
            ),
        ));
    }
    UnixStream::connect(path).map(Stream)
}
