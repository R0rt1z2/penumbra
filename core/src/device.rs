/*
    SPDX-License-Identifier: AGPL-3.0-or-later
    SPDX-FileCopyrightText: 2025 Shomy
*/
use std::time::Duration;

use log::{error, info};
use tokio::time::timeout;

use crate::connection::Connection;
use crate::connection::port::{ConnectionType, MTKPort};
use crate::core::crypto::config::{CryptoConfig, CryptoIO};
use crate::core::crypto::sej::SEJCrypto;
use crate::core::devinfo::{DevInfoData, DeviceInfo};
use crate::core::seccfg::{LockFlag, SecCfgV4};
use crate::core::storage::{Partition, PartitionKind, parse_gpt};
use crate::da::{DAFile, DAProtocol, DAType, XFlash};
use crate::error::{Error, Result};

/// A builder for creating a new [`Device`].
///
/// This struct allows for configuring various parameters before constructing the device instance.
/// You can optionally (but suggested) provide DA data to enable DA protocol support.
/// When no DA data is provided, only preloader commands will be available, limiting functionality.
/// A MTKPort must be provided to build the device.
///
/// # Example
/// ```rust
/// use penumbra::{Device, DeviceBuilder, find_mtk_port};
///
/// let mtk_port = find_mtk_port().await.ok_or("No MTK port found")?;
/// let da_data = std::fs::read("path/to/da/file").expect("Failed to read DA file");
/// let device =
///     DeviceBuilder::default().with_mtk_port(your_mtk_port).with_da_data(your_da_data).build()?;
/// ```
#[derive(Default)]
pub struct DeviceBuilder {
    /// MTK port to use during connection. It can be either a serial port or a USB port.
    /// This field is required to build a Device.
    mtk_port: Option<Box<dyn MTKPort>>,
    /// DA data to use for the device. This field is optional, but recommended.
    /// If not provided, the device will not be able to use DA protocol, and instead
    /// Only preloader commands will be available.
    da_data: Option<Vec<u8>>,
}

impl DeviceBuilder {
    /// Assigns the MTK port to be used for the device connection.
    pub fn with_mtk_port(mut self, port: Box<dyn MTKPort>) -> Self {
        self.mtk_port = Some(port);
        self
    }

    /// Assigns the DA data to be used for the device.
    pub fn with_da_data(mut self, data: Vec<u8>) -> Self {
        self.da_data = Some(data);
        self
    }

    /// Builds and returns a new `Device` instance.
    pub fn build(self) -> Result<Device> {
        let connection = self.mtk_port.map(Connection::new);

        if connection.is_none() {
            return Err(Error::penumbra("MTK port must be provided to build a Device."));
        }

        Ok(Device {
            dev_info: DeviceInfo::default(),
            connection,
            protocol: None,
            connected: false,
            da_data: self.da_data,
        })
    }
}

/// Represents a connected MTK device.
///
/// This struct is the **main interface** for interacting with the device.
/// It handles initialization, entering DA mode, reading/writing partitions,
/// and accessing connection or protocol information.
///
/// # Lifecycle
/// 1. Construct via [`DeviceBuilder`].
/// 2. Call [`Device::init`] to handshake with the device.
/// 3. Optionally call [`Device::enter_da_mode`] to switch to DA protocol.
/// 4. Perform operations like `read_partition`, `write_partition`, etc.
pub struct Device {
    /// Device information and metadata, shared accross the whole crate.
    pub dev_info: DeviceInfo,
    /// Connection to the device via MTK port, null if DA protocol is used.
    connection: Option<Connection>,
    /// DA protocol handler, null if only preloader commands are used.
    protocol: Option<Box<dyn DAProtocol + Send>>,
    /// Whether the device is connected and initialized.
    connected: bool,
    /// Raw DA file data, if provided.
    da_data: Option<Vec<u8>>,
}

impl Device {
    /// Initializes the device by performing handshake and retrieving device information.
    /// This must be called before any other operations.
    ///
    /// # Examples
    /// ```rust
    /// use penumbra::{DeviceBuilder, find_mtk_port};
    ///
    /// let mtk_port = find_mtk_port().await.ok_or("No MTK port found")?;
    /// let mut device = DeviceBuilder::default().with_mtk_port(mtk_port).build()?;
    ///
    /// device.init().await?;
    /// assert_eq!(device.connected, true);
    /// ```
    pub async fn init(&mut self) -> Result<()> {
        let mut conn = self
            .connection
            .take()
            .ok_or_else(|| Error::penumbra("Connection is not initialized."))?;

        conn.handshake().await?;

        let soc_id = conn.get_soc_id().await?;
        let meid = conn.get_meid().await?;
        let hw_code = conn.get_hw_code().await?;
        let target_config = conn.get_target_config().await?;

        let device_info = DevInfoData {
            soc_id,
            meid,
            hw_code,
            chipset: String::from("Unknown"),
            storage: None,
            partitions: vec![],
            target_config,
        };

        self.dev_info.set_data(device_info).await;

        if self.da_data.is_some() {
            self.protocol = Some(self.init_da_protocol(conn, true).await?);
            self.get_partitions().await;
        } else {
            self.connection = Some(conn);
        }

        self.connected = true;

        Ok(())
    }

