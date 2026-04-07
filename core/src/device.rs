/*
    SPDX-License-Identifier: AGPL-3.0-or-later
    SPDX-FileCopyrightText: 2025 Shomy
*/

use std::io::{Read, Write};

use log::{error, info, warn};

use crate::connection::Connection;
use crate::connection::port::{ConnectionType, MTKPort};
use crate::core::chip::{ChipInfo, chip_from_hw_code};
use crate::core::crypto::config::CryptoIO;
use crate::core::devinfo::{DevInfoData, DeviceInfo};
use crate::core::log_buffer::DeviceLog;
use crate::core::seccfg::LockFlag;
use crate::core::storage::{Partition, PartitionKind, RpmbRegion};
use crate::da::protocol::{BootMode, DAProtocolParams};
use crate::da::{DAFile, DAProtocol, DAType, DownloadProtocol, XFlash, Xml};
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
    /// Preloader data to use for the device. This field is optional.
    /// If provided, it can be used to extract EMI settings or other information.
    /// Only needed if told to do so, like when the device is in BROM mode.
    preloader_data: Option<Vec<u8>>,
    /// Authentication data for DAA-enabled devices. This field is optional.
    /// If the device has DAA enabled and is in BROM mode, this data will be
    /// sent during initialization to authenticate with the device.
    auth_data: Option<Vec<u8>>,
    /// Whether to enable verbose logging.
    verbose: bool,
    /// Whether to use USB as the DA log channel instead of UART.
    /// When enabled, DA log messages are captured into a [`DeviceLog`] buffer
    /// instead of being sent over UART.
    usb_log_channel: bool,
    /// A buffer to store DA log messages when `usb_log_channel` is enabled.
    /// This allows for capturing logs from devices without needing UART.
    device_log: Option<DeviceLog>,
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

    /// Assigns the preloader data to be used for the device.
    pub fn with_preloader(mut self, data: Vec<u8>) -> Self {
        self.preloader_data = Some(data);
        self
    }

    /// Assigns the authentication data for DAA-enabled devices.
    pub fn with_auth(mut self, data: Vec<u8>) -> Self {
        self.auth_data = Some(data);
        self
    }

    /// Enables verbose logging mode.
    pub fn with_verbose(mut self, verbose: bool) -> Self {
        self.verbose = verbose;
        self
    }

    /// Enable USB logging
    pub fn with_usb_log_channel(mut self, enabled: bool) -> Self {
        self.usb_log_channel = enabled;
        self
    }

    /// Assigns a [`DeviceLog`] buffer to capture DA log messages
    /// when `usb_log_channel` is enabled.
    /// This allows to attach an optional Callback to the log buffer
    /// (i.e. to save to a file).
    pub fn with_device_log(mut self, log: DeviceLog) -> Self {
        self.device_log = Some(log);
        self
    }

    /// Builds and returns a new `Device` instance.
    pub fn build(self) -> Result<Device> {
        let connection = self.mtk_port.map(Connection::new);

        if connection.is_none() {
            return Err(Error::penumbra("MTK port must be provided to build a Device."));
        }

        let device_log = self.device_log.unwrap_or_default();

        Ok(Device {
            dev_info: DeviceInfo::default(),
            connection,
            protocol: None,
            connected: false,
            da_data: self.da_data,
            preloader_data: self.preloader_data,
            auth_data: self.auth_data,
            verbose: self.verbose,
            usb_log_channel: self.usb_log_channel,
            device_log,
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
    protocol: Option<DAProtocol>,
    /// Whether the device is connected and initialized.
    connected: bool,
    /// Raw DA file data, if provided.
    da_data: Option<Vec<u8>>,
    /// Preloader data, if provided.
    preloader_data: Option<Vec<u8>>,
    /// Authentication data for DAA-enabled devices, if provided.
    auth_data: Option<Vec<u8>>,
    /// Whether verbose logging is enabled.
    verbose: bool,
    /// Whether to log DA messages over USB.
    usb_log_channel: bool,
    /// Buffer to store DA log messages.
    device_log: DeviceLog,
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
    pub fn init(&mut self) -> Result<()> {
        let mut conn = self
            .connection
            .take()
            .ok_or_else(|| Error::penumbra("Connection is not initialized."))?;

        conn.handshake()?;

        let soc_id = conn.get_soc_id()?;
        let meid = conn.get_meid()?;
        let hw_code = conn.get_hw_code()?;
        let target_config = conn.get_target_config()?;

        let device_info = DevInfoData { soc_id, meid, hw_code, partitions: vec![], target_config };

        self.dev_info.set_data(device_info);
        let chip = chip_from_hw_code(hw_code);
        if chip.hw_code() == 0x0000 {
            warn!("Unknown hardware code 0x{:04X}. Device might not work as expected.", hw_code);
            warn!("If you think this is incorrect, please report this hw code to the developers.");
        }

        self.dev_info.set_chip(chip);

        let daa = (target_config & 0x4) != 0;
        if daa && conn.connection_type == ConnectionType::Brom {
            if let Some(auth) = &self.auth_data {
                conn.send_auth(auth)?;
            } else {
                warn!("DAA enabled but no auth data provided.");
            }
        }

        if self.da_data.is_some() {
            self.protocol = Some(self.init_da_protocol(conn)?);
        } else {
            self.connection = Some(conn);
        }

        self.connected = true;

        Ok(())
    }

    /// Reinits the device connection based on the current connection type and optional DA info.
    /// This is useful for CLIs or scenarios where the Device instance needs to be reset.
    pub fn reinit(&mut self, dev_info: DevInfoData) -> Result<()> {
        let mut conn = self
            .connection
            .take()
            .ok_or_else(|| Error::penumbra("Connection is not initialized."))?;

        self.dev_info.set_data(dev_info);
        self.dev_info.set_chip(chip_from_hw_code(self.dev_info.hw_code()));

        match conn.connection_type {
            ConnectionType::Preloader | ConnectionType::Brom => {
                // If we already are in preloader/brom mode, just handshake again
                conn.handshake()?;
            }
            ConnectionType::Da => {
                self.protocol = Some(self.init_da_protocol(conn)?);
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
    pub fn enter_da_mode(&mut self) -> Result<()> {
        if !self.connected {
            return Err(Error::conn("Device is not connected. Call init() first."));
        }

        let conn_type = self.get_connection()?.connection_type;

        if self.protocol.is_none() {
            let conn =
                self.connection.take().ok_or_else(|| Error::conn("No connection available."))?;
            let protocol = self.init_da_protocol(conn)?;
            self.protocol = Some(protocol);
        }

        let protocol = self.protocol.as_mut().unwrap();
        if conn_type != ConnectionType::Da {
            protocol.upload_da()?;
            self.set_connection_type(ConnectionType::Da)?;
        }

        // Fallback to ensure we always have the partitions available.
        self.get_partitions();
        Ok(())
    }

    /// Internal helper to ensure the device enters DA mode before performing DA operations.
    fn ensure_da_mode(&mut self) -> Result<&mut DAProtocol> {
        if !self.connected {
            return Err(Error::conn("Device is not connected. Call init() first."));
        }

        if self.protocol.is_none() {
            return Err(Error::conn("DA protocol is not initialized. DA data might be missing."));
        }

        if self.get_connection()?.connection_type != ConnectionType::Da {
            info!("Not in DA mode, entering now...");
            self.enter_da_mode()?;
        }

        Ok(self.get_protocol().unwrap())
    }

    fn init_da_protocol(&mut self, conn: Connection) -> Result<DAProtocol> {
        let da_bytes = self.da_data.clone().ok_or_else(|| {
            Error::conn("DA protocol is not initialized and no DA file was provided.")
        })?;

        let da_file = DAFile::parse_da(&da_bytes)?;
        let hw_code = self.dev_info.hw_code();
        let da = da_file.get_da_from_hw_code(hw_code).ok_or_else(|| {
            Error::penumbra(format!("No compatible DA for hardware code 0x{:04X}", hw_code))
        })?;

        let da_type = da.da_type;

        let params = DAProtocolParams {
            da,
            devinfo: self.dev_info.clone(),
            device_log: self.device_log.clone(),
            verbose: self.verbose,
            usb_log_channel: self.usb_log_channel,
            preloader: self.preloader_data.clone(),
        };

        let protocol: DAProtocol = match da_type {
            DAType::V5 => DAProtocol::V5(XFlash::new(conn, params)),
            DAType::V6 => DAProtocol::V6(Xml::new(conn, params)),
            _ => return Err(Error::penumbra("Unsupported DA type")),
        };

        self.get_partitions();
        Ok(protocol)
    }

    /// Returns the resolved [`ChipInfo`] for this device.
    pub fn chip(&self) -> &'static ChipInfo {
        self.dev_info.chip()
    }

    /// Returns a reference to the device log buffer
    pub fn device_log(&self) -> &DeviceLog {
        &self.device_log
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
    pub fn get_protocol(&mut self) -> Option<&mut DAProtocol> {
        self.protocol.as_mut()
    }

    /// Retrieves the list of partitions from the device.
    /// If partitions have already been fetched, returns the cached list.
    /// Otherwise, queries the DA protocol for partition information and caches the result.
    ///
    /// Returns an empty list if no DA protocol is available.
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
    /// let partitions = device.get_partitions().await;
    /// for part in &partitions {
    ///     println!("{}: size={}", part.name, part.size);
    /// }
    /// ```
    pub fn get_partitions(&mut self) -> Vec<Partition> {
        let cached = self.dev_info.partitions();
        if !cached.is_empty() {
            return cached;
        }

        let protocol = match self.get_protocol() {
            Some(p) => p,
            None => return Vec::new(),
        };

        info!("Retrieving partition information...");
        let partitions = protocol.get_partitions();

        self.dev_info.set_partitions(partitions.clone());

        partitions
    }

    /// Reads data from a specified partition on the device.
    /// This function assumes the partition to be part of the user section.
    /// To read from other sections, use `read_offset` with appropriate address.
    pub fn read_partition<W, F>(&mut self, name: &str, mut progress: F, mut writer: W) -> Result<()>
    where
        W: Write + Send,
        F: FnMut(usize, usize) + Send,
    {
        self.ensure_da_mode()?;

        let part = self
            .dev_info
            .get_partition(name)
            .ok_or_else(|| Error::penumbra(format!("Partition '{}' not found", name)))?;

        let protocol = self.protocol.as_mut().unwrap();
        protocol.read_flash(part.address, part.size, part.kind, &mut progress, &mut writer)
    }

    /// Writes data to a specified partition on the device.
    /// This function assumes the partition to be part of the user section.
    /// To write to other sections, use `write_offset` with appropriate address.
    pub fn write_partition<R, F>(
        &mut self,
        name: &str,
        mut reader: R,
        mut progress: F,
    ) -> Result<()>
    where
        R: Read + Send,
        F: FnMut(usize, usize) + Send,
    {
        self.ensure_da_mode()?;

        let part = self
            .dev_info
            .get_partition(name)
            .ok_or_else(|| Error::penumbra(format!("Partition '{}' not found", name)))?;

        let protocol = self.protocol.as_mut().unwrap();
        protocol.write_flash(part.address, part.size, &mut reader, part.kind, &mut progress)
    }

    /// Erases a specified partition on the device.
    /// This function assumes the partition to be part of the user section.
    /// To erase other sections, use `erase_offset` with the appropriate address.
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
    /// let mut progress = |_erased: usize, _total: usize| {};
    /// device.erase_partition("userdata", &mut progress).await?;
    /// ```
    pub fn erase_partition(
        &mut self,
        partition: &str,
        progress: &mut (dyn FnMut(usize, usize) + Send),
    ) -> Result<()> {
        self.ensure_da_mode()?;

        let part = self
            .dev_info
            .get_partition(partition)
            .ok_or_else(|| Error::penumbra(format!("Partition '{}' not found", partition)))?;

        let protocol = self.protocol.as_mut().unwrap();
        protocol.erase_flash(part.address, part.size, part.kind, progress)
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
    pub fn read_offset<W, F>(
        &mut self,
        address: u64,
        size: usize,
        section: PartitionKind,
        mut progress: F,
        mut writer: W,
    ) -> Result<()>
    where
        W: Write + Send,
        F: FnMut(usize, usize) + Send,
    {
        self.ensure_da_mode()?;

        let protocol = self.protocol.as_mut().unwrap();
        protocol.read_flash(address, size, section, &mut progress, &mut writer)
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
    pub fn write_offset<R, F>(
        &mut self,
        address: u64,
        size: usize,
        mut reader: R,
        section: PartitionKind,
        mut progress: F,
    ) -> Result<()>
    where
        R: Read + Send,
        F: FnMut(usize, usize) + Send,
    {
        self.ensure_da_mode()?;

        let protocol = self.protocol.as_mut().unwrap();
        protocol.write_flash(address, size, &mut reader, section, &mut progress)
    }

    /// Erases data at a specified offset and size on the device.
    /// This allows erasing arbitrary locations, not limited to named partitions.
    /// To specify the section (e.g., user, pl_part1, pl_part2), provide the appropriate
    /// `PartitionKind`.
    ///
    /// # Examples
    /// ```rust
    /// use penumbra::{DeviceBuilder, PartitionKind, find_mtk_port};
    ///
    /// let mtk_port = find_mtk_port().await.ok_or("No MTK port found")?;
    /// let da_data = std::fs::read("path/to/da/file").expect("Failed to read DA file");
    /// let mut device =
    ///     DeviceBuilder::default().with_mtk_port(mtk_port).with_da_data(da_data).build()?;
    ///
    /// device.init().await?;
    /// let mut progress = |_erased: usize, _total: usize| {};
    /// device
    ///     .erase_offset(0x0, 0x40000, PartitionKind::Emmc(EmmcPartition::Boot1), &mut progress)
    ///     .await?;
    /// ```
    pub fn erase_offset(
        &mut self,
        address: u64,
        size: usize,
        section: PartitionKind,
        progress: &mut (dyn FnMut(usize, usize) + Send),
    ) -> Result<()> {
        self.ensure_da_mode()?;

        let protocol = self.protocol.as_mut().unwrap();
        protocol.erase_flash(address, size, section, progress)
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
    pub fn download<R, F>(
        &mut self,
        partition: &str,
        size: usize,
        mut reader: R,
        mut progress: F,
    ) -> Result<()>
    where
        R: Read + Send,
        F: FnMut(usize, usize) + Send,
    {
        self.ensure_da_mode()?;

        let protocol = self.protocol.as_mut().unwrap();
        protocol.download(partition.to_string(), size, &mut reader, &mut progress)
    }

    /// Like `read_partition`, but instead of reading using offsets and sizes from GPT,
    /// it uses the partition name directly.
    ///
    /// This is the same method uses by SP Flash Tool when reading back without scatter.
    ///
    /// # Examples
    /// ```rust
    /// use penumbra::{DeviceBuilder, find_mtk_port};
    /// use tokio::fs::File;
    /// use tokio::io::BufWriter;
    ///
    /// let mtk_port = find_mtk_port().await.ok_or("No MTK port found")?;
    /// let da_data = std::fs::read("path/to/da/file").expect("Failed to read DA file");
    /// let mut device =
    ///     DeviceBuilder::default().with_mtk_port(mtk_port).with_da_data(da_data).build()?;
    ///
    /// device.init().await?;
    /// // Readsback "logo" partition to "logo.bin"
    /// let file = File::create("logo.bin").await?;
    /// let mut writer = BufWriter::new(file);
    /// let mut progress = |_written: usize, _total: usize| {};
    /// device.upload("logo", &mut writer, &mut progress).await?;
    /// ```
    pub fn upload<W, F>(&mut self, partition: &str, mut writer: W, mut progress: F) -> Result<()>
    where
        W: Write + Send,
        F: FnMut(usize, usize) + Send,
    {
        self.ensure_da_mode()?;

        let protocol = self.protocol.as_mut().unwrap();
        protocol.upload(partition.to_string(), &mut writer, &mut progress)
    }

    /// Formats a specified partition on the device
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
    /// let mut progress = |_erased: usize, _total: usize| {};
    /// device.format("userdata", &mut progress).await?;
    /// ```
    pub fn format(
        &mut self,
        partition: &str,
        progress: &mut (dyn FnMut(usize, usize) + Send),
    ) -> Result<()> {
        self.ensure_da_mode()?;

        let protocol = self.protocol.as_mut().unwrap();
        protocol.format(partition.to_string(), progress)
    }

    /// Shuts down the device
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
    /// device.shutdown().await?;
    /// ```
    pub fn shutdown(&mut self) -> Result<()> {
        self.ensure_da_mode()?;

        let protocol = self.protocol.as_mut().unwrap();
        protocol.shutdown()
    }

    /// Reboots the device into the specified boot mode.
    /// Supported boot modes include `Normal`, `HomeScreen`, `Fastboot`, `Test`, and `Meta`.
    ///
    /// # Examples
    /// ```rust
    /// use penumbra::{BootMode, DeviceBuilder, find_mtk_port};
    ///
    /// let mtk_port = find_mtk_port().await.ok_or("No MTK port found")?;
    /// let da_data = std::fs::read("path/to/da/file").expect("Failed to read DA file");
    /// let mut device =
    ///     DeviceBuilder::default().with_mtk_port(mtk_port).with_da_data(da_data).build()?;
    ///
    /// device.init().await?;
    /// device.reboot(BootMode::Normal).await?;
    /// ```
    pub fn reboot(&mut self, bootmode: BootMode) -> Result<()> {
        self.ensure_da_mode()?;

        let protocol = self.protocol.as_mut().unwrap();
        protocol.reboot(bootmode)
    }

    /// Sets the lock state in `seccfg` to either lock or unlock the bootloader.
    /// Returns the raw `seccfg` data on success, or `None` if the operation fails.
    ///
    /// Only available when the `no_exploits` feature is **not** enabled.
    /// Requires DA Extensions.
    ///
    /// # Examples
    /// ```rust
    /// use penumbra::{DeviceBuilder, LockFlag, find_mtk_port};
    ///
    /// let mtk_port = find_mtk_port().await.ok_or("No MTK port found")?;
    /// let da_data = std::fs::read("path/to/da/file").expect("Failed to read DA file");
    /// let mut device =
    ///     DeviceBuilder::default().with_mtk_port(mtk_port).with_da_data(da_data).build()?;
    ///
    /// device.init().await?;
    /// let seccfg = device.set_seccfg_lock_state(LockFlag::Unlock).await;
    /// ```
    #[cfg(not(feature = "no_exploits"))]
    pub fn set_seccfg_lock_state(&mut self, lock_state: LockFlag) -> Option<[u8; 512]> {
        // Ensure DA mode first; this will populate partitions and storage
        self.ensure_da_mode().ok()?;
        let protocol = self.protocol.as_mut().unwrap();
        protocol.set_seccfg_lock_state(lock_state)
    }

    /// Reads memory from the device at the given address and size.
    /// The data is written to the provided `writer` as it is read..
    ///
    /// Only available when the `no_exploits` feature is **not** enabled.
    ///
    /// # Examples
    /// ```rust
    /// use penumbra::{DeviceBuilder, find_mtk_port};
    /// use tokio::fs::File;
    /// use tokio::io::BufWriter;
    ///
    /// let mtk_port = find_mtk_port().await.ok_or("No MTK port found")?;
    /// let da_data = std::fs::read("path/to/da/file").expect("Failed to read DA file");
    /// let mut device =
    ///     DeviceBuilder::default().with_mtk_port(mtk_port).with_da_data(da_data).build()?;
    ///
    /// device.init().await?;
    /// let file = File::create("dump.bin").await?;
    /// let mut writer = BufWriter::new(file);
    /// let mut progress = |_read: usize, _total: usize| {};
    /// device.peek(0x0010_0000, 0x1000, &mut writer, &mut progress).await?;
    /// ```
    #[cfg(not(feature = "no_exploits"))]
    pub fn peek<W, F>(
        &mut self,
        addr: u32,
        size: usize,
        mut writer: W,
        mut progress: F,
    ) -> Result<()>
    where
        W: Write + Send,
        F: FnMut(usize, usize) + Send,
    {
        self.ensure_da_mode()?;

        let protocol = self.protocol.as_mut().unwrap();
        protocol.peek(addr, size, &mut writer, &mut progress)
    }

    /// Writes memory to the device at the given address and size.
    /// The data is read from the provided `reader` as it is written..
    ///
    /// Only available when the `no_exploits` feature is **not** enabled.
    ///
    /// # Examples
    /// ```rust
    /// use penumbra::{DeviceBuilder, find_mtk_port};
    /// use tokio::fs::File;
    /// use tokio::io::BufWriter;
    ///
    /// let mtk_port = find_mtk_port().await.ok_or("No MTK port found")?;
    /// let da_data = std::fs::read("path/to/da/file").expect("Failed to read DA file");
    /// let mut device =
    ///     DeviceBuilder::default().with_mtk_port(mtk_port).with_da_data(da_data).build()?;
    ///
    /// device.init().await?;
    /// let file = File::create("dump.bin").await?;
    /// let mut reader = BufReader::new(file);
    /// let mut progress = |_read: usize, _total: usize| {};
    /// device.poke(0x0010_0000, 0x1000, &mut reader, &mut progress).await?;
    /// ```
    #[cfg(not(feature = "no_exploits"))]
    pub fn poke<R, F>(
        &mut self,
        addr: u32,
        size: usize,
        mut reader: R,
        mut progress: F,
    ) -> Result<()>
    where
        R: Read + Send,
        F: FnMut(usize, usize) + Send,
    {
        self.ensure_da_mode()?;

        let protocol = self.protocol.as_mut().unwrap();
        protocol.poke(addr, size, &mut reader, &mut progress)
    }

    #[cfg(not(feature = "no_exploits"))]
    pub fn read_rpmb<W, F>(
        &mut self,
        region: RpmbRegion,
        start_sector: u32,
        sectors_count: u32,
        mut writer: W,
        mut progress: F,
    ) -> Result<()>
    where
        W: Write + Send,
        F: FnMut(usize, usize) + Send,
    {
        self.ensure_da_mode()?;

        let protocol = self.protocol.as_mut().unwrap();
        protocol.read_rpmb(region, start_sector, sectors_count, &mut writer, &mut progress)
    }

    #[cfg(not(feature = "no_exploits"))]
    pub fn write_rpmb<R, F>(
        &mut self,
        region: RpmbRegion,
        start_sector: u32,
        sectors_count: u32,
        mut reader: R,
        mut progress: F,
    ) -> Result<()>
    where
        R: Read + Send,
        F: FnMut(usize, usize) + Send,
    {
        self.ensure_da_mode()?;

        let protocol = self.protocol.as_mut().unwrap();
        protocol.write_rpmb(region, start_sector, sectors_count, &mut reader, &mut progress)
    }

    #[cfg(not(feature = "no_exploits"))]
    pub fn auth_rpmb(&mut self, region: RpmbRegion, key: &[u8]) -> Result<()> {
        self.ensure_da_mode()?;

        let protocol = self.protocol.as_mut().unwrap();
        protocol.auth_rpmb(region, key)
    }
}

impl CryptoIO for Device {
    fn read32(&mut self, addr: u32) -> u32 {
        let Some(protocol) = self.get_protocol() else {
            error!("No protocol available for read32 at 0x{:08X}!", addr);
            return 0;
        };

        match protocol.read32(addr) {
            Ok(val) => val,
            Err(e) => {
                error!("Failed to read32 from protocol at 0x{:08X}: {}", addr, e);
                0
            }
        }
    }

    fn write32(&mut self, addr: u32, val: u32) {
        let Some(protocol) = self.get_protocol() else {
            error!("No protocol available for write32 at 0x{:08X}!", addr);
            return;
        };

        if let Err(e) = protocol.write32(addr, val) {
            error!("Failed to write32 to protocol at 0x{:08X}: {}", addr, e);
        }
    }
}
