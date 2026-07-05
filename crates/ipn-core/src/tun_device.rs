//! The OS TUN device. On desktop it's opened directly via `tun-rs` (wintun on
//! Windows, /dev/net/tun on Linux, utun on macOS), which needs elevated
//! privileges; the engine degrades to "no routing" if that fails, so membership +
//! presence still work. On **Android** we can't open a TUN ourselves — only the
//! system `VpnService` can — so the app establishes the interface and hands us its
//! file descriptor, which we adopt with [`RealTun::from_fd`]. Both paths produce
//! the same `tun_rs::AsyncDevice`, so the engine's data-plane pump is identical.

use std::io;
#[cfg(not(target_os = "android"))]
use std::net::Ipv4Addr;

use tun_rs::AsyncDevice;
#[cfg(not(target_os = "android"))]
use tun_rs::DeviceBuilder;

/// An opened TUN interface configured with our virtual IP.
pub struct RealTun {
    dev: AsyncDevice,
    mtu: usize,
}

impl RealTun {
    /// Open and configure the interface: assign `ip/prefix` and set the MTU.
    /// (Desktop only — on Android the interface comes from `VpnService`.)
    #[cfg(not(target_os = "android"))]
    pub fn open(ip: Ipv4Addr, prefix: u8, mtu: u16) -> io::Result<Self> {
        let builder = DeviceBuilder::new().ipv4(ip, prefix, None).mtu(mtu);
        // macOS utun interfaces are kernel-control devices whose names must be
        // `utunN` (the OS picks the index); passing an arbitrary name like
        // "nullgate" makes device creation fail, which would silently leave
        // routing "off". Only name the interface on Windows/Linux, where a custom
        // adapter name is allowed; on macOS let the OS assign the utun index.
        #[cfg(not(target_os = "macos"))]
        let builder = builder.name("nullgate");
        let dev = builder.build_async()?;
        Ok(Self {
            dev,
            mtu: mtu as usize,
        })
    }

    /// Adopt the TUN file descriptor produced by Android's `VpnService`
    /// (`ParcelFileDescriptor.detachFd()`). Takes **ownership** of `fd`: the
    /// returned device closes it on drop, and `tun-rs` sets it non-blocking and
    /// registers it with the tokio reactor. On Android `tun-rs` does raw
    /// read/write with no packet-info header, matching VpnService's bare IP frames.
    ///
    /// # Safety
    /// `fd` must be a valid, open, owned file descriptor (e.g. from `detachFd()`);
    /// the caller must not close it or use it after this call.
    #[cfg(target_os = "android")]
    pub unsafe fn from_fd(fd: std::os::fd::RawFd, mtu: u16) -> io::Result<Self> {
        let dev = unsafe { AsyncDevice::from_fd(fd)? };
        Ok(Self {
            dev,
            mtu: mtu as usize,
        })
    }

    /// Read one IP packet from the OS.
    pub async fn recv(&self, buf: &mut [u8]) -> io::Result<usize> {
        self.dev.recv(buf).await
    }

    /// Write one IP packet to the OS.
    pub async fn send(&self, pkt: &[u8]) -> io::Result<usize> {
        self.dev.send(pkt).await
    }

    pub fn mtu(&self) -> usize {
        self.mtu
    }
}
