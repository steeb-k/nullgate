//! The real OS TUN device (desktop), via `tun-rs` (wintun on Windows, /dev/net/tun
//! on Linux, utun on macOS). Creating it requires elevated privileges; the engine
//! degrades to "no routing" if this fails, so membership + presence still work.

use std::io;
use std::net::Ipv4Addr;

use tun_rs::{AsyncDevice, DeviceBuilder};

/// An opened TUN interface configured with our virtual IP.
pub struct RealTun {
    dev: AsyncDevice,
    mtu: usize,
}

impl RealTun {
    /// Open and configure the interface: assign `ip/prefix` and set the MTU.
    pub fn open(ip: Ipv4Addr, prefix: u8, mtu: u16) -> io::Result<Self> {
        let dev = DeviceBuilder::new()
            .name("nullgate")
            .ipv4(ip, prefix, None)
            .mtu(mtu)
            .build_async()?;
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
