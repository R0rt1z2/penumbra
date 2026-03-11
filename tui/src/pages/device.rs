/*
    SPDX-License-Identifier: AGPL-3.0-or-later
    SPDX-FileCopyrightText: 2025 Shomy
*/
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Result, anyhow};
use async_trait::async_trait;
use human_bytes::human_bytes;
use penumbra::core::devinfo::DevInfoData;
use penumbra::core::seccfg::LockFlag;
use penumbra::core::storage::{Partition, Storage};
use penumbra::{Device, DeviceBuilder, find_mtk_port};
#[cfg(target_os = "windows")]
use ratatui::crossterm::event::KeyEventKind;
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::prelude::{Alignment, Frame};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph, Row, Table};
use strum::IntoEnumIterator;
use strum_macros::{AsRefStr, EnumIter};
use tokio::fs::File;
use tokio::io::{BufReader, BufWriter};
use tokio::spawn;
use tokio::sync::{Mutex, mpsc};
use tokio::task::JoinHandle;
use tokio::time::{Duration, sleep};

use crate::app::{AppCtx, AppPage};
use crate::components::selectable_list::{
    ListItemEntry,
    ListItemEntryBuilder,
    SelectableList,
    SelectableListBuilder,
};
use crate::components::{
    ExplorerResult,
    FileExplorer,
    ProgressBar,
    Stars,
    ThemedWidgetMut,
    ThemedWidgetRef,
};
use crate::pages::Page;

/// Which panel is currently focused
pub enum FocusedPanel {
    Menu,
    PartitionMenu,
}

/// Device connection status, used for UI updates
#[derive(Debug, Clone, PartialEq)]
pub enum DeviceStatus {
    Disconnected,
    Connecting,
    Connected,
}

/// A list of event to which the DevicePage can respond
pub enum DeviceEvent {
    // Progress Bar Events
    /// Start a progress operation, and set the max bytes
    /// and a message
    ProgressStart {
        total_bytes: u64,
        message: String,
    },
    /// Update progress with bytes written
    /// If message is Some, update the message as well
    ProgressUpdate {
        written: u64,
        message: Option<String>,
    },
    /// Finish progress with a final message
    ProgressFinish {
        message: String,
    },
    /// Notify of device status change (Disconnected, Connecting, Connected)
    StatusChanged(DeviceStatus),
    /// Notify that device is connected (To be sent once)
    Connected(Device),

    /// Change focused panel
    FocusPanel(FocusedPanel),
    /// Show the provided file explorer
    ShowExplorer(FileExplorer),
    /// Yield result from the file explorer
    ExplorerResult(ExplorerResult),

    // Opens the dialog with an error message
    Error(String),
    // Little text on top
    HeaderStatus(String),

    /// Whether to enable or disable input.
    /// Used to block input during operations
    Input(bool),
}

/// A list of event used by the page and callbacks to communicate to
/// each other.
/// Works via a bi-directional channel.
#[derive(Debug, Clone)]
pub enum CallbackEvent {
    /// All selected partitions
    PartitionsSelected(Vec<Partition>),
    /// A partition that got selected or unselected in the explorer
    PartitionToggled(Partition, bool),
    ExplorerResult(ExplorerResult),
}

/// The Menu Actions available
/// Used for both mapping a menu entry to a callback, and rendering the menu
#[derive(EnumIter, AsRefStr, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DeviceAction {
    #[strum(serialize = "Unlock Bootloader")]
    UnlockBootloader,
    #[strum(serialize = "Lock Bootloader")]
    LockBootloader,
    #[strum(serialize = "Read Partition")]
    ReadPartition,
    #[strum(serialize = "Write Partition")]
    WritePartition,
    #[strum(serialize = "Back to Menu")]
    BackToMenu,
}

/// Represent a callback for a device action
/// The callback is executed in an async task, allowing for background operations.
/// The callback can communicate with the page via the provided channels.
/// * Event TX: To ask to the page to perform UI actions, specifically for UI events
/// * Callback TX/RX: To communicate with the page for specific data (like selected partitions)
#[async_trait]
pub trait DeviceActionCallback: Send + Sync {
    async fn execute(
        &self,
        device: Arc<Mutex<Device>>,
        event_tx: mpsc::Sender<DeviceEvent>,
        cb_tx: mpsc::Sender<CallbackEvent>,
        cb_rx: mpsc::Receiver<CallbackEvent>,
    ) -> Result<()>;
}

