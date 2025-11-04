/*
    SPDX-License-Identifier: AGPL-3.0-or-later
    SPDX-FileCopyrightText: 2025 Shomy
*/
use std::sync::Arc;

use tokio::sync::RwLock;

use crate::core::storage::{Partition, Storage};

/// Safe wrapper around device information with async read/write access.
#[derive(Clone, Default)]
pub struct DeviceInfo {
    inner: Arc<RwLock<DevInfoData>>,
}

/// Struct holding device information data.
/// This should not be accessed directly, instead use the `DeviceInfo` wrapper.
#[derive(Clone, Default)]
pub struct DevInfoData {
    pub chipset: String,
    pub soc_id: Vec<u8>,
    pub meid: Vec<u8>,
    pub hw_code: u16,
    pub partitions: Vec<Partition>,
    pub storage: Option<Arc<dyn Storage + Send + Sync>>,
    pub target_config: u32,
}

impl DeviceInfo {
    pub fn new() -> Self {
        DeviceInfo { inner: Arc::new(RwLock::new(DevInfoData::default())) }
    }

    fn inner(&self) -> &Arc<RwLock<DevInfoData>> {
        &self.inner
    }

    pub async fn get_data(&self) -> DevInfoData {
        self.inner().read().await.clone()
    }

    pub async fn set_data(&self, data: DevInfoData) {
        let mut write_guard = self.inner().write().await;
        *write_guard = data;
    }

    pub async fn chipset(&self) -> String {
        self.inner().read().await.chipset.clone()
    }

    pub async fn soc_id(&self) -> Vec<u8> {
        self.inner().read().await.soc_id.clone()
    }

    pub async fn meid(&self) -> Vec<u8> {
        self.inner().read().await.meid.clone()
    }

    pub async fn hw_code(&self) -> u16 {
        self.inner().read().await.hw_code
    }

    pub async fn partitions(&self) -> Vec<Partition> {
        self.inner().read().await.partitions.clone()
    }

    pub async fn storage(&self) -> Option<Arc<dyn Storage + Send + Sync>> {
        self.inner().read().await.storage.clone()
    }

    pub async fn set_storage(&self, storage: Arc<dyn Storage + Send + Sync>) {
        let mut write_guard = self.inner().write().await;
        write_guard.storage = Some(storage);
    }

    pub async fn get_partition(&self, name: &str) -> Option<Partition> {
        let partitions = self.inner().read().await.partitions.clone();
        partitions.into_iter().find(|p| p.name == name)
    }

    pub async fn set_partitions(&self, partitions: Vec<Partition>) {
        let mut write_guard = self.inner().write().await;
        write_guard.partitions = partitions;
    }

    pub async fn target_config(&self) -> u32 {
        self.inner().read().await.target_config
    }

    pub async fn sbc_enabled(&self) -> bool {
        let target_config = self.inner().read().await.target_config;
        (target_config & 0x1) != 0
    }

    pub async fn sla_enabled(&self) -> bool {
        let target_config = self.inner().read().await.target_config;
        (target_config & 0x2) != 0
    }

    pub async fn daa_enabled(&self) -> bool {
        let target_config = self.inner().read().await.target_config;
        (target_config & 0x4) != 0
    }
}
