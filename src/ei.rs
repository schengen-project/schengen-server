//! EI (Emulated Input) protocol module
//!
//! This module handles the libei protocol connection for receiving input events
//! through the InputCapture portal and managing devices and regions.

use anyhow::{Context, Result};
use kbvm::Components;
use kbvm::lookup::LookupTable;
use kbvm::state_machine::{Direction, State, StateMachine};
use kbvm::xkb::diagnostic::{Severity, WriteToLog};
use log::{debug, info, warn};
use reis::{PendingRequestResult, ei, handshake::ei_handshake_blocking};
use std::collections::HashMap;
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd, RawFd};
use tokio::io::unix::AsyncFd;

/// Synergy modifier mask bits
#[repr(u16)]
#[derive(Debug, Clone, Copy)]
#[allow(dead_code)]
enum SynergyModifier {
    Shift = 0x0001,
    Control = 0x0002,
    Alt = 0x0004,
    Meta = 0x0008,
    Super = 0x0010,
    AltGr = 0x0020,
    Level5Lock = 0x0040,
    CapsLock = 0x1000,
    NumLock = 0x2000,
    ScrollLock = 0x4000,
}

// X11/XKB modifier bit positions
const XKB_SHIFT: u32 = 0x01;
const XKB_LOCK: u32 = 0x02;
const XKB_CONTROL: u32 = 0x04;
const XKB_MOD1: u32 = 0x08; // Alt
const XKB_MOD2: u32 = 0x10; // Num Lock
#[allow(dead_code)]
const XKB_MOD3: u32 = 0x20;
const XKB_MOD4: u32 = 0x40; // Super/Windows
const XKB_MOD5: u32 = 0x80; // AltGr (typically)

/// Input event from EI
#[derive(Debug, Clone)]
pub enum InputEvent {
    /// Pointer absolute motion event
    PointerAbsolute { x: f64, y: f64 },
    /// Pointer relative motion event
    PointerRelative { dx: f64, dy: f64 },
    /// Button press/release event
    Button { button: u32, is_press: bool },
    /// Keyboard key press/release event
    /// - keysym: X keysym (converted from Linux key code via XKB)
    /// - is_press: true for press, false for release
    /// - mask: modifier bitmask
    /// - button: physical key code (Linux evdev key code)
    Key {
        keysym: u32,
        is_press: bool,
        mask: u16,
        button: u16,
    },
    /// Scroll event
    Scroll { x: f64, y: f64 },
}

/// Region information for a device
#[derive(Debug, Clone)]
pub struct Region {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
    pub scale: f32,
}

/// Information about a device being configured
#[derive(Debug, Clone)]
struct PendingDeviceInfo {
    name: Option<String>,
    device_type: Option<ei::device::DeviceType>,
    regions: Vec<Region>,
    interfaces: Vec<String>,
}

impl PendingDeviceInfo {
    fn new() -> Self {
        Self {
            name: None,
            device_type: None,
            regions: Vec::new(),
            interfaces: Vec::new(),
        }
    }
}

/// Information about a configured device
#[derive(Debug, Clone)]
pub struct DeviceInfo {
    pub name: String,
    pub device_type: ei::device::DeviceType,
    pub regions: Vec<Region>,
    pub interfaces: Vec<String>,
}

/// EI context wrapper for managing libei devices
pub struct EiContext {
    context: ei::Context,
    async_fd: AsyncFd<OwnedFd>,
    connection: Option<ei::Connection>,
    seats: HashMap<String, ei::Seat>,
    capabilities: HashMap<String, u64>,
    pending_devices: HashMap<ei::Device, PendingDeviceInfo>,
    pub devices: Vec<DeviceInfo>,
    /// Queue of input events
    input_events: Vec<InputEvent>,
    /// XKB lookup table for converting key codes to keysyms
    lookup_table: Option<LookupTable>,
    /// State machine for tracking modifier state
    state_machine: Option<StateMachine>,
    /// State for the state machine
    machine_state: Option<State>,
    /// Components to track active modifiers and group
    components: Components,
}