pub struct DeviceState {
    pub status: DeviceStatus,
    pub last_status_change: Instant,
}

impl DeviceState {
    pub fn new() -> Self {
        Self { status: DeviceStatus::Disconnected, last_status_change: Instant::now() }
    }

    pub fn set_status(&mut self, status: DeviceStatus) {
        self.status = status;
        self.last_status_change = Instant::now();
    }

    pub fn is_connected(&self) -> bool {
        matches!(self.status, DeviceStatus::Connected)
    }
}

pub struct DevicePage {
    pub device: Option<Arc<Mutex<Device>>>,
    pub device_state: DeviceState,
    pub status_message: Option<String>,

    // Event channel for async communication
    /// UI Events
    pub event_tx: mpsc::Sender<DeviceEvent>,
    pub event_rx: mpsc::Receiver<DeviceEvent>,
    /// Callbacks Events, for transmitting Callback results
    pub callback_tx: Option<mpsc::Sender<CallbackEvent>>,
    pub callback_rx: Option<mpsc::Receiver<CallbackEvent>>,

    // Action callbacks and active operations
    pub action_callbacks: HashMap<DeviceAction, Arc<dyn DeviceActionCallback>>,
    pub active_operations: HashMap<DeviceAction, JoinHandle<()>>,

    // UI components (foundation only, not rendered yet)
    stars: Stars,
    progress_bar: ProgressBar,
    menu: SelectableList,
    partition_list: SelectableList,
    explorer: Option<FileExplorer>,

    // UI State
    pub focused_panel: FocusedPanel,
    pub input_enabled: bool,

    // Various Device Info
    pub partitions: Vec<Partition>,
    pub devinfo: Option<DevInfoData>,
    pub storage: Option<Arc<dyn Storage + Send + Sync>>,
}

impl DevicePage {
    pub fn new() -> Self {
        let (event_tx, event_rx) = mpsc::channel(32);
        let progress_bar = ProgressBar::new();

        // Build menu from actions
        let actions: Vec<DeviceAction> = DeviceAction::iter().collect();
        let menu_items: Vec<ListItemEntry> = actions
            .iter()
            .map(|action| {
                let icon = match action {
                    DeviceAction::UnlockBootloader => '🔓',
                    DeviceAction::LockBootloader => '🔒',
                    DeviceAction::ReadPartition => '📁',
                    DeviceAction::WritePartition => '📝',
                    DeviceAction::BackToMenu => '↩',
                };
                ListItemEntryBuilder::new(action.as_ref().to_string()).icon(icon).build().unwrap()
            })
            .collect();

        let menu = SelectableListBuilder::default()
            .items(menu_items)
            .highlight_symbol(">> ".to_string())
            .build()
            .unwrap();

        let partition_list = SelectableListBuilder::default()
            .items(Vec::new())
            .highlight_symbol(">> ".to_string())
            .build()
            .unwrap();

        let mut page = Self {
            device: None,
            device_state: DeviceState::new(),
            status_message: None,
            event_tx,
            event_rx,
            callback_tx: None,
            callback_rx: None,
            action_callbacks: HashMap::new(),
            active_operations: HashMap::new(),
            stars: Stars::default(),
            progress_bar,
            menu,
            explorer: None,
            focused_panel: FocusedPanel::Menu,
            input_enabled: true,
            partition_list,
            partitions: Vec::new(),
            devinfo: None,
            storage: None,
        };

        page.register_action(DeviceAction::UnlockBootloader, Arc::new(UnlockBootloaderCallback));
        page.register_action(DeviceAction::LockBootloader, Arc::new(LockBootloaderCallback));
        page.register_action(DeviceAction::ReadPartition, Arc::new(ReadPartitionCallback));
        page.register_action(DeviceAction::WritePartition, Arc::new(WritePartitionCallback));

        page
    }