    /// Reinits the device connection based on the current connection type and optional DA info.
    /// This is useful for CLIs or scenarios where the Device instance needs to be reset.
    pub async fn reinit(&mut self, dev_info: DevInfoData) -> Result<()> {
        let mut conn = self
            .connection
            .take()
            .ok_or_else(|| Error::penumbra("Connection is not initialized."))?;

        self.dev_info.set_data(dev_info).await;

        match conn.connection_type {
            ConnectionType::Preloader | ConnectionType::Brom => {
                // If we already are in preloader/brom mode, we either handshake again or timeout
                let handshake_result = timeout(Duration::from_secs(3), conn.handshake()).await;
                match handshake_result {
                    Ok(result) => result?,
                    Err(_) => {
                        return Err(Error::conn(
                            "Handshake timed out. Reset the device and try again.",
                        ));
                    }
                }
            }
            ConnectionType::Da => {
                self.protocol = Some(self.init_da_protocol(conn, false).await?);
            }
        };

        self.connected = true;

        Ok(())
    }

    /// Enters DA mode by uploading the DA to the device.
    /// This is required for performing DA protocol operations.
    /// After entering DA mode, the device's partition information is read and stored in `dev_info`.
    ///
    /// # Examples
    /// ```rust
    /// use penumbra::{DeviceBuilder, find_mtk_port};
    ///
    /// let mtk_port = find_mtk_port().await.ok_or("No MTK port found")?;
    /// let da_data = std::fs::read("path/to/da/file").expect("Failed to read DA file");
    /// let mut device =
    ///     DeviceBuilder::default().with_mtk_port(mtk_port).with_da_data(da_data).build()?;
    ///
    /// device.init().await?;
    /// device.enter_da_mode().await?;
    /// assert_eq!(device.get_connection()?.connection_type, ConnectionType::Da);
    /// ```
    pub async fn enter_da_mode(&mut self) -> Result<()> {
        if !self.connected {
            return Err(Error::conn("Device is not connected. Call init() first."));
        }

        if self.protocol.is_none() {
            let conn =
                self.connection.take().ok_or_else(|| Error::conn("No connection available."))?;
            let protocol = self.init_da_protocol(conn, true).await?;
            self.protocol = Some(protocol);
        }

        // Fallback to ensure we always have the partitions available.
        self.get_partitions().await;
        Ok(())
    }

    /// Internal helper to ensure the device enters DA mode before performing DA operations.
    async fn ensure_da_mode(&mut self) -> Result<&mut Box<dyn DAProtocol + Send>> {
        if !self.connected {
            return Err(Error::conn("Device is not connected. Call init() first."));
        }

        if self.protocol.is_none() {
            return Err(Error::conn("DA protocol is not initialized. DA data might be missing."));
        }

        if self.get_connection()?.connection_type != ConnectionType::Da {
            info!("Not in DA mode, entering now...");
            self.enter_da_mode().await?;
        }

        Ok(self.get_protocol().unwrap())
    }

    async fn init_da_protocol(
        &mut self,
        conn: Connection,
        upload_da: bool,
    ) -> Result<Box<dyn DAProtocol + Send>> {
        let da_bytes = self.da_data.clone().ok_or_else(|| {
            Error::conn("DA protocol is not initialized and no DA file was provided.")
        })?;

        let da_file = DAFile::parse_da(&da_bytes)?;
        let hw_code = self.dev_info.hw_code().await;
        let da = da_file.get_da_from_hw_code(hw_code).ok_or_else(|| {
            Error::penumbra(format!("No compatible DA for hardware code 0x{:04X}", hw_code))
        })?;

        let mut protocol: Box<dyn DAProtocol + Send> = match da.da_type {
            DAType::V5 => Box::new(XFlash::new(conn, da, self.dev_info.clone())),
            _ => return Err(Error::penumbra("Unsupported DA type")),
        };

        if upload_da {
            protocol.set_connection_type(ConnectionType::Da)?;
            protocol.upload_da().await?;
        }

        Ok(protocol)
    }

    /// Gets a mutable reference to the active connection.
    /// If the device is in DA mode, it retrieves the connection from the DA protocol.
    pub fn get_connection(&mut self) -> Result<&mut Connection> {
        match (&mut self.connection, &mut self.protocol) {
            (Some(conn), _) => Ok(conn),
            (None, Some(proto)) => Ok(proto.get_connection()),
            (None, None) => Err(Error::conn("No active connection available.")),
        }
    }