impl EiContext {
    /// Process EI events
    pub async fn recv_event(&mut self) -> Result<()> {
        // Wait for the file descriptor to be readable before processing
        self.async_fd.readable().await?.clear_ready();
        self.process_events().await
    }

    /// Take all pending input events
    pub fn take_input_events(&mut self) -> Vec<InputEvent> {
        std::mem::take(&mut self.input_events)
    }

    /// Process EI events
    async fn process_events(&mut self) -> Result<()> {
        // Read from socket
        self.context
            .read()
            .context("Failed to read from EI socket")?;

        // Process all pending events
        while let Some(pending) = self.context.pending_event() {
            let event = match pending {
                PendingRequestResult::Request(event) => event,
                PendingRequestResult::ParseError(e) => {
                    return Err(anyhow::anyhow!("Failed to parse EI event: {:?}", e));
                }
                PendingRequestResult::InvalidObject(id) => {
                    return Err(anyhow::anyhow!("Invalid object ID: {}", id));
                }
            };

            self.handle_event(event)?;
        }

        Ok(())
    }

    /// Handle a single EI event
    fn handle_event(&mut self, event: ei::Event) -> Result<()> {
        match event {
            ei::Event::Connection(_connection, conn_event) => {
                self.handle_connection_event(conn_event)?;
            }
            ei::Event::Seat(seat, seat_event) => {
                self.handle_seat_event(seat, seat_event)?;
            }
            ei::Event::Device(device, device_event) => {
                self.handle_device_event(device, device_event)?;
            }
            ei::Event::Pingpong(pingpong, _event) => {
                debug!("Received pingpong, responding with done");
                pingpong.done(0);
                self.context.flush()?;
            }
            ei::Event::PointerAbsolute(_pointer_abs, pointer_event) => {
                self.handle_pointer_absolute_event(pointer_event)?;
            }
            ei::Event::Pointer(_pointer, pointer_event) => {
                self.handle_pointer_event(pointer_event)?;
            }
            ei::Event::Keyboard(_keyboard, keyboard_event) => {
                self.handle_keyboard_event(keyboard_event)?;
            }
            ei::Event::Button(_button, button_event) => {
                self.handle_button_event(button_event)?;
            }
            ei::Event::Scroll(_scroll, scroll_event) => {
                self.handle_scroll_event(scroll_event)?;
            }
            _ => {
                debug!("Received other EI event");
            }
        }

        Ok(())
    }

    /// Handle pointer absolute events
    fn handle_pointer_absolute_event(&mut self, event: ei::pointer_absolute::Event) -> Result<()> {
        match event {
            ei::pointer_absolute::Event::MotionAbsolute { x, y } => {
                debug!("Pointer absolute motion: ({:.2}, {:.2})", x, y);
                self.input_events.push(InputEvent::PointerAbsolute {
                    x: x as f64,
                    y: y as f64,
                });
            }
            _ => {
                debug!("Other pointer absolute event: {:?}", event);
            }
        }
        Ok(())
    }

    /// Handle pointer (relative) events
    fn handle_pointer_event(&mut self, event: ei::pointer::Event) -> Result<()> {
        match event {
            ei::pointer::Event::MotionRelative { x, y } => {
                // Don't log every motion event - too spammy
                self.input_events.push(InputEvent::PointerRelative {
                    dx: x as f64,
                    dy: y as f64,
                });
            }
            _ => {
                debug!("Other pointer relative event: {:?}", event);
            }
        }
        Ok(())
    }