    pub fn register_action(
        &mut self,
        action: DeviceAction,
        callback: Arc<dyn DeviceActionCallback>,
    ) {
        self.action_callbacks.insert(action, callback);
    }

    pub async fn execute_action(&mut self, action: DeviceAction) {
        // Abort any existing operation for the same action, as a safety measure
        if let Some(handle) = self.active_operations.remove(&action) {
            handle.abort();
        }

        let Some(device) = self.device.clone() else {
            self.event_tx.send(DeviceEvent::Error("Device not connected".to_string())).await.ok();
            return;
        };

        let Some(callback) = self.action_callbacks.get(&action).cloned() else {
            self.event_tx.send(DeviceEvent::Error("No callback registered".to_string())).await.ok();
            return;
        };

        // From executor to callback
        let (cb_tx_to_callback, cb_rx_from_callback) = mpsc::channel(1);
        // From callback to executor
        let (cb_tx_from_callback, cb_rx_to_main) = mpsc::channel(1);

        self.callback_tx = Some(cb_tx_to_callback);
        self.callback_rx = Some(cb_rx_to_main);

        let event_tx = self.event_tx.clone();

        let handle = tokio::spawn(async move {
            let result = callback
                .execute(device, event_tx.clone(), cb_tx_from_callback, cb_rx_from_callback)
                .await;
            if let Err(e) = result {
                event_tx.send(DeviceEvent::Error(e.to_string())).await.ok();
            }
        });

        self.active_operations.insert(action, handle);
    }

    /// Process all pending events from the event channel
    pub async fn process_events(&mut self, ctx: &mut AppCtx) {
        while let Ok(event) = self.event_rx.try_recv() {
            match event {
                DeviceEvent::ProgressStart { total_bytes, message } => {
                    self.progress_bar.start(total_bytes, message);
                }
                DeviceEvent::ProgressUpdate { written, message } => {
                    self.progress_bar.set_written(written);
                    if let Some(msg) = message {
                        self.progress_bar.set_message(msg);
                    }
                }
                DeviceEvent::ProgressFinish { message } => {
                    self.progress_bar.finish();
                    self.status_message = Some(message);
                }

                DeviceEvent::StatusChanged(status) => {
                    self.device_state.set_status(status);
                }
                DeviceEvent::Connected(mut device) => {
                    self.devinfo = Some(device.dev_info.get_data().await);

                    let partitions = device.get_partitions().await;
                    let partition_list_items: Vec<ListItemEntry> = partitions
                        .iter()
                        .map(|p| {
                            ListItemEntryBuilder::new(format!(
                                "{} ({})",
                                p.name,
                                human_bytes(p.size as f64)
                            ))
                            .value(p.name.clone())
                            .build()
                            .unwrap()
                        })
                        .collect();

                    self.partition_list.items = partition_list_items;

                    self.partitions = partitions;
                    self.storage = device.dev_info.storage().await.clone();
                    self.device = Some(Arc::new(Mutex::new(device)));
                    self.device_state.set_status(DeviceStatus::Connected);
                }

                DeviceEvent::FocusPanel(panel) => {
                    self.focused_panel = panel;
                }
                DeviceEvent::Input(flag) => {
                    self.input_enabled = flag;
                }
                DeviceEvent::ShowExplorer(explorer) => {
                    self.explorer = Some(explorer);
                }
                DeviceEvent::ExplorerResult(result) => {
                    match result {
                        ExplorerResult::Cancelled | ExplorerResult::Selected(_) => {
                            log::debug!("Closing file explorer");
                            self.explorer = None;
                        }
                        _ => {
                            log::debug!("Explorer result received: {:?}", result);
                        }
                    }

                    if let Some(cb_tx) = &self.callback_tx {
                        cb_tx.send(CallbackEvent::ExplorerResult(result)).await.ok();
                    }
                }
                DeviceEvent::Error(msg) => {
                    error_dialog!(ctx, msg);
                }
                DeviceEvent::HeaderStatus(msg) => {
                    self.status_message = Some(msg);
                }
            }
        }
    }

    pub fn cancel_all_operations(&mut self) {
        for (_, handle) in self.active_operations.drain() {
            handle.abort();
        }
    }

