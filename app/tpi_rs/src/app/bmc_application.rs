use super::bits_trait::ToBits;
use crate::middleware::usbboot::FlashingError;
use crate::middleware::{
    app_persistency::ApplicationPersistency, event_listener::EventListener,
    pin_controller::PinController, usbboot, NodeId, UsbMode, UsbRoute,
};
use anyhow::{ensure, Context};
use evdev::Key;
use log::debug;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc::Receiver;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tokio::time::sleep;

/// Stores which slots are actually used. This information is used to determine
/// for instance, which nodes need to be powered on, when such command is given
const NODE_ENABLED_KEY: &str = "node_enabled";
/// stores to which node the usb multiplexer is configured to.
const USB_NODE_KEY: &str = "usb_node";
const USB_ROUTE_KEY: &str = "usb_route";
const USB_MODE_KEY: &str = "usb_mode";

#[derive(Debug)]
pub struct BmcApplication {
    pin_controller: PinController,
    app_db: ApplicationPersistency,
    power_state: Mutex<u8>,
}

impl BmcApplication {
    pub async fn new() -> anyhow::Result<Arc<Self>> {
        let pin_controller = PinController::new()?;
        let app_db = ApplicationPersistency::new().await?;

        let instance = Arc::new(Self {
            pin_controller,
            app_db,
            power_state: Mutex::new(0),
        });

        instance.initialize().await?;

        // start listening for device events.
        EventListener::new(instance.clone())
            .add_action_async(Key::KEY_1, 1, |app| {
                Box::pin(Self::toggle_power_states(app.clone()))
            })
            .add_action_async(Key::KEY_POWER, 1, |app| {
                Box::pin(Self::toggle_power_states(app.clone()))
            })
            .add_action_async(Key::KEY_RESTART, 1, |_| Box::pin(async { reboot() }))
            .run()?;

        Ok(instance)
    }

    async fn toggle_power_states(app: Arc<BmcApplication>) -> anyhow::Result<()> {
        let mut lock = app.power_state.lock().await;
        *lock = !*lock;
        app.power_node(*lock).await
    }

    async fn initialize(&self) -> anyhow::Result<()> {
        self.initialize_usb_mode().await?;
        self.initialize_power().await
    }

    async fn initialize_power(&self) -> anyhow::Result<()> {
        // power on nodes
        if let Ok(enabled_nodes) = self.app_db.get::<u8>(NODE_ENABLED_KEY).await {
            self.power_node(enabled_nodes).await
        } else {
            // default, given a new app persistency
            self.app_db.set::<u8>(NODE_ENABLED_KEY, 0).await
        }
    }

    async fn initialize_usb_mode(&self) -> std::io::Result<()> {
        let node = self
            .app_db
            .get::<NodeId>(USB_NODE_KEY)
            .await
            .unwrap_or(NodeId::Node1);
        let res = self.pin_controller.select_usb(node);

        let route = self
            .app_db
            .get::<UsbRoute>(USB_ROUTE_KEY)
            .await
            .unwrap_or(UsbRoute::UsbA);
        let res2 = self.pin_controller.set_usb_route(route);

        let mode = self.app_db.get::<u8>(USB_MODE_KEY).await.unwrap_or(0b1111);
        let res3 = self.pin_controller.inner_set_usb_mode(mode);

        res.and(res2).and(res3)
    }

    /// Helper function that returns the new state of ATX power
    fn need_atx_change(current_node_state: u8, next_node_state: u8) -> Option<bool> {
        if current_node_state == 0 && next_node_state > 0 {
            // power down
            Some(true)
        } else if current_node_state > 0 && next_node_state == 0 {
            // power up
            Some(false)
        } else {
            // dont do anything
            None
        }
    }

    pub async fn get_node_power(&self, node: NodeId) -> anyhow::Result<bool> {
        let state = self.power_state.lock().await;
        Ok(*state & node.to_bits() != 0)
    }

    /// This function is used to active a given node. Call this function if a
    /// module is inserted at that slot. Failing to call this method means that
    /// this slot is not considered for power up and power down commands.
    pub async fn activate_slot(&self, node: NodeId, on: bool) -> anyhow::Result<()> {
        ensure!(node.to_bits() != 0);

        let mask = node.to_bits();
        let bits = if on { node.to_bits() } else { !node.to_bits() };
        let mut state = self.app_db.get::<u8>(NODE_ENABLED_KEY).await?;
        state = (state & !mask) | (bits & mask);
        self.app_db.set::<u8>(NODE_ENABLED_KEY, state).await?;
        debug!("node enable bits updated. going to state {:#04b}", state);

        self.power_node(node).await?;
        Ok(())
    }

    pub async fn power_on(&self) -> anyhow::Result<()> {
        self.power_node(0b1111).await
    }

    pub async fn power_off(&self) -> anyhow::Result<()> {
        self.power_node(0).await
    }

    async fn power_node<N: ToBits>(&self, node: N) -> anyhow::Result<()> {
        let mask = self.app_db.get::<u8>(NODE_ENABLED_KEY).await?;
        let mut lock = self.power_state.lock().await;
        let power_state = *lock;

        let mut new_power_state = 0;
        if node.to_bits() != 0 {
            new_power_state = (power_state & !node.to_bits()) | (node.to_bits() & mask);
        }

        if power_state == new_power_state {
            debug!(
                "requested powerstate {:#04b} is already active. enabled={:#04b}",
                power_state, mask
            );
            return Ok(());
        }

        debug!(
            "applying change in power state. current state={:#04b}, new state={:#04b}",
            power_state, new_power_state
        );

        if let Some(on) = Self::need_atx_change(power_state, new_power_state) {
            debug!("changing state of atx to {}", on);
            self.pin_controller.set_atx_power(on).await?;
        }

        self.pin_controller
            .set_power_node(power_state, new_power_state)
            .await
            .context("pin controller error")?;

        *lock = new_power_state;
        Ok(())
    }