    /// Convert XKB modifier mask to Synergy modifier mask
    fn xkb_to_synergy_modifiers(xkb_mask: u32) -> u16 {
        let mut synergy_mask: u16 = 0;

        if xkb_mask & XKB_SHIFT != 0 {
            synergy_mask |= SynergyModifier::Shift as u16;
        }
        if xkb_mask & XKB_CONTROL != 0 {
            synergy_mask |= SynergyModifier::Control as u16;
        }
        if xkb_mask & XKB_MOD1 != 0 {
            synergy_mask |= SynergyModifier::Alt as u16;
        }
        if xkb_mask & XKB_MOD4 != 0 {
            synergy_mask |= SynergyModifier::Super as u16;
        }
        if xkb_mask & XKB_MOD5 != 0 {
            synergy_mask |= SynergyModifier::AltGr as u16;
        }
        if xkb_mask & XKB_LOCK != 0 {
            synergy_mask |= SynergyModifier::CapsLock as u16;
        }
        if xkb_mask & XKB_MOD2 != 0 {
            synergy_mask |= SynergyModifier::NumLock as u16;
        }
        // Note: Scroll Lock and Level5Lock are not commonly used in XKB,
        // may need additional mapping if needed

        synergy_mask
    }

    /// Convert X11 keysym to Deskflow KeyID
    ///
    /// Deskflow uses the 0xEFxx range for special keys where X11 uses 0xFFxx.
    /// Regular character keys (ASCII, Unicode) are passed through unchanged.
    fn xk_to_deskflow_key(xk_keysym: u32) -> u32 {
        // X11 special keys are in the 0xFF00-0xFFFF range
        // Deskflow maps these to 0xEF00-0xEFFF range
        if (0xFF00..=0xFFFF).contains(&xk_keysym) {
            // Replace 0xFF with 0xEF
            0xEF00 | (xk_keysym & 0x00FF)
        } else {
            // Regular characters (ASCII, Unicode, etc.) pass through unchanged
            xk_keysym
        }
    }

    /// Handle keyboard events
    fn handle_keyboard_event(&mut self, event: ei::keyboard::Event) -> Result<()> {
        match event {
            ei::keyboard::Event::Key { key, state } => {
                debug!("Keyboard key: {} state: {:?}", key, state);
                let is_press = matches!(state, ei::keyboard::KeyState::Press);
                let keycode = kbvm::Keycode::from_evdev(key);

                // Convert Linux key code to X keysym using the lookup table with CURRENT modifiers
                // (before updating the state machine with this key event)
                let keysym = if let Some(ref lookup_table) = self.lookup_table {
                    // Use current effective modifiers and group from components
                    let lookup =
                        lookup_table.lookup(self.components.group, self.components.mods, keycode);

                    // Get the first keysym from the lookup result
                    let keysyms: Vec<_> = lookup.into_iter().map(|p| p.keysym()).collect();
                    if let Some(&first_keysym) = keysyms.first() {
                        debug!(
                            "Converted key code {} to keysym 0x{:x} (with mods 0x{:x})",
                            key, first_keysym.0, self.components.mods.0
                        );
                        first_keysym.0
                    } else {
                        warn!("No keysym found for key code {}, using raw code", key);
                        key
                    }
                } else {
                    warn!("No lookup table available, using raw key code {}", key);
                    key
                };

                // Store the physical button code (Linux evdev key code + X keycode offset)
                // X11 keycodes are offset by 8 from Linux evdev keycodes
                let button_code = (keycode.raw() & 0xFFFF) as u16;

                // Update the state machine with this key event if available
                // This must happen AFTER the keysym lookup so that modifier keys are looked up
                // without their own modifier being applied
                if let (Some(state_machine), Some(machine_state)) =
                    (&self.state_machine, &mut self.machine_state)
                {
                    let mut events = Vec::new();
                    let direction = if is_press {
                        Direction::Down
                    } else {
                        Direction::Up
                    };

                    state_machine.handle_key(machine_state, &mut events, keycode, direction);

                    // Apply events to update components
                    for evt in events {
                        self.components.apply_event(evt);
                    }

                    // Log the modifier state after update
                    debug!(
                        "Modifier state after key {}: mods=0x{:x}, group={}",
                        key, self.components.mods.0, self.components.group.0
                    );
                }

                // Capture modifier state AFTER updating the state machine
                // For key DOWN: this includes the modifier if this key is a modifier
                // For key UP: this excludes the modifier since it was just released
                // Convert XKB modifier mask to Synergy modifier mask
                let xkb_mask = self.components.mods.0;
                let modifier_mask = Self::xkb_to_synergy_modifiers(xkb_mask);
                debug!(
                    "XKB mask 0x{:x} -> Synergy mask 0x{:x}",
                    xkb_mask, modifier_mask
                );

                // Convert X11 keysym to Deskflow KeyID
                let deskflow_keyid = Self::xk_to_deskflow_key(keysym);
                debug!(
                    "Converted X11 keysym 0x{:x} to Deskflow KeyID 0x{:x}",
                    keysym, deskflow_keyid
                );

                self.input_events.push(InputEvent::Key {
                    keysym: deskflow_keyid,
                    is_press,
                    mask: modifier_mask,
                    button: button_code,
                });
            }
            ei::keyboard::Event::Keymap {
                keymap_type,
                size,
                keymap,
            } => {
                info!("Received keymap: type={:?}, size={}", keymap_type, size);

                // Only handle XKB keymaps
                if matches!(keymap_type, ei::keyboard::KeymapType::Xkb) {
                    // Read the keymap data from the file descriptor
                    match Self::read_keymap_from_fd(keymap.as_raw_fd(), size as usize) {
                        Ok(keymap_bytes) => {
                            // Create an XKB context and parse the keymap
                            let context = kbvm::xkb::Context::default();
                            match context.keymap_from_bytes(
                                (WriteToLog, Severity::Error),
                                None,
                                &keymap_bytes,
                            ) {
                                Ok(xkb_keymap) => {
                                    // Build both a lookup table and state machine from the keymap
                                    let builder = xkb_keymap.to_builder();
                                    let lookup_table = builder.clone().build_lookup_table();
                                    let state_machine = builder.build_state_machine();
                                    let machine_state = state_machine.create_state();
                                    info!(
                                        "Successfully loaded XKB keymap and built lookup table and state machine"
                                    );
                                    self.lookup_table = Some(lookup_table);
                                    self.state_machine = Some(state_machine);
                                    self.machine_state = Some(machine_state);
                                }
                                Err(e) => {
                                    warn!("Failed to parse XKB keymap: {}", e);
                                }
                            }
                        }
                        Err(e) => {
                            warn!("Failed to read keymap from fd: {}", e);
                        }
                    }
                } else {
                    warn!("Unsupported keymap type: {:?}", keymap_type);
                }
            }
            _ => {
                debug!("Other keyboard event: {:?}", event);
            }
        }
        Ok(())
    }