    pub fn connect_device(&mut self, ctx: &mut AppCtx) {
        if self.device.is_some() || self.device_state.status == DeviceStatus::Connecting {
            return;
        }

        let tx = self.event_tx.clone();

        let da_data = ctx.loader().map(|da| da.file().da_raw_data.clone());
        let pl_data = ctx.preloader().map(|pl| pl.data());

        spawn(async move {
            let port = loop {
                match find_mtk_port(None).await {
                    Some(p) => break p,
                    None => sleep(Duration::from_millis(700)).await,
                }
            };
            let _ = tx.send(DeviceEvent::StatusChanged(DeviceStatus::Connecting)).await;

            let mut devbuilder = DeviceBuilder::default().with_mtk_port(port);

            if let Some(da) = da_data {
                devbuilder = devbuilder.with_da_data(da);
            }
            if let Some(pl) = pl_data {
                devbuilder = devbuilder.with_preloader(pl);
            }

            match devbuilder.build() {
                Ok(mut dev) => {
                    if let Err(e) = dev.init().await {
                        let _ = tx.send(DeviceEvent::Error(format!("Init failed: {}", e))).await;
                        let _ =
                            tx.send(DeviceEvent::StatusChanged(DeviceStatus::Disconnected)).await;
                        return;
                    }

                    if let Err(e) = dev.enter_da_mode().await {
                        let _ = tx.send(DeviceEvent::Error(format!("DA Mode failed: {}", e))).await;
                        let _ =
                            tx.send(DeviceEvent::StatusChanged(DeviceStatus::Disconnected)).await;
                        return;
                    }

                    let _ = tx.send(DeviceEvent::Connected(dev)).await;
                }
                Err(e) => {
                    let _ = tx.send(DeviceEvent::Error(format!("Build failed: {}", e))).await;
                    let _ = tx.send(DeviceEvent::StatusChanged(DeviceStatus::Disconnected)).await;
                }
            }
        });
    }

    /// Handles the action menu input
    async fn handle_menu_input(&mut self, ctx: &mut AppCtx, key: KeyEvent) {
        match key.code {
            KeyCode::Up => self.menu.previous(),
            KeyCode::Down => self.menu.next(),

            KeyCode::Right => {
                if self.device_state.is_connected() {
                    let _ = self
                        .event_tx
                        .send(DeviceEvent::FocusPanel(FocusedPanel::PartitionMenu))
                        .await;
                }
            }

            KeyCode::Enter => {
                if let Some(idx) = self.menu.selected_index()
                    && let Some(action) = DeviceAction::iter().nth(idx)
                {
                    if action == DeviceAction::BackToMenu {
                        ctx.change_page(AppPage::Welcome);
                        return;
                    }
                    self.execute_action(action).await;
                }
            }

            _ => {}
        }
    }

    /// Handles the partition menu input
    async fn handle_partition_input(&mut self, _ctx: &mut AppCtx, key: KeyEvent) {
        match key.code {
            KeyCode::Up => self.partition_list.previous(),
            KeyCode::Down => self.partition_list.next(),

            KeyCode::Esc => {
                self.partition_list.toggled = false;
                self.partition_list.clear_selections();
                let _ = self.event_tx.send(DeviceEvent::FocusPanel(FocusedPanel::Menu)).await;
            }

            KeyCode::Enter => {
                if let Some(cb_tx) = &self.callback_tx {
                    let selected: HashSet<&str> = self
                        .partition_list
                        .checked_items()
                        .iter()
                        .filter_map(|item| item.value.as_deref())
                        .collect();

                    let partitions: Vec<_> = self
                        .partitions
                        .iter()
                        .filter(|p| selected.contains(p.name.as_str()))
                        .cloned()
                        .collect();

                    if !partitions.is_empty() {
                        let _ = cb_tx.send(CallbackEvent::PartitionsSelected(partitions)).await;
                    }
                }
            }
            KeyCode::Char('x') => {
                self.partition_list.toggled = true;
                self.partition_list.toggle_selected();

                let cb_tx = match &self.callback_tx {
                    Some(tx) => tx,
                    None => return,
                };

                let part_item = match self.partition_list.selected_item() {
                    Some(part) => part,
                    None => return,
                };

                let value = match &part_item.value {
                    Some(value) => value,
                    None => return,
                };

                let part = match self.partitions.iter().find(|p| p.name == *value) {
                    Some(part) => part,
                    None => return,
                };

                let is_checked = part_item.is_toggled();
                cb_tx.send(CallbackEvent::PartitionToggled(part.clone(), is_checked)).await.ok();
            }
            _ => {}
        }
    }

