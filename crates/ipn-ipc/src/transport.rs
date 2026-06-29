//! Async transport: length-prefixed CBOR [`Frame`]s over an `interprocess` local
//! socket (Unix domain socket on Linux/macOS, named pipe on Windows). Mirrors
//! seed-sync's transport. The Windows pipe DACL and the Unix socket mode are set
//! so an unprivileged GUI can talk to a daemon running as a service / as root.

use std::io;
use std::path::Path;

use interprocess::local_socket::tokio::prelude::*;
#[cfg(unix)]
use interprocess::local_socket::{GenericFilePath, ToFsName};
#[cfg(windows)]
use interprocess::local_socket::{GenericNamespaced, ToNsName};
use interprocess::local_socket::{ListenerOptions, Name};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::{decode, encode, Frame};

pub use interprocess::local_socket::tokio::{Listener, Stream};

fn io_other<E: std::fmt::Display>(e: E) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, e.to_string())
}

pub async fn write_frame<W: AsyncWrite + Unpin>(w: &mut W, frame: &Frame) -> io::Result<()> {
    let bytes = encode(frame).map_err(io_other)?;
    w.write_u32(bytes.len() as u32).await?;
    w.write_all(&bytes).await?;
    w.flush().await?;
    Ok(())
}

/// Read one frame. `Ok(None)` on clean EOF.
pub async fn read_frame<R: AsyncRead + Unpin>(r: &mut R) -> io::Result<Option<Frame>> {
    let len = match r.read_u32().await {
        Ok(len) => len,
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    };
    let mut buf = vec![0u8; len as usize];
    r.read_exact(&mut buf).await?;
    Ok(Some(decode(&buf).map_err(io_other)?))
}

#[cfg(unix)]
fn socket_name(path: &Path) -> io::Result<Name<'_>> {
    path.to_fs_name::<GenericFilePath>()
}

#[cfg(windows)]
fn socket_name(path: &Path) -> io::Result<Name<'static>> {
    // FNV-1a over the path → a stable, legal pipe name agreed by daemon + GUI.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in path.to_string_lossy().bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("nullgate-{hash:016x}.sock").to_ns_name::<GenericNamespaced>()
}

pub async fn connect(path: &Path) -> io::Result<Stream> {
    Stream::connect(socket_name(path)?).await
}

/// SYSTEM + Admins full control; Authenticated Users connect + read/write (but
/// not create a pipe instance). Lets the user's GUI use a LocalSystem daemon's pipe.
#[cfg(windows)]
const PIPE_SDDL: &str = "D:(A;;FA;;;SY)(A;;FA;;;BA)(A;;FRFW;;;AU)";

/// Bind a listener at `path`. Unix: remove stale socket, then loosen the socket
/// mode to 0666 so a user GUI can reach a root daemon. Windows: permissive DACL.
pub fn bind(path: &Path) -> io::Result<Listener> {
    #[cfg(unix)]
    {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if path.exists() {
            let _ = std::fs::remove_file(path);
        }
    }
    let opts = ListenerOptions::new().name(socket_name(path)?);
    #[cfg(windows)]
    let opts = {
        use interprocess::os::windows::local_socket::ListenerOptionsExt;
        use interprocess::os::windows::security_descriptor::SecurityDescriptor;
        let sddl = widestring::U16CString::from_str(PIPE_SDDL).map_err(io_other)?;
        opts.security_descriptor(SecurityDescriptor::deserialize(&sddl)?)
    };
    let listener = opts.create_tokio()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o666));
    }
    Ok(listener)
}

pub async fn accept(listener: &Listener) -> io::Result<Stream> {
    listener.accept().await
}

/// One-shot: connect, send one request, return its correlated response.
pub async fn oneshot_request(path: &Path, req: crate::IpcRequest) -> io::Result<crate::IpcResponse> {
    let stream = connect(path).await?;
    let (mut reader, mut writer) = tokio::io::split(stream);
    write_frame(
        &mut writer,
        &Frame {
            id: 1,
            body: crate::Message::Request(req),
        },
    )
    .await?;
    loop {
        let Some(frame) = read_frame(&mut reader).await? else {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "daemon closed without responding",
            ));
        };
        if frame.id == 1 {
            if let crate::Message::Response(resp) = frame.body {
                return Ok(resp);
            }
        }
    }
}