    /// Read keymap data from a file descriptor
    fn read_keymap_from_fd(fd: RawFd, size: usize) -> Result<Vec<u8>> {
        use std::io::Read;

        // Duplicate the fd so we don't consume the original
        let dup_fd = unsafe {
            let raw = libc::dup(fd);
            if raw < 0 {
                return Err(anyhow::anyhow!("Failed to duplicate fd"));
            }
            std::fs::File::from_raw_fd(raw)
        };

        // Read the keymap data
        let mut reader = std::io::BufReader::new(dup_fd);
        let mut buffer = Vec::with_capacity(size);
        reader
            .read_to_end(&mut buffer)
            .context("Failed to read keymap data")?;

        Ok(buffer)
    }

    /// Map evdev button code to Synergy button number
    ///
    /// evdev button codes (from linux/input-event-codes.h):
    /// - BTN_LEFT = 0x110 (272) -> Synergy button 1
    /// - BTN_RIGHT = 0x111 (273) -> Synergy button 3
    /// - BTN_MIDDLE = 0x112 (274) -> Synergy button 2
    /// - BTN_SIDE = 0x113 (275) -> Synergy button 4
    /// - BTN_EXTRA = 0x114 (276) -> Synergy button 5
    fn evdev_to_synergy_button(evdev_button: u32) -> u32 {
        match evdev_button {
            0x110 => 1, // BTN_LEFT -> Button 1 (left)
            0x111 => 3, // BTN_RIGHT -> Button 3 (right)
            0x112 => 2, // BTN_MIDDLE -> Button 2 (middle)
            0x113 => 4, // BTN_SIDE -> Button 4 (back)
            0x114 => 5, // BTN_EXTRA -> Button 5 (forward)
            0x115 => 6, // BTN_FORWARD -> Button 6
            0x116 => 7, // BTN_BACK -> Button 7
            0x117 => 8, // BTN_TASK -> Button 8
            // For unknown buttons, try to preserve some mapping
            // If it's in the BTN range (0x100-0x11f), map relative to BTN_LEFT
            b if (0x100..0x120).contains(&b) => {
                // Offset from BTN_MISC (0x100)
                (b - 0x100 + 1).min(32)
            }
            // Otherwise pass through as-is
            b => b,
        }
    }