    /// Renders the background (stars :D)
    fn render_background(&mut self, frame: &mut Frame<'_>, area: Rect, ctx: &mut AppCtx) {
        self.stars.render(area, frame.buffer_mut(), &ctx.theme);
        self.stars.tick();
    }

    /// Renders the whole layout
    fn render_layout(&mut self, frame: &mut Frame<'_>, area: Rect, ctx: &mut AppCtx) {
        let vertical = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3), // Header
                Constraint::Min(0),    // Content
                Constraint::Length(5), // Progress
                Constraint::Length(1), // Footer
            ])
            .margin(1)
            .split(area);

        let centered = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Min(0)])
            .split(vertical[1]);

        self.render_header(frame, vertical[0], ctx);
        self.render_content(frame, centered[0], ctx);
        self.render_progress(frame, vertical[2], ctx);
        self.render_footer(frame, vertical[3], ctx);
    }

    /// Header banner
    fn render_header(&self, frame: &mut Frame<'_>, area: Rect, ctx: &mut AppCtx) {
        let status = match &self.device_state.status {
            DeviceStatus::Disconnected => {
                Span::styled("  Disconnected ", Style::default().fg(ctx.theme.muted))
            }
            DeviceStatus::Connecting => {
                Span::styled("  Connecting… ", Style::default().fg(ctx.theme.warning))
            }
            DeviceStatus::Connected => {
                Span::styled("  Connected ", Style::default().fg(ctx.theme.success))
            }
        };

        let header = Paragraph::new(Line::from(vec![
            Span::styled(" Antumbra ", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(" | "),
            status,
            Span::raw(" | "),
            Span::styled(
                self.status_message.as_deref().unwrap_or(" "),
                Style::default().fg(ctx.theme.info),
            ),
        ]))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .style(Style::default().fg(ctx.theme.accent)),
        )
        .alignment(Alignment::Left);

        frame.render_widget(header, area);
    }

    /// Menu + Device Info
    fn render_content(&mut self, frame: &mut Frame<'_>, area: Rect, ctx: &mut AppCtx) {
        let chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Percentage(30),
                Constraint::Length(2),
                Constraint::Percentage(70),
            ])
            .split(area);

        self.render_menu(frame, chunks[0], ctx);
        self.render_device_info(frame, chunks[2], ctx);
    }

    /// Action menu
    fn render_menu(&mut self, frame: &mut Frame<'_>, area: Rect, ctx: &mut AppCtx) {
        let block = Block::default()
            .title(" ACTIONS ")
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(ctx.theme.text));

        frame.render_widget(block.clone(), area);
        self.menu.render(block.inner(area), frame.buffer_mut(), &ctx.theme);
    }

    /// Device info card
    fn render_device_info(&mut self, frame: &mut Frame<'_>, area: Rect, ctx: &mut AppCtx) {
        let block = Block::default()
            .title(" DEVICE INFO ")
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(ctx.theme.text));

        frame.render_widget(block.clone(), area);
        let inner = block.inner(area);

        if !self.device_state.is_connected() {
            self.render_disconnected(frame, inner, ctx);
            return;
        }

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(8), Constraint::Length(1), Constraint::Min(0)])
            .split(inner);

        self.render_device_table(frame, chunks[0], ctx);
        self.partition_list.render(chunks[2], frame.buffer_mut(), &ctx.theme);
    }

    /// Disconnected message
    fn render_disconnected(&self, frame: &mut Frame<'_>, area: Rect, ctx: &mut AppCtx) {
        let message = Paragraph::new(vec![
            Line::from(""),
            Line::from(Span::styled(
                " Waiting for device connection…",
                Style::default().fg(ctx.theme.warning).add_modifier(Modifier::BOLD),
            )),
            Line::from(Span::styled(
                " (Plug device in BOOTROM or Preloader mode)",
                Style::default().fg(ctx.theme.muted),
            )),
        ])
        .alignment(Alignment::Center);

        frame.render_widget(message, area);
    }

    /// Device configuration table
    fn render_device_table(&self, frame: &mut Frame<'_>, area: Rect, ctx: &mut AppCtx) {
        let Some(devinfo) = &self.devinfo else { return };

        let hw_code = format!("0x{:X}", devinfo.hw_code);

        let sbc = if devinfo.target_config & 0x1 != 0 { "Yes" } else { "No" };
        let sla = if devinfo.target_config & 0x2 != 0 { "Yes" } else { "No" };
        let daa = if devinfo.target_config & 0x4 != 0 { "Yes" } else { "No" };

        let rows = vec![
            Row::new(vec!["HW Code", hw_code.as_str()]),
            Row::new(vec!["Secure Boot (SBC)", sbc]),
            Row::new(vec!["Serial Link Auth (SLA)", sla]),
            Row::new(vec!["Download Agent Auth (DAA)", daa]),
        ];

        let table = Table::new(rows, [Constraint::Percentage(45), Constraint::Percentage(55)])
            .block(Block::default().borders(Borders::BOTTOM))
            .column_spacing(1)
            .style(Style::default().fg(ctx.theme.text));

        frame.render_widget(table, area);
    }

    /// Progress bar
    fn render_progress(&self, frame: &mut Frame<'_>, area: Rect, ctx: &mut AppCtx) {
        let block = Block::default()
            .title(" PROGRESS ")
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .style(Style::default().fg(ctx.theme.accent));

        frame.render_widget(block.clone(), area);

        let inner = block.inner(area);

        // Give the progress bar exactly 3 rows
        let bar_area = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(3)])
            .split(inner)[0];

        self.progress_bar.render_ref(bar_area, frame.buffer_mut(), &ctx.theme);
    }

    /// Footer help text
    fn render_footer(&self, frame: &mut Frame<'_>, area: Rect, ctx: &mut AppCtx) {
        let footer = Paragraph::new("[↑↓] Navigate   [Enter] Select   [Esc] Back")
            .alignment(Alignment::Center)
            .style(Style::default().fg(ctx.theme.foreground));

        frame.render_widget(footer, area);
    }
}

