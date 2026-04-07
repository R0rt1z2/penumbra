/*
    SPDX-License-Identifier: AGPL-3.0-or-later
    SPDX-FileCopyrightText: 2025 Shomy
*/
mod commands;
mod common;
mod helpers;
mod macros;
mod state;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::Result;
use clap::{CommandFactory, Parser};
use log::info;
use penumbra::connection::port::ConnectionType;
use penumbra::core::devinfo::DevInfoData;
use penumbra::{Device, DeviceBuilder, find_mtk_port};
use tokio::fs::read;

use crate::cli::commands::*;
use crate::cli::helpers::setup_file_logger;
use crate::cli::macros::mtk_commands;
use crate::cli::state::PersistedDeviceState;

const DA_LOG_FILE: &str = "da.log";

#[derive(Parser, Debug)]
#[command(author, version, about)]
pub struct CliArgs {
    /// Run in CLI mode without TUI
    #[arg(short, long, global = true)]
    pub cli: bool,
    /// Enable verbose logging, including debug information
    #[arg(short, long, global = true)]
    pub verbose: bool,
    /// The DA file to use
    #[arg(short, long = "da", value_name = "DA_FILE", global = true)]
    pub da_file: Option<PathBuf>,
    /// The preloader file to use
    #[arg(short, long = "pl", value_name = "PRELOADER_FILE", global = true)]
    pub preloader_file: Option<PathBuf>,
    /// The auth file for DAA-enabled devices
    #[arg(short, long = "auth", value_name = "AUTH_FILE", global = true)]
    pub auth_file: Option<PathBuf>,
    /// Enable USB DA logging
    #[arg(long = "usb-log", global = true)]
    pub usb_log: bool,
    /// Subcommands for CLI mode. If provided, TUI mode will be disabled.
    #[command(subcommand)]
    pub command: Option<Commands>,
}

mtk_commands! {
    Download(DownloadArgs),
    Upload(UploadArgs),
    Format(FormatArgs),
    WriteFlash(WriteArgs),
    ReadFlash(ReadArgs),
    Erase(EraseArgs),
    ReadAll(ReadAllArgs),
    Seccfg(SeccfgArgs),
    Pgpt(PgptArgs),
    Peek(PeekArgs),
    Poke(PokeArgs),
    Rpmb(RpmbArgs),
    Shutdown(ShutdownArgs),
    Reboot(RebootArgs),
    XFlash(XFlashArgs),
}

pub trait MtkCommand {
    fn run(&self, dev: &mut Device, state: &mut PersistedDeviceState) -> Result<()>;
}

pub async fn run_cli(args: &CliArgs) -> Result<()> {
    if args.command.is_none() {
        CliArgs::command().print_help()?;
        return Ok(());
    }

    let mut state = PersistedDeviceState::load().await;

    let usb_log_channel = state.usb_log || args.usb_log;

    let da_data = if let Some(da_path) = &args.da_file {
        let data = read(da_path).await?;
        state.da_file_path = Some(da_path.to_string_lossy().to_string());
        Some(data)
    } else {
        None
    };

    let pl_data = if let Some(pl_path) = &args.preloader_file {
        let data = read(pl_path).await?;
        Some(data)
    } else {
        None
    };

    let auth_data = if let Some(auth_path) = &args.auth_file {
        let data = read(auth_path).await?;
        Some(data)
    } else {
        None
    };

    let mut last_seen = Instant::now();
    let timeout = Duration::from_millis(500);

    info!("Waiting for MTK device...");
    let mtk_port = loop {
        if let Some(port) = find_mtk_port() {
            info!("Found MTK port: {}", port.get_port_name());
            break port;
        } else if last_seen.elapsed() > timeout {
            state.reset().await?;
            last_seen = Instant::now();
        }
    };

    let usb_log = usb_log_channel;

    let mut builder = DeviceBuilder::default()
        .with_mtk_port(mtk_port)
        .with_verbose(args.verbose)
        .with_usb_log_channel(usb_log);

    if usb_log {
        if let Some(device_log) = setup_file_logger(DA_LOG_FILE).await {
            builder = builder.with_device_log(device_log);
        }
    }

    builder = if let Some(da) = da_data {
        builder.with_da_data(da)
    } else if let Some(da_path_str) = &state.da_file_path {
        let da_path = Path::new(da_path_str);
        let data = read(da_path).await?;
        builder.with_da_data(data)
    } else {
        builder
    };

    builder = if let Some(pl) = pl_data { builder.with_preloader(pl) } else { builder };
    builder = if let Some(auth) = auth_data { builder.with_auth(auth) } else { builder };

    let mut dev = builder.build()?;

    if state.hw_code != 0 {
        let dev_info = DevInfoData {
            soc_id: state.soc_id,
            meid: state.meid,
            hw_code: state.hw_code,
            partitions: vec![],
            target_config: state.target_config,
        };

        if state.flash_mode != 0 {
            dev.set_connection_type(ConnectionType::Da)?;
        }

        dev.dev_info.set_chip(penumbra::core::chip::chip_from_hw_code(state.hw_code));
        dev.reinit(dev_info)?;
    } else {
        info!("Initializing device...");
        dev.init()?;

        state.soc_id = dev.dev_info.soc_id();
        state.meid = dev.dev_info.meid();
        state.hw_code = dev.dev_info.hw_code();
        state.target_config = dev.dev_info.target_config();

        state.save().await?;
    }

    info!("=====================================");
    info!("SBC: {}", (state.target_config & 0x1) != 0);
    info!("SLA: {}", (state.target_config & 0x2) != 0);
    info!("DAA: {}", (state.target_config & 0x4) != 0);
    info!("=====================================");

    if let Some(cmd) = &args.command {
        cmd.run(&mut dev, &mut state)?;
        state.target_config = dev.dev_info.target_config(); // Update just in case after Kamakiri
        state.save().await?;
    }

    Ok(())
}
