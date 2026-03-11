/*
    SPDX-License-Identifier: AGPL-3.0-or-later
    SPDX-FileCopyrightText: 2025 Shomy
*/
pub mod connection;
pub mod core;
pub mod da;
pub mod device;
pub mod error;
#[cfg(not(feature = "no_exploits"))]
pub mod exploit;
pub mod macros;
pub mod utilities;

pub use connection::port::{MTKPort, PortFilter, find_mtk_port};
pub use device::{Device, DeviceBuilder};

const VERSION: &str = env!("CARGO_PKG_VERSION");