#[async_trait]
impl Page for DevicePage {
    fn render(&mut self, frame: &mut Frame<'_>, ctx: &mut AppCtx) {
        let area = frame.area();

        self.render_background(frame, area, ctx);
        self.render_layout(frame, area, ctx);

        if let Some(explorer) = &mut self.explorer {
            explorer.render_modal(area, frame.buffer_mut(), &ctx.theme);
        }
    }

    async fn handle_input(&mut self, ctx: &mut AppCtx, key: KeyEvent) {
        #[cfg(target_os = "windows")]
        if key.kind != KeyEventKind::Press {
            return;
        }

        if !self.input_enabled {
            return;
        }

        // The explorer takes priority if active
        if let Some(explorer) = &mut self.explorer {
            let result = explorer.handle_key(key);
            let _ = self.event_tx.send(DeviceEvent::ExplorerResult(result)).await;
            return;
        }

        match self.focused_panel {
            FocusedPanel::Menu => self.handle_menu_input(ctx, key).await,
            FocusedPanel::PartitionMenu => self.handle_partition_input(ctx, key).await,
        }
    }

    async fn on_enter(&mut self, ctx: &mut AppCtx) {
        self.device_state.set_status(DeviceStatus::Disconnected);

        self.connect_device(ctx);
    }

    async fn on_exit(&mut self, _ctx: &mut AppCtx) {
        self.cancel_all_operations();
        // TOOD: Add device shutdown if connected
    }

    async fn update(&mut self, ctx: &mut AppCtx) {
        self.process_events(ctx).await;
    }
}