    /// Sets the connection type of the active connection.
    /// Note that this does not change the actual connection state, only the type metadata.
    /// This is mainly used for reinitialization after entering DA mode.
    pub fn set_connection_type(&mut self, conn_type: ConnectionType) -> Result<()> {
        let conn = self.get_connection()?;
        conn.connection_type = conn_type;
        Ok(())
    }

    /// Gets a mutable reference to the DA protocol handler, if available.
    /// Returns `None` if the device is not in DA mode.
    pub fn get_protocol(&mut self) -> Option<&mut Box<dyn DAProtocol + Send>> {
        self.protocol.as_mut()
    }

    pub async fn get_partitions(&mut self) -> Vec<Partition> {
        let cached = self.dev_info.partitions().await;
        if !cached.is_empty() {
            return cached;
        }

        let protocol = match self.get_protocol() {
            Some(p) => p,
            None => return Vec::new(),
        };

        let storage_type = protocol.get_storage_type().await;
        let storage = match protocol.get_storage().await {
            Some(s) => s,
            None => return Vec::new(),
        };

        let user_section = storage.get_user_part();

        let mut progress = |_read: usize, _total: usize| {};
        let pgpt_data = match protocol.read_flash(0x0, 0x8000, user_section, &mut progress).await {
            Ok(data) => data,
            Err(_) => return Vec::new(),
        };

        let partitions = match parse_gpt(&pgpt_data, storage_type) {
            Ok(p) => p,
            Err(_) => return Vec::new(),
        };

        self.dev_info.set_partitions(partitions.clone()).await;

        partitions
    }

    /// Reads data from a specified partition on the device.
    /// This function assumes the partition to be part of the user section.
    /// To read from other sections, use `read_offset` with appropriate address.
    pub async fn read_partition(
        &mut self,
        name: &str,
        progress: &mut (dyn FnMut(usize, usize) + Send),
    ) -> Result<Vec<u8>> {
        self.ensure_da_mode().await?;

        let part = self
            .dev_info
            .get_partition(name)
            .await
            .ok_or_else(|| Error::penumbra(format!("Partition '{}' not found", name)))?;

        let storage = self
            .dev_info
            .storage()
            .await
            .ok_or_else(|| Error::proto("Failed to get storage information."))?;

        let section = storage.get_user_part();

        let protocol = self.protocol.as_mut().unwrap();
        protocol.read_flash(part.address, part.size, section, progress).await
    }

    /// Writes data to a specified partition on the device.
    /// This function assumes the partition to be part of the user section.
    /// To write to other sections, use `write_offset` with appropriate address.
    pub async fn write_partition(
        &mut self,
        name: &str,
        data: &[u8],
        progress: &mut (dyn FnMut(usize, usize) + Send),
    ) -> Result<()> {
        self.ensure_da_mode().await?;

        let part = self
            .dev_info
            .get_partition(name)
            .await
            .ok_or_else(|| Error::penumbra(format!("Partition '{}' not found", name)))?;

        // Not needed since write_flash automatically truncates data to partition size.
        // but we keep it for letting user know if they are trying to write too much data.
        if data.len() > part.size {
            return Err(Error::penumbra(format!(
                "Data size {} exceeds partition size {}",
                data.len(),
                part.size
            )));
        }

        let storage = self
            .dev_info
            .storage()
            .await
            .ok_or_else(|| Error::proto("Failed to get storage information."))?;

        let section = storage.get_user_part();

        let protocol = self.protocol.as_mut().unwrap();
        protocol.write_flash(part.address, part.size, data, section, progress).await
    }

    /// Reads data from a specified offset and size on the device.
    /// This allows reading from arbitrary locations, not limited to named partitions.
    /// To specify the section (e.g., user, pl_part1, pl_part2), provide the appropriate
    /// `PartitionKind`.
    ///
    /// # Examples
    /// ```rust
    /// // Let's assume we want to read preloader
    /// use penumbra::{DeviceBuilder, PartitionKind, find_mtk_port};
    ///
    /// let mtk_port = find_mtk_port().await.ok_or("No MTK port found")?;
    /// let mut device = DeviceBuilder::default().with_mtk_port(mtk_port).build()?;
    ///
    /// device.init().await?;
    ///
    /// let mut progress = |_read: usize, _total: usize| {};
    /// let preloader_data = device
    ///     .read_offset(0x0, 0x40000, PartitionKind::Emmc(EmmcPartition::Boot1), &mut progress)
    ///     .await?;
    /// ```
    pub async fn read_offset(
        &mut self,
        address: u64,
        size: usize,
        section: PartitionKind,
        progress: &mut (dyn FnMut(usize, usize) + Send),
    ) -> Result<Vec<u8>> {
        self.ensure_da_mode().await?;

        let protocol = self.protocol.as_mut().unwrap();
        protocol.read_flash(address, size, section, progress).await
    }