    /// Handle button events
    fn handle_button_event(&mut self, event: ei::button::Event) -> Result<()> {
        match event {
            ei::button::Event::Button { button, state } => {
                let synergy_button = Self::evdev_to_synergy_button(button);
                debug!(
                    "Button: {} (evdev: {}) state: {:?}",
                    synergy_button, button, state
                );
                let is_press = matches!(state, ei::button::ButtonState::Press);
                self.input_events.push(InputEvent::Button {
                    button: synergy_button,
                    is_press,
                });
            }
            _ => {
                debug!("Other button event: {:?}", event);
            }
        }
        Ok(())
    }

    /// Handle scroll events
    fn handle_scroll_event(&mut self, event: ei::scroll::Event) -> Result<()> {
        match event {
            ei::scroll::Event::Scroll { x, y } => {
                debug!("Scroll: ({:.2}, {:.2})", x, y);
                self.input_events.push(InputEvent::Scroll {
                    x: x as f64,
                    y: y as f64,
                });
            }
            _ => {
                debug!("Other scroll event: {:?}", event);
            }
        }
        Ok(())
    }

    /// Handle connection events
    fn handle_connection_event(&mut self, event: ei::connection::Event) -> Result<()> {
        match event {
            ei::connection::Event::Seat { seat: _ } => {
                debug!("EI Connection: Received seat from connection");
            }
            ei::connection::Event::Ping { ping } => {
                debug!("Received ping, responding with done");
                ping.done(0);
                self.context.flush()?;
            }
            ei::connection::Event::Disconnected { .. } => {
                warn!("EI connection disconnected");
            }
            _ => {
                debug!("Other connection event");
            }
        }
        Ok(())
    }

    /// Handle seat events
    fn handle_seat_event(&mut self, seat: ei::Seat, event: ei::seat::Event) -> Result<()> {
        match event {
            ei::seat::Event::Name { name } => {
                info!("EI Seat: '{}'", name);
            }
            ei::seat::Event::Capability { mask, interface } => {
                info!("  Seat capability: {} (mask=0x{:x})", interface, mask);
                self.capabilities.insert(interface, mask);
            }
            ei::seat::Event::Done => {
                info!("Seat configuration complete, binding to seat");
                self.bind_to_seat(seat)?;
            }
            ei::seat::Event::Device { device } => {
                info!("  Received new device from seat");
                self.pending_devices
                    .insert(device.clone(), PendingDeviceInfo::new());
            }
            ei::seat::Event::Destroyed { serial } => {
                info!("Seat destroyed (serial={})", serial);
            }
            _ => {
                debug!("Other seat event");
            }
        }
        Ok(())
    }