pub struct UnlockBootloaderCallback;
#[async_trait]
impl DeviceActionCallback for UnlockBootloaderCallback {
    async fn execute(
        &self,
        device: Arc<Mutex<Device>>,
        event_tx: mpsc::Sender<DeviceEvent>,
        _cb_tx: mpsc::Sender<CallbackEvent>,
        _cb_rx: mpsc::Receiver<CallbackEvent>,
    ) -> Result<()> {
        let _ = event_tx.send(DeviceEvent::HeaderStatus("Unlocking bootloader...".into())).await;

        let mut dev = device.lock().await;
        match dev.set_seccfg_lock_state(LockFlag::Unlock).await {
            Some(_) => {
                let _ =
                    event_tx.send(DeviceEvent::HeaderStatus("Bootloader unlocked.".into())).await;
                Ok(())
            }
            None => Err(anyhow!("Failed to unlock bootloader")),
        }
    }
}

pub struct LockBootloaderCallback;
#[async_trait]
impl DeviceActionCallback for LockBootloaderCallback {
    async fn execute(
        &self,
        device: Arc<Mutex<Device>>,
        event_tx: mpsc::Sender<DeviceEvent>,
        _cb_tx: mpsc::Sender<CallbackEvent>,
        _cb_rx: mpsc::Receiver<CallbackEvent>,
    ) -> Result<()> {
        event_tx.send(DeviceEvent::HeaderStatus("Locking bootloader...".into())).await.ok();

        let mut dev = device.lock().await;
        match dev.set_seccfg_lock_state(LockFlag::Unlock).await {
            Some(_) => {
                event_tx.send(DeviceEvent::HeaderStatus("Bootloader locked.".into())).await.ok();
                Ok(())
            }
            None => Err(anyhow!("Failed to lock bootloader")),
        }
    }
}

pub struct ReadPartitionCallback;
#[async_trait]
impl DeviceActionCallback for ReadPartitionCallback {
    async fn execute(
        &self,
        device: Arc<Mutex<Device>>,
        event_tx: mpsc::Sender<DeviceEvent>,
        _cb_tx: mpsc::Sender<CallbackEvent>,
        mut cb_rx: mpsc::Receiver<CallbackEvent>,
    ) -> Result<()> {
        let _ = event_tx.send(DeviceEvent::FocusPanel(FocusedPanel::PartitionMenu)).await;

        let explorer = FileExplorer::new("Output dump directory")?.directories_only();

        let partitions = loop {
            match cb_rx.recv().await {
                Some(CallbackEvent::PartitionsSelected(parts)) => break parts,
                Some(CallbackEvent::ExplorerResult(ExplorerResult::Cancelled)) => {
                    return Ok(());
                }
                _ => {}
            }
        };

        let _ = event_tx.send(DeviceEvent::ShowExplorer(explorer)).await;

        let output_dir = loop {
            match cb_rx.recv().await {
                Some(CallbackEvent::ExplorerResult(ExplorerResult::Selected(path))) => {
                    break path;
                }
                Some(CallbackEvent::ExplorerResult(ExplorerResult::Cancelled)) => {
                    return Ok(());
                }
                _ => {}
            }
        };

        let total_size = partitions.iter().map(|p| p.size as u64).sum::<u64>();

        let mut bytes_read: u64 = 0;

        let mut dev = device.lock().await;
        // Block page input to avoid interruptions
        event_tx.send(DeviceEvent::Input(false)).await.ok();

        event_tx
            .send(DeviceEvent::ProgressStart {
                total_bytes: total_size,
                message: "Reading partitions...".into(),
            })
            .await
            .ok();
        for partition in partitions {
            let output_path = output_dir.join(format!("{}.bin", partition.name));
            let file = File::create(&output_path).await?;
            let mut writer = BufWriter::new(file);

            let mut progress_cb = |written: usize, _total_partition_bytes: usize| {
                let total_bytes = bytes_read + written as u64;

                let event_tx = event_tx.clone();
                let part_name = partition.name.clone();
                spawn(async move {
                    let _ = event_tx
                        .send(DeviceEvent::ProgressUpdate {
                            written: total_bytes,
                            message: Some(format!("Reading partition '{}'...", part_name,)),
                        })
                        .await;
                });
            };

            dev.upload(&partition.name, &mut writer, &mut progress_cb).await?;

            bytes_read += partition.size as u64;
        }

        let _ = event_tx
            .send(DeviceEvent::ProgressFinish { message: "Partition read complete.".into() })
            .await;

        // Focus back the menu panel to avoid confusion
        let _ = event_tx.send(DeviceEvent::FocusPanel(FocusedPanel::Menu)).await;
        event_tx.send(DeviceEvent::Input(true)).await.ok();

        Ok(())
    }
}

