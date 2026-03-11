/*
    SPDX-License-Identifier: AGPL-3.0-or-later
    SPDX-FileCopyrightText: 2025 Shomy
*/

use std::fmt::Debug;
use std::str::FromStr;

use crate::connection::backend::*;
use crate::error::Result;

/// List of all ports available for connecting and what mode they refer to.
/// Add more entries here for vendor specific ports
#[rustfmt::skip]
pub const KNOWN_PORTS: &[(u16, u16, ConnectionType)] = &[
    (0x0E8D, 0x0003, ConnectionType::Brom),      // Mediatek USB Port (BROM)
    (0x0E8D, 0x6000, ConnectionType::Preloader), // Mediatek USB Port (Preloader)
    (0x0E8D, 0x2000, ConnectionType::Preloader), // Mediatek USB Port (Preloader)
    (0x0E8D, 0x2001, ConnectionType::Da),        // Mediatek USB Port (DA)
    (0x0E8D, 0x20FF, ConnectionType::Preloader), // Mediatek USB Port (Preloader)
    (0x0E8D, 0x3000, ConnectionType::Preloader), // Mediatek USB Port (Preloader)
    (0x1004, 0x6000, ConnectionType::Preloader), // LG USB Port (Preloader)
    (0x22D9, 0x0006, ConnectionType::Preloader), // OPPO USB Port (Preloader)
    (0x0FCE, 0xF200, ConnectionType::Brom),      // Sony USB Port (BROM)
    (0x0FCE, 0xD1E9, ConnectionType::Brom),      // Sony USB Port (BROM XA1)
    (0x0FCE, 0xD1E2, ConnectionType::Brom),      // Sony USB Port (BROM)
    (0x0FCE, 0xD1EC, ConnectionType::Brom),      // Sony USB Port (BROM L1)
    (0x0FCE, 0xD1DD, ConnectionType::Brom),      // Sony USB Port (BROM F3111)
];

#[derive(Debug, PartialEq, Copy, Clone)]
pub enum ConnectionType {
    Brom,
    Preloader,
    Da,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PortFilter {
    pub vid: u16,
    pub pid: u16,
}

impl PortFilter {
    pub fn new(vid: u16, pid: u16) -> Self {
        Self { vid, pid }
    }

    pub fn connection_type(&self) -> ConnectionType {
        KNOWN_PORTS
            .iter()
            .find(|(vid, pid, _)| *vid == self.vid && *pid == self.pid)
            .map(|(_, _, ct)| *ct)
            .unwrap_or(ConnectionType::Brom)
    }
}

impl FromStr for PortFilter {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        let (vid_str, pid_str) = s.split_once(':').ok_or_else(|| {
            format!("Invalid port format '{s}', expected VID:PID (e.g. 0FCE:D1EC)")
        })?;
        let vid = u16::from_str_radix(vid_str, 16)
            .map_err(|_| format!("Invalid VID '{vid_str}', expected hex"))?;
        let pid = u16::from_str_radix(pid_str, 16)
            .map_err(|_| format!("Invalid PID '{pid_str}', expected hex"))?;
        Ok(PortFilter { vid, pid })
    }
}

#[async_trait::async_trait]
pub trait MTKPort: Send + Debug {
    async fn open(&mut self) -> Result<()>;
    async fn close(&mut self) -> Result<()>;
    async fn read_exact(&mut self, buf: &mut [u8]) -> Result<usize>;
    async fn write_all(&mut self, buf: &[u8]) -> Result<()>;
    async fn flush(&mut self) -> Result<()>;

    async fn handshake(&mut self) -> Result<()>;
    fn get_connection_type(&self) -> ConnectionType;
    fn get_baudrate(&self) -> u32;
    fn get_port_name(&self) -> String;

    async fn find_device(filter: Option<&PortFilter>) -> Result<Option<Self>>
    where
        Self: Sized;

    // Only for USB ports
    async fn ctrl_out(
        &mut self,
        request_type: u8,
        request: u8,
        value: u16,
        index: u16,
        data: &[u8],
    ) -> Result<()>;
    async fn ctrl_in(
        &mut self,
        request_type: u8,
        request: u8,
        value: u16,
        index: u16,
        len: usize,
    ) -> Result<Vec<u8>>;
}

pub async fn find_mtk_port(filter: Option<&PortFilter>) -> Option<Box<dyn MTKPort>> {
    // Default NUSB backend
    #[cfg(not(any(feature = "libusb", feature = "serial")))]
    let port = UsbMTKPort::find_device(filter).await;

    // LibUSB backend
    #[cfg(feature = "libusb")]
    let port = UsbMTKPort::find_device(filter).await;

    // Serial backend, not ideal since some features (i.e. linecoding) aren't available.
    #[cfg(feature = "serial")]
    let port = SerialMTKPort::find_device(filter).await;

    match port {
        Ok(Some(mut port)) => {
            if port.open().await.is_ok() {
                Some(Box::new(port))
            } else {
                None
            }
        }
        Ok(None) => None,
        Err(_) => None,
    }
}