    /// Bind to a seat with keyboard and pointer capabilities
    fn bind_to_seat(&mut self, seat: ei::Seat) -> Result<()> {
        info!("Binding to seat with keyboard and pointer capabilities");

        // Calculate the capability mask for keyboard and pointer
        let mut capabilities_mask = 0u64;

        // For a receiver, we want all available capabilities
        if let Some(&mask) = self.capabilities.get("ei_keyboard") {
            capabilities_mask |= mask;
            debug!("Added keyboard capability: 0x{:x}", mask);
        }

        if let Some(&mask) = self.capabilities.get("ei_pointer") {
            capabilities_mask |= mask;
            debug!("Added pointer capability: 0x{:x}", mask);
        }

        if let Some(&mask) = self.capabilities.get("ei_pointer_absolute") {
            capabilities_mask |= mask;
            debug!("Added pointer_absolute capability: 0x{:x}", mask);
        }

        if let Some(&mask) = self.capabilities.get("ei_button") {
            capabilities_mask |= mask;
            debug!("Added button capability: 0x{:x}", mask);
        }

        if let Some(&mask) = self.capabilities.get("ei_scroll") {
            capabilities_mask |= mask;
            debug!("Added scroll capability: 0x{:x}", mask);
        }

        if capabilities_mask == 0 {
            warn!("No suitable capabilities available on seat");
            return Ok(());
        }

        info!(
            "Binding to seat with capability mask: 0x{:x}",
            capabilities_mask
        );

        // Bind to the seat with the combined capabilities
        seat.bind(capabilities_mask);

        // Flush to send the bind request
        self.context.flush()?;

        // Store the seat for later use
        self.seats.insert("default".to_string(), seat);

        info!("Successfully bound to seat");

        Ok(())
    }

    /// Handle device events
    fn handle_device_event(&mut self, device: ei::Device, event: ei::device::Event) -> Result<()> {
        match event {
            ei::device::Event::Name { name } => {
                debug!("Device name: '{}'", name);
                if let Some(pending) = self.pending_devices.get_mut(&device) {
                    pending.name = Some(name);
                }
            }
            ei::device::Event::DeviceType { device_type } => {
                debug!("Device type: {:?}", device_type);
                if let Some(pending) = self.pending_devices.get_mut(&device) {
                    pending.device_type = Some(device_type);
                }
            }
            ei::device::Event::Dimensions { width, height } => {
                debug!("Device dimensions: {}x{}", width, height);
                if let Some(pending) = self.pending_devices.get_mut(&device) {
                    // If we get dimensions without explicit regions, create a default region
                    if pending.regions.is_empty() {
                        pending.regions.push(Region {
                            x: 0,
                            y: 0,
                            width,
                            height,
                            scale: 1.0,
                        });
                    }
                }
            }
            ei::device::Event::Region {
                offset_x,
                offset_y,
                width,
                hight, // Note: typo in the protocol binding
                scale,
            } => {
                debug!(
                    "Device region: offset=({}, {}), size={}x{}, scale={:.2}",
                    offset_x, offset_y, width, hight, scale
                );
                if let Some(pending) = self.pending_devices.get_mut(&device) {
                    pending.regions.push(Region {
                        x: offset_x,
                        y: offset_y,
                        width,
                        height: hight,
                        scale,
                    });
                }
            }
            ei::device::Event::Interface { object } => {
                // Get the interface name from the object
                let interface_name = object.interface().to_string();
                debug!("Device interface: '{}'", interface_name);
                if let Some(pending) = self.pending_devices.get_mut(&device) {
                    pending.interfaces.push(interface_name);
                }
            }
            ei::device::Event::Done => {
                debug!(
                    "✓ Device configuration complete (pending_devices count: {})",
                    self.pending_devices.len()
                );

                // Move the device from pending to active
                if let Some(pending) = self.pending_devices.remove(&device) {
                    debug!(
                        "Found pending device, name: {:?}, type: {:?}",
                        pending.name, pending.device_type
                    );
                    if let (Some(name), Some(device_type)) = (pending.name, pending.device_type) {
                        let device_info = DeviceInfo {
                            name: name.clone(),
                            device_type,
                            regions: pending.regions.clone(),
                            interfaces: pending.interfaces.clone(),
                        };

                        info!(
                            "✓ Device '{}' (type: {:?}) ready with {} region(s) and {} interface(s)",
                            name,
                            device_type,
                            device_info.regions.len(),
                            device_info.interfaces.len()
                        );

                        // Log region details
                        for (idx, region) in device_info.regions.iter().enumerate() {
                            info!(
                                "  Region {}: offset=({}, {}), size={}x{}, scale={:.2}",
                                idx, region.x, region.y, region.width, region.height, region.scale
                            );
                        }

                        // Log interface details
                        for interface in &device_info.interfaces {
                            info!("  Interface: '{}'", interface);
                        }

                        self.devices.push(device_info);
                    } else {
                        warn!("Device configuration complete but missing name or device_type!");
                    }
                } else {
                    warn!("Device configuration complete but device not found in pending_devices!");
                }
            }
            ei::device::Event::Resumed { serial: _ } => {
                debug!("Device resumed");
            }
            ei::device::Event::Paused { serial: _ } => {
                debug!("Device paused");
            }
            ei::device::Event::Frame { .. } => {
                // Ignore frame events silently
            }
            _ => {
                debug!("Other device event");
            }
        }
        Ok(())
    }
}