pub struct WritePartitionCallback;
#[async_trait]
impl DeviceActionCallback for WritePartitionCallback {
    async fn execute(
        &self,
        device: Arc<Mutex<Device>>,
        event_tx: mpsc::Sender<DeviceEvent>,
        _cb_tx: mpsc::Sender<CallbackEvent>,
        mut cb_rx: mpsc::Receiver<CallbackEvent>,
    ) -> Result<()> {
        event_tx.send(DeviceEvent::FocusPanel(FocusedPanel::PartitionMenu)).await.ok();

        let mut partition_map: HashMap<String, PathBuf> = HashMap::new();

        let partitions: Vec<Partition>;

        loop {
            match cb_rx.recv().await {
                Some(CallbackEvent::PartitionToggled(partition, selected)) => {
                    if selected {
                        // Show file explorer to select partition file
                        let explorer = FileExplorer::new(format!(
                            "Select file for partition '{}'",
                            partition.name
                        ))?;

                        event_tx.send(DeviceEvent::ShowExplorer(explorer)).await.ok();

                        let path = loop {
                            match cb_rx.recv().await {
                                Some(CallbackEvent::ExplorerResult(ExplorerResult::Selected(
                                    path,
                                ))) => break path,
                                Some(CallbackEvent::ExplorerResult(ExplorerResult::Cancelled)) => {
                                    continue;
                                }
                                _ => {}
                            }
                        };

                        partition_map.insert(partition.name.clone(), path);
                    } else {
                        partition_map.remove(&partition.name);
                    }
                }
                Some(CallbackEvent::ExplorerResult(ExplorerResult::Cancelled)) => {
                    continue;
                }
                Some(CallbackEvent::PartitionsSelected(parts)) => {
                    partitions = parts;
                    break;
                }
                _ => {}
            }
        }

        let part_to_write: Vec<(Partition, PathBuf)> = partitions
            .into_iter()
            .filter_map(|p| partition_map.get(&p.name).cloned().map(|path| (p, path)))
            .collect();

        let total_size = part_to_write.iter().map(|(p, _)| p.size as u64).sum::<u64>();

        let mut bytes_written: u64 = 0;

        let mut dev = device.lock().await;
        // Block page input to avoid interruptions
        event_tx.send(DeviceEvent::Input(false)).await.ok();

        event_tx
            .send(DeviceEvent::ProgressStart {
                total_bytes: total_size,
                message: "Writing partitions...".into(),
            })
            .await
            .ok();

        for (partition, path) in part_to_write {
            let file = File::open(path).await?;
            let mut reader = BufReader::new(file);

            let mut progress_cb = |written: usize, _total_partition_bytes: usize| {
                let total_bytes = bytes_written + written as u64;

                let event_tx = event_tx.clone();
                let part_name = partition.name.clone();
                spawn(async move {
                    let _ = event_tx
                        .send(DeviceEvent::ProgressUpdate {
                            written: total_bytes,
                            message: Some(format!("Flashing partition '{}'...", part_name,)),
                        })
                        .await;
                });
            };

            dev.download(&partition.name, partition.size, &mut reader, &mut progress_cb).await?;

            bytes_written += partition.size as u64;
        }

        let _ = event_tx
            .send(DeviceEvent::ProgressFinish { message: "Partition write complete.".into() })
            .await;

        // Focus back the menu panel to avoid confusion
        let _ = event_tx.send(DeviceEvent::FocusPanel(FocusedPanel::Menu)).await;
        event_tx.send(DeviceEvent::Input(true)).await.ok();

        Ok(())
    }
}