    pub async fn usb_mode(&self, mode: UsbMode, node: NodeId) -> anyhow::Result<()> {
        self.pin_controller.select_usb(node)?;
        self.app_db.set(USB_NODE_KEY, node).await?;

        self.pin_controller.set_usb_route(UsbRoute::UsbA)?;
        self.app_db.set(USB_ROUTE_KEY, UsbRoute::UsbA).await?;

        self.set_usb_mode(node, mode).await?;

        // Hack: as in the previous version of the firmware, set RPIBOOT pins of a node when the
        // selected mode is "device", because users execute a command such as `tpi -n 1 -u device`
        // and expect device to be flash-able via rpiboot.
        match mode {
            UsbMode::Host => self.pin_controller.clear_usb_boot()?,
            UsbMode::Device => self.pin_controller.set_usb_boot(node)?,
        }

        Ok(())
    }

    async fn set_usb_mode(&self, node: NodeId, mode: UsbMode) -> anyhow::Result<()> {
        let prev_mode = self.app_db.get::<u8>(USB_MODE_KEY).await.unwrap_or(0b1111);
        let new_mode = self.pin_controller.set_usb_mode(node, mode, prev_mode)?;

        self.app_db.set(USB_MODE_KEY, new_mode).await
    }

    pub async fn rtl_reset(&self) -> anyhow::Result<()> {
        self.pin_controller.rtl_reset().await.context("rtl error")
    }

    pub fn flash_node(
        self: Arc<BmcApplication>,
        node: NodeId,
        image_path: PathBuf,
    ) -> (JoinHandle<anyhow::Result<()>>, Receiver<FlashProgress>) {
        let (sender, receiver) = tokio::sync::mpsc::channel(64);
        let inner = async move {
            // arbitrary number, this sleep may not even be required
            let reboot_delay = Duration::from_millis(500);
            let mut progress_state = FlashProgress {
                message: String::new(),
                status: FlashStatus::Idle,
            };

            progress_state.message = format!("Powering off node {}...", node as u8 + 1);
            progress_state.status = FlashStatus::Progress {
                read_percent: 0,
                est_minutes: u64::MAX,
                est_seconds: u64::MAX,
            };
            sender.send(progress_state.clone()).await?;

            self.activate_slot(node, false).await?;
            self.pin_controller.clear_usb_boot()?;

            sleep(reboot_delay).await;

            self.pin_controller.select_usb(node)?;
            self.pin_controller.set_usb_boot(node)?;
            self.pin_controller.set_usb_route(UsbRoute::BMC)?;

            self.set_usb_mode(node, UsbMode::Device).await?;

            progress_state.message = String::from("Prerequisite settings toggled, powering on...");
            sender.send(progress_state.clone()).await?;

            self.activate_slot(node, true).await?;

            sleep(Duration::from_secs(2)).await;

            progress_state.message = String::from("Checking for presence of a USB device...");
            sender.send(progress_state.clone()).await?;

            let allowed_devices = [
                (0x0a5c, 0x2711), // Raspberry Pi Compute module 4
            ];
            usbboot::check_only_one_device_present(&allowed_devices)?;

            progress_state.message = String::from("Rebooting as a USB mass storage device...");
            sender.send(progress_state.clone()).await?;

            usbboot::boot_node_to_msd(node)?;

            sleep(Duration::from_secs(3)).await;

            progress_state.message = String::from("Checking for presence of a device file...");
            sender.send(progress_state.clone()).await?;

            let allowed_vendors = ["RPi-MSD-"];
            let device_path = usbboot::get_device_path(&allowed_vendors).await?;

            progress_state.message = format!("Writing {:?} to {:?}", image_path, device_path);
            sender.send(progress_state.clone()).await?;

            let (img_len, img_checksum) =
                usbboot::write_to_device(image_path, &device_path, &sender).await?;

            progress_state.message = String::from("Verifying checksum...");
            sender.send(progress_state.clone()).await?;

            usbboot::verify_checksum(img_checksum, img_len, &device_path, &sender).await?;

            progress_state.message = String::from("Flashing successful, restarting device...");
            sender.send(progress_state.clone()).await?;

            self.activate_slot(node, false).await?;
            self.usb_mode(UsbMode::Host, node).await?;

            sleep(reboot_delay).await;

            self.activate_slot(node, true).await?;

            progress_state.message = String::from("Done");
            sender.send(progress_state).await?;

            Ok(())
        };

        (tokio::spawn(inner), receiver)
    }
}

fn reboot() -> anyhow::Result<()> {
    Command::new("shutdown").args(["-r", "now"]).spawn()?;

    Ok(())
}

#[derive(Debug, Clone, Copy)]
pub enum FlashStatus {
    Idle,
    Progress {
        read_percent: u64,
        est_minutes: u64,
        est_seconds: u64,
    },
    Error(FlashingError),
    Done,
}

#[derive(Debug, Clone)]
pub struct FlashProgress {
    pub status: FlashStatus,
    pub message: String,
}