/// Connect to EI using the provided file descriptor and wait for devices
///
/// This function creates an EI context from the file descriptor and processes
/// initial events to discover and register EI devices.
///
/// # Arguments
///
/// * `fd` - The file descriptor for the EI connection from the portal
///
/// # Returns
///
/// Returns an `EiContext` with discovered devices
///
/// # Errors
///
/// Returns an error if the EI connection fails or device discovery times out
pub async fn connect_with_fd(fd: RawFd) -> Result<EiContext> {
    debug!("Creating EI context from fd {}", fd);

    // Create EI context from the file descriptor
    // SAFETY: The fd is owned by the Session and valid for the program duration
    let owned_fd = unsafe { OwnedFd::from_raw_fd(fd) };

    // Duplicate the fd for AsyncFd (we need two fds: one for reis, one for tokio)
    let dup_fd = unsafe {
        let raw = libc::dup(owned_fd.as_fd().as_raw_fd());
        if raw < 0 {
            return Err(anyhow::anyhow!("Failed to duplicate fd"));
        }
        OwnedFd::from_raw_fd(raw)
    };

    // Convert OwnedFd to UnixStream for EI context
    let unix_stream = std::os::unix::net::UnixStream::from(owned_fd);
    let ei_context = ei::Context::new(unix_stream).context("Failed to create EI context")?;

    // Create AsyncFd for async waiting
    let async_fd = AsyncFd::new(dup_fd).context("Failed to create AsyncFd")?;

    let mut context = EiContext {
        context: ei_context,
        async_fd,
        connection: None,
        seats: HashMap::new(),
        capabilities: HashMap::new(),
        pending_devices: HashMap::new(),
        devices: Vec::new(),
        input_events: Vec::new(),
        lookup_table: None,
        state_machine: None,
        machine_state: None,
        components: Components::default(),
    };

    // Perform EI handshake
    info!("Starting EI handshake");

    let handshake_resp = ei_handshake_blocking(
        &context.context,
        "schengen-server",
        ei::handshake::ContextType::Receiver,
    )
    .context("EI handshake failed")?;

    info!("EI handshake completed successfully");
    context.connection = Some(handshake_resp.connection);

    // Process initial events to discover devices
    // We only need keyboard and pointer devices to continue
    info!("Discovering essential EI devices (keyboard and pointer capabilities)...");
    let max_wait = std::time::Duration::from_secs(5);
    let start = std::time::Instant::now();

    while start.elapsed() < max_wait {
        // Process events
        context.process_events().await?;

        // Check if we have the essential devices by their capabilities
        let has_keyboard = context.devices.iter().any(|d| {
            matches!(d.device_type, ei::device::DeviceType::Virtual)
                && d.interfaces.iter().any(|i| i == "ei_keyboard")
        });

        let has_pointer = context.devices.iter().any(|d| {
            matches!(d.device_type, ei::device::DeviceType::Virtual)
                && (d.interfaces.iter().any(|i| i == "ei_pointer")
                    || d.interfaces.iter().any(|i| i == "ei_pointer_absolute"))
        });

        if has_keyboard && has_pointer {
            info!("✓ Essential device capabilities found (keyboard and pointer)");
            break;
        }

        // Brief sleep to avoid busy waiting
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
    }

    if context.devices.is_empty() {
        warn!("No EI devices discovered after waiting");
    } else {
        info!("Discovered {} EI device(s)", context.devices.len());
        for device in &context.devices {
            info!("  - '{}' (type: {:?})", device.name, device.device_type);
        }
    }

    Ok(context)
}
