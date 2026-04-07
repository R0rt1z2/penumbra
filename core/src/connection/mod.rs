/*
    SPDX-License-Identifier: AGPL-3.0-or-later
    SPDX-FileCopyrightText: 2025 Shomy
*/
mod backend;
mod command;
pub mod port;

use log::{debug, error, info};

use crate::connection::command::Command;
use crate::connection::port::{ConnectionType, MTKPort};
use crate::error::{Error, Result};

#[derive(Debug)]
pub struct Connection {
    pub port: Box<dyn MTKPort>,
    pub connection_type: ConnectionType,
    pub baudrate: u32,
}

impl Connection {
    pub fn new(port: Box<dyn MTKPort>) -> Self {
        let connection_type = port.get_connection_type();
        let baudrate = port.get_baudrate();

        Connection { port, connection_type, baudrate }
    }

    // Writes the provided data to the device
    pub fn write(&mut self, data: &[u8]) -> Result<()> {
        self.port.write_all(data)
    }

    // Reads the exact number of bytes required to fill the provided buffer
    pub fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
        self.port.read_exact(buf)
    }

    // Reads the specified number of bytes
    pub fn read_bytes(&mut self, size: usize) -> Result<Vec<u8>> {
        let mut buf = vec![0u8; size];
        self.port.read_exact(&mut buf)?;
        Ok(buf)
    }

    fn read_u16_be(&mut self) -> Result<u16> {
        let mut buf = [0u8; 2];
        self.port.read_exact(&mut buf)?;
        Ok(u16::from_be_bytes(buf))
    }

    fn read_u16_le(&mut self) -> Result<u16> {
        let mut buf = [0u8; 2];
        self.port.read_exact(&mut buf)?;
        Ok(u16::from_le_bytes(buf))
    }

    fn read_u32_be(&mut self) -> Result<u32> {
        let mut buf = [0u8; 4];
        self.port.read_exact(&mut buf)?;
        Ok(u32::from_be_bytes(buf))
    }

    fn write_u32_be(&mut self, val: u32) -> Result<()> {
        self.write(&val.to_be_bytes())
    }

    pub fn check(&self, data: &[u8], expected_data: &[u8]) -> Result<()> {
        if data == expected_data {
            Ok(())
        } else {
            error!("Data mismatch. Expected: {:x?}, Got: {:x?}", expected_data, data);
            Err(Error::conn("Data mismatch"))
        }
    }

    pub fn echo(&mut self, data: &[u8], size: usize) -> Result<()> {
        self.write(data)?;
        let mut buf = vec![0u8; size];
        self.read(&mut buf)?;
        self.check(&buf, data)
    }

    /* BROM / Preloader download handlers below :D */

    pub fn handshake(&mut self) -> Result<()> {
        info!("Starting handshake...");
        self.port.handshake()?;
        info!("Handshake completed!");
        Ok(())
    }

    pub fn jump_da(&mut self, address: u32) -> Result<()> {
        debug!("Jump to DA at 0x{:08X}", address);

        self.echo(&[Command::JumpDa as u8], 1)?;
        self.echo(&address.to_be_bytes(), 4)?;

        let status = self.read_u16_le()?;
        if status != 0 {
            error!("JumpDA failed with status: {:04X}", status);
            return Err(Error::conn("JumpDA failed"));
        }

        Ok(())
    }

    pub fn send_da(
        &mut self,
        da_data: &[u8],
        da_len: u32,
        address: u32,
        sig_len: u32,
    ) -> Result<()> {
        debug!("Sending DA, size: {}", da_data.len());
        self.echo(&[Command::SendDa as u8], 1)?;
        self.echo(&address.to_be_bytes(), 4)?;
        self.echo(&(da_len).to_be_bytes(), 4)?;
        self.echo(&sig_len.to_be_bytes(), 4)?;

        let status = self.read_u16_be()?;
        debug!("Received status: 0x{:04X}", status);

        if status != 0 {
            error!("SendDA command failed with status: {:04X}", status);
            return Err(Error::conn("SendDA command failed"));
        }

        self.port.write_all(da_data)?;

        debug!("DA sent!");

        let checksum = self.read_u16_be()?;
        debug!("Received checksum: 0x{:04X}", checksum);

        let status = self.read_u16_be()?;
        debug!("Received final status: 0x{:04X}", status);
        if status != 0 {
            error!("SendDA data transfer failed with status: {:04X}", status);
            return Err(Error::conn("SendDA data transfer failed"));
        }

        Ok(())
    }

    pub fn send_auth(&mut self, auth_data: &[u8]) -> Result<()> {
        let mut data = auth_data.to_vec();
        if data.len() % 2 != 0 {
            data.push(0x00);
        }

        self.echo(&[Command::SendAuth as u8], 1)?;

        let len = data.len() as u32;
        self.write_u32_be(len)?;

        let rlen = self.read_u32_be()?;
        if rlen != len {
            return Err(Error::conn("SendAuth length mismatch"));
        }

        let status = self.read_u16_be()?;
        if status == 0x1D0C {
            info!("No auth needed.");
            return Ok(());
        }
        if status > 0xFF {
            return Err(Error::conn(format!("SendAuth failed: 0x{:04X}", status)));
        }

        self.port.write_all(&data)?;

        // Checksum
        self.read_u16_be()?;

        let final_status = self.read_u16_be()?;

        if final_status > 0xFF {
            return Err(Error::conn(format!("SendAuth final status: 0x{:04X}", final_status)));
        }

        info!("Auth sent successfully!");
        Ok(())
    }

    pub fn get_hw_code(&mut self) -> Result<u16> {
        self.echo(&[Command::GetHwCode as u8], 1)?;

        let hw_code = self.read_u16_be()?;
        let status = self.read_u16_le()?;

        if status != 0 {
            error!("GetHwCode failed with status: {:04X}", status);
            return Err(Error::conn("GetHwCode failed"));
        }

        Ok(hw_code)
    }

    pub fn get_hw_sw_ver(&mut self) -> Result<(u16, u16, u16)> {
        self.echo(&[Command::GetHwSwVer as u8], 1)?;

        let hw_sub_code = self.read_u16_le()?;
        let hw_ver = self.read_u16_le()?;
        let sw_ver = self.read_u16_le()?;
        let status = self.read_u16_le()?;

        if status != 0 {
            error!("GetHwSwVer failed with status: 0x{:04X}", status);
            return Err(Error::conn("GetHwSwVer failed"));
        }

        Ok((hw_sub_code, hw_ver, sw_ver))
    }

    pub fn get_soc_id(&mut self) -> Result<[u8; 32]> {
        let mut soc_id = [0u8; 32];

        self.echo(&[Command::GetSocId as u8], 1)?;

        let length = self.read_u32_be()? as usize;

        if length > soc_id.len() {
            return Err(Error::conn("Invalid SoC ID length"));
        }

        self.port.read_exact(&mut soc_id)?;

        let status = self.read_u16_le()?;

        if status != 0 {
            error!("GetSocId failed with status: 0x{:04X}", status);
            return Err(Error::conn("GetSocId failed"));
        }

        Ok(soc_id)
    }

    pub fn get_meid(&mut self) -> Result<[u8; 16]> {
        self.port.write_all(&[Command::GetMeId as u8])?;
        let mut echo = [0u8; 1];
        self.port.read_exact(&mut echo)?;

        let mut meid = [0u8; 16];

        // IQO Preloader seems to have a custom security gate that blocks most commands
        // behind an OEM authentication challenge (0x90/0x91). Only a small whitelist of
        // commands (GET_HW_CODE, GET_HW_SW_VER, GET_SOC_ID, and the OEM commands) are
        // allowed before authentication. Blocked commands receive 0xDC instead of an echo.
        if echo[0] == 0xDC {
            return Err(Error::conn(
                "Command blocked by Preloader security. \
                This device requires OEM authentication before commands can be executed.",
            ));
        }

        if echo[0] != Command::GetMeId as u8 {
            return Err(Error::conn("Data mismatch"));
        }

        let length = self.read_u32_be()? as usize;

        if length > meid.len() {
            return Err(Error::conn("Invalid MEID length"));
        }

        self.port.read_exact(&mut meid)?;

        let status = self.read_u16_le()?;

        if status != 0 {
            error!("GetMeid failed with status: 0x{:04X}", status);
            return Err(Error::conn("GetMeid failed"));
        }

        Ok(meid)
    }

    /// Returns the target configuration of the device.
    /// This configuration can be interpreted as follows:
    ///
    /// SBC = target_config & 0x1
    /// SLA = target_config & 0x2
    /// DAA = target_config & 0x4
    pub fn get_target_config(&mut self) -> Result<u32> {
        self.echo(&[Command::GetTargetConfig as u8], 1)?;

        let config = self.read_u32_be()?;
        let status = self.read_u16_le()?;

        if status != 0 {
            error!("GetTargetConfig failed with status: 0x{:04X}", status);
            return Err(Error::conn("GetTargetConfig failed"));
        }

        Ok(config)
    }

    pub fn get_pl_capabilities(&mut self) -> Result<u32> {
        self.echo(&[Command::GetPlCap as u8], 1)?;

        let cap0 = self.read_u32_be()?;
        let _cap1 = self.read_u32_be()?; // Reserved

        Ok(cap0)
    }

    /// Reads memory from the device with size, split into 4-byte chunks.
    pub fn read32(&mut self, address: u32, size: usize) -> Result<Vec<u8>> {
        let aligned = size.div_ceil(4) * 4;

        self.echo(&[Command::Read32 as u8], 1)?;
        self.echo(&address.to_be_bytes(), 4)?;
        self.echo(&((aligned / 4) as u32).to_be_bytes(), 4)?;

        let status = self.read_u16_be()?;
        if status != 0 {
            return Err(Error::conn(format!("Read32 failed with status: 0x{:04X}", status)));
        }

        let mut data = vec![0u8; aligned];
        for chunk in data.chunks_mut(4) {
            self.port.read_exact(chunk)?;
        }

        let status = self.read_u16_be()?;
        if status != 0 {
            return Err(Error::conn(format!("Read32 failed with status: 0x{:04X}", status)));
        }

        data.truncate(size);
        Ok(data)
    }
}