    /// Writes data to a specified offset and size on the device.
    /// This allows writing to arbitrary locations, not limited to named partitions.
    /// To specify the section (e.g., user, pl_part1, pl_part2), provide the appropriate
    /// `PartitionKind`.
    ///
    /// # Examples
    /// ```rust
    /// // Let's assume we want to write to preloader
    /// use penumbra::{DeviceBuilder, PartitionKind, find_mtk_port};
    ///
    /// let mtk_port = find_mtk_port().await.ok_or("No MTK port found")?;
    /// let mut device = DeviceBuilder::default().with_mtk_port(mtk_port).build()?;
    ///
    /// device.init().await?;
    ///
    /// let preloader_data = std::fs::read("path/to/preloader_penangf.bin").expect("Failed to read preloader");
    /// let mut progress = |_written: usize, _total: usize| {};
    /// device
    ///     .write_offset(
    ///         0x1000, // Actual preloader offset is 0x0, but we skip the header to ensure correct writing
    ///         preloader_data.len(),
    ///         &preloader_data,
    ///         PartitionKind::Emmc(EmmcPartition::Boot1),
    ///         &mut progress,
    ///     )
    ///     .await?;
    /// ```
    pub async fn write_offset(
        &mut self,
        address: u64,
        size: usize,
        data: &[u8],
        section: PartitionKind,
        progress: &mut (dyn FnMut(usize, usize) + Send),
    ) -> Result<()> {
        self.ensure_da_mode().await?;

        let protocol = self.protocol.as_mut().unwrap();
        protocol.write_flash(address, size, data, section, progress).await
    }

    /// Like `write_partition`, but instead of writing using offsets and sizes from GPT,
    /// it uses the partition name directly.
    ///
    /// This is the same method uses by SP Flash Tool when flashing firmware files.
    /// On locked bootloader, this is the only method that works for flashing stock firmware
    /// without hitting security checks, since the data is first uploaded and then verified as a
    /// whole.
    ///
    /// # Examples
    /// ```rust
    /// use penumbra::{DeviceBuilder, find_mtk_port};
    ///
    /// let mtk_port = find_mtk_port().await.ok_or("No MTK port found")?;
    /// let mut device = DeviceBuilder::default().with_mtk_port(mtk_port).build()?;
    ///
    /// device.init().await?;
    /// let firmware_data = std::fs::read("logo.bin").expect("Failed to read firmware");
    /// device.download("logo", &firmware_data).await?;
    /// ```
    pub async fn download(&mut self, partition: &str, data: &[u8]) -> Result<()> {
        self.ensure_da_mode().await?;

        let protocol = self.protocol.as_mut().unwrap();
        protocol.download(partition.to_string(), data).await
    }

    pub async fn set_seccfg_lock_state(&mut self, lock_state: LockFlag) -> Option<Vec<u8>> {
        // Ensure DA mode first; this will populate partitions and storage
        self.ensure_da_mode().await.ok()?;

        // Use a no-op progress callback
        let mut progress = |_read: usize, _total: usize| {};

        // TODO: Dynamically determine SEJ base (maybe through preloader)
        let sej_base = 0x1000A000;

        // Read the current seccfg partition
        let seccfg_raw = self.read_partition("seccfg", &mut progress).await.ok()?;

        // Compute the new SECCFG
        let new_seccfg = {
            let mut crypto_config = CryptoConfig::new(sej_base, self);
            let mut sej = SEJCrypto::new(&mut crypto_config);
            let mut seccfg = SecCfgV4::parse(&seccfg_raw, &mut sej).await.ok()?;

            seccfg.create(&mut sej, lock_state).await
        };

        // Write the updated seccfg back
        self.write_partition("seccfg", &new_seccfg, &mut progress).await.ok()?;

        Some(new_seccfg)
    }
}

#[async_trait::async_trait]
impl CryptoIO for Device {
    async fn read32(&mut self, addr: u32) -> u32 {
        let Some(protocol) = self.get_protocol() else {
            error!("No protocol available for read32 at 0x{:08X}!", addr);
            return 0;
        };

        match protocol.read32(addr).await {
            Ok(val) => val,
            Err(e) => {
                error!("Failed to read32 from protocol at 0x{:08X}: {}", addr, e);
                0
            }
        }
    }

    async fn write32(&mut self, addr: u32, val: u32) {
        let Some(protocol) = self.get_protocol() else {
            error!("No protocol available for write32 at 0x{:08X}!", addr);
            return;
        };

        if let Err(e) = protocol.write32(addr, val).await {
            error!("Failed to write32 to protocol at 0x{:08X}: {}", addr, e);
        }
    }
}
