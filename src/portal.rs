//! InputCapture portal connection module
//!
//! This module handles the connection to the InputCapture portal using ashpd,
//! requesting mouse and keyboard capture permissions.

use anyhow::{Context, Result};
use ashpd::desktop::input_capture::{
    Barrier, ConnectToEISOptions, CreateSessionOptions, DisableOptions, EnableOptions,
    GetZonesOptions, InputCapture, ReleaseOptions, SetPointerBarriersOptions,
};
use log::{debug, info, warn};
use std::collections::HashMap;
use std::num::NonZeroU32;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::config::{ClientConfig, Position};
use crate::ei;

/// A 2D point with floating-point coordinates
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Point {
    pub x: f64,
    pub y: f64,
}

impl Point {
    /// Create a new Point
    pub fn new(x: f64, y: f64) -> Self {
        Self { x, y }
    }
}

/// A 2D size with floating-point dimensions
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Size {
    pub width: f64,
    pub height: f64,
}

impl Size {
    /// Create a new Size
    pub fn new(width: f64, height: f64) -> Self {
        Self { width, height }
    }
}

/// Desktop bounds from portal zones
#[derive(Debug, Clone)]
pub struct DesktopBounds {
    pub min_x: i32,
    pub min_y: i32,
    pub max_x: i32,
    pub max_y: i32,
}

/// Portal session wrapper holding the InputCapture session
pub struct Session {
    _owned_fd: OwnedFd,
    /// Receiver for events from the portal
    pub event_rx: tokio::sync::mpsc::UnboundedReceiver<crate::server::Event>,
    /// Sender to send events to the portal
    pub event_tx: tokio::sync::mpsc::UnboundedSender<crate::server::Event>,
}

impl Session {
    /// Split the session into its components
    ///
    /// Returns (OwnedFd, event receiver, event sender)
    pub fn split(
        self,
    ) -> (
        OwnedFd,
        tokio::sync::mpsc::UnboundedReceiver<crate::server::Event>,
        tokio::sync::mpsc::UnboundedSender<crate::server::Event>,
    ) {
        (self._owned_fd, self.event_rx, self.event_tx)
    }
}

/// Set up pointer barriers based on zones from InputCapture portal
///
/// This queries the zones from the InputCapture portal and creates barriers
/// at the edges of each zone based on the configured client positions.
///
/// # Arguments
///
/// * `proxy` - The InputCapture proxy
/// * `session` - The portal session handle
/// * `client_configs` - Map of client names to their position configurations
///
/// # Returns
///
/// Returns a tuple of (barrier map, desktop bounds)
///
/// # Errors
///
/// Returns an error if querying zones or setting barriers fails
async fn setup_barriers(
    proxy: &InputCapture,
    session: &ashpd::desktop::Session<InputCapture>,
    client_configs: &HashMap<String, ClientConfig>,
) -> Result<(HashMap<u32, (String, Position)>, DesktopBounds)> {
    // Query zones from the InputCapture portal
    info!("Querying zones from InputCapture portal...");
    let zones = proxy
        .zones(session, GetZonesOptions::default())
        .await
        .context("Failed to request zones")?
        .response()
        .context("Failed to get zones response")?;

    info!(
        "Received {} zone(s) with zone_set ID: {}",
        zones.regions().len(),
        zones.zone_set()
    );

    // Calculate the bounding box of all zones (the entire desktop)
    let mut min_x = i32::MAX;
    let mut min_y = i32::MAX;
    let mut max_x = i32::MIN;
    let mut max_y = i32::MIN;

    for (zone_idx, zone) in zones.regions().iter().enumerate() {
        let x = zone.x_offset();
        let y = zone.y_offset();
        let width = zone.width();
        let height = zone.height();

        info!(
            "Zone {}: offset=({}, {}), size={}x{}",
            zone_idx, x, y, width, height
        );

        min_x = min_x.min(x);
        min_y = min_y.min(y);
        max_x = max_x.max(x + width as i32);
        max_y = max_y.max(y + height as i32);
    }

    info!(
        "Desktop bounding box: ({}, {}) to ({}, {}) [{}x{}]",
        min_x,
        min_y,
        max_x,
        max_y,
        max_x - min_x,
        max_y - min_y
    );

    let mut barriers = Vec::new();
    let mut barrier_id = 1u32;
    let mut barrier_map: HashMap<u32, (String, Position)> = HashMap::new();

    // Create barriers only on the outer edges of the entire desktop
    for (client_name, config) in client_configs {
        // Only create barriers for clients positioned relative to the server (self)
        if config.reference != "self" {
            continue;
        }

        let barrier_id_nonzero = NonZeroU32::new(barrier_id).unwrap();
        let barrier = match config.position {
            Position::LeftOf => {
                // Client is to the left, so barrier on left edge of desktop
                info!(
                    "  Adding left barrier (ID {}) for client '{}'",
                    barrier_id, client_name
                );
                Barrier::new(barrier_id_nonzero, (min_x, min_y, min_x, max_y))
            }
            Position::RightOf => {
                // Client is to the right, so barrier on right edge of desktop
                info!(
                    "  Adding right barrier (ID {}) for client '{}'",
                    barrier_id, client_name
                );
                Barrier::new(barrier_id_nonzero, (max_x, min_y, max_x, max_y))
            }
            Position::TopOf => {
                // Client is above, so barrier on top edge of desktop
                info!(
                    "  Adding top barrier (ID {}) for client '{}'",
                    barrier_id, client_name
                );
                Barrier::new(barrier_id_nonzero, (min_x, min_y, max_x, min_y))
            }
            Position::BottomOf => {
                // Client is below, so barrier on bottom edge of desktop
                info!(
                    "  Adding bottom barrier (ID {}) for client '{}'",
                    barrier_id, client_name
                );
                Barrier::new(barrier_id_nonzero, (min_x, max_y, max_x, max_y))
            }
        };

        barriers.push(barrier);
        barrier_map.insert(barrier_id, (client_name.clone(), config.position));
        barrier_id += 1;
    }

    info!(
        "Setting {} pointer barrier(s) with zone_set {}",
        barriers.len(),
        zones.zone_set()
    );
    proxy
        .set_pointer_barriers(
            session,
            &barriers,
            zones.zone_set(),
            SetPointerBarriersOptions::default(),
        )
        .await
        .context("Failed to set pointer barriers")?
        .response()
        .context("Failed to get set_pointer_barriers response")?;

    info!("✓ Pointer barriers configured");

    let desktop_bounds = DesktopBounds {
        min_x,
        min_y,
        max_x,
        max_y,
    };

    Ok((barrier_map, desktop_bounds))
}

/// Connect to the InputCapture portal with mouse and keyboard permissions
///
/// This function uses ashpd to establish a connection to the InputCapture portal,
/// requesting permission to capture mouse and keyboard events. It creates a session,
/// obtains a file descriptor for the EI connection, sets up the EI context,
/// and configures pointer barriers based on client configurations.
///
/// # Arguments
///
/// * `client_configs` - Map of client names to their position configurations
///
/// # Returns
///
/// Returns a tuple of (portal Session, barrier map, desktop bounds)
///
/// # Errors
///
/// Returns an error if the portal connection fails or permissions are denied
pub async fn connect_input_capture(
    client_configs: &HashMap<String, ClientConfig>,
) -> Result<(
    Session,
    Arc<RwLock<HashMap<u32, (String, Position)>>>,
    Arc<RwLock<DesktopBounds>>,
)> {
    info!("Connecting to InputCapture portal");

    let proxy = InputCapture::new()
        .await
        .context("Failed to create InputCapture proxy")?;

    // Get capabilities - we want both keyboard and pointer
    let capabilities = ashpd::desktop::input_capture::Capabilities::Keyboard
        | ashpd::desktop::input_capture::Capabilities::Pointer;

    // Create the barrier map and desktop bounds that will be shared
    let barrier_map = Arc::new(RwLock::new(HashMap::new()));
    let barrier_map_clone = Arc::clone(&barrier_map);
    let desktop_bounds = Arc::new(RwLock::new(DesktopBounds {
        min_x: 0,
        min_y: 0,
        max_x: 1920, // Default, will be updated after setup_barriers
        max_y: 1080,
    }));
    let desktop_bounds_clone = Arc::clone(&desktop_bounds);
    let client_configs_arc = Arc::new(client_configs.clone());

    // We need to create the session, get the EI fd, and then spawn the monitoring task
    // The monitoring task will own proxy and session for their lifetimes
    // Create a oneshot channel to get the fd back from the spawned task
    let (fd_tx, fd_rx) = tokio::sync::oneshot::channel();

    // Create event channels for bidirectional communication
    // portal_to_server: portal task sends events to server
    // server_to_portal: server sends events to portal task
    let (portal_to_server_tx, portal_to_server_rx) = tokio::sync::mpsc::unbounded_channel();
    let (server_to_portal_tx, mut server_to_portal_rx) = tokio::sync::mpsc::unbounded_channel();

    tokio::spawn(async move {
        // Create session inside the spawned task
        let opts = CreateSessionOptions::default().set_capabilities(capabilities);
        let (session, _caps) = match proxy.create_session(None, opts).await {
            Ok(result) => result,
            Err(e) => {
                warn!("Failed to create InputCapture session: {}", e);
                return;
            }
        };

        debug!("InputCapture session created");

        // Connect to the EI (Emulated Input) socket
        let owned_fd = match proxy
            .connect_to_eis(&session, ConnectToEISOptions::default())
            .await
        {
            Ok(fd) => fd,
            Err(e) => {
                warn!("Failed to connect to EIS: {}", e);
                return;
            }
        };

        let raw_fd = owned_fd.as_raw_fd();
        info!(
            "InputCapture portal connected successfully (fd: {})",
            raw_fd
        );

        // Set up EI context for this task
        let mut ei_context = match ei::connect_with_fd(raw_fd).await {
            Ok(ctx) => ctx,
            Err(e) => {
                warn!("Failed to connect to EI in portal task: {}", e);
                return;
            }
        };

        // Send the fd back to the main task (for keeping the fd alive)
        // We need to duplicate the fd since the original is owned by the spawned task
        let dup_fd = unsafe { libc::dup(raw_fd) };
        if dup_fd < 0 {
            warn!("Failed to duplicate fd");
            return;
        }

        if fd_tx.send(dup_fd).is_err() {
            warn!("Failed to send fd to main task");
            return;
        };

        // Set up barriers based on zones from InputCapture portal
        info!("Setting up pointer barriers...");
        match setup_barriers(&proxy, &session, &client_configs_arc).await {
            Ok((map, bounds)) => {
                *barrier_map_clone.write().await = map;
                *desktop_bounds_clone.write().await = bounds;
            }
            Err(e) => {
                warn!("Failed to setup initial barriers: {}", e);
                return;
            }
        }

        // Wait for signal to enable the InputCapture session
        // This will be sent when the first client connects
        info!("Waiting for first client connection before enabling InputCapture...");
        let mut is_enabled = false;
        loop {
            match server_to_portal_rx.recv().await {
                Some(crate::server::Event::EnableCapture) if !is_enabled => {
                    info!("Received enable command");
                    break;
                }
                Some(crate::server::Event::DisableCapture) if is_enabled => {
                    // Will handle in main loop below
                    warn!("Received disable before initial enable, ignoring");
                }
                Some(_) => {
                    // Ignore redundant commands
                }
                None => {
                    warn!("Event channel closed, not enabling InputCapture");
                    return;
                }
            }
        }

        // Subscribe to signals BEFORE enabling the session
        debug!("Subscribing to zones_changed signal...");
        let mut zones_changed_stream = match proxy.receive_zones_changed().await {
            Ok(stream) => stream,
            Err(e) => {
                warn!("Failed to subscribe to zones_changed signal: {}", e);
                return;
            }
        };

        debug!("Subscribing to activated signal...");
        let mut activated_stream = match proxy.receive_activated().await {
            Ok(stream) => {
                info!("✓ Successfully subscribed to activated signal");
                stream
            }
            Err(e) => {
                warn!("Failed to subscribe to activated signal: {}", e);
                return;
            }
        };

        // Enable the InputCapture session (after subscribing to signals)
        info!("Enabling InputCapture session...");
        if let Err(e) = proxy.enable(&session, EnableOptions::default()).await {
            warn!("Failed to enable InputCapture session: {}", e);
            return;
        }
        is_enabled = true;
        info!("✓ InputCapture session enabled");

        info!("Portal task starting event loop");

        // Track whether we should poll EIS events (only when capture is active)
        let mut poll_eis = false;

        loop {
            use futures_util::StreamExt;

            tokio::select! {
                // Poll EIS events when capture is active
                _ = ei_context.recv_event(), if poll_eis => {
                    // Drain all pending input events and forward them
                    let events = ei_context.take_input_events();

                    if !events.is_empty() {
                        debug!("ei: Received {} events from portal", events.len());

                        for event in events {
                            let portal_event = match event {
                                ei::InputEvent::PointerAbsolute { x, y } => {
                                    crate::server::Event::PointerAbsolute { x, y }
                                }
                                ei::InputEvent::PointerRelative { dx, dy } => {
                                    crate::server::Event::PointerRelative { dx, dy }
                                }
                                ei::InputEvent::Button { button, is_press } => {
                                    crate::server::Event::Button { button, is_press }
                                }
                                ei::InputEvent::Key { keysym, is_press, mask, button } => {
                                    crate::server::Event::Key { keysym, is_press, mask, button }
                                }
                                ei::InputEvent::Scroll { x, y } => {
                                    crate::server::Event::Scroll { x, y }
                                }
                            };

                            if portal_to_server_tx.send(portal_event).is_err() {
                                warn!("Failed to send input event (receiver dropped)");
                                break;
                            }
                        }
                    }
                }

                // Handle activated events from the portal
                Some(activated) = activated_stream.next() => {
                    info!("Activated signal received!");

                    // Extract activation ID and cursor position
                    let activation_id = match activated.activation_id() {
                        Some(id) => id,
                        None => {
                            warn!("Activated event without activation ID");
                            continue;
                        }
                    };

                    let barrier_id = match activated.barrier_id() {
                        Some(ashpd::desktop::input_capture::ActivatedBarrier::Barrier(id)) => {
                            id.get()
                        }
                        Some(ashpd::desktop::input_capture::ActivatedBarrier::UnknownBarrier) => {
                            warn!("Activated event with unknown barrier");
                            continue;
                        }
                        None => {
                            warn!("Activated event without barrier ID");
                            continue;
                        }
                    };

                    let cursor = match activated.cursor_position() {
                        Some((x, y)) => Point::new(x as f64, y as f64),
                        None => {
                            warn!("Activated event without cursor position");
                            Point::new(0.0, 0.0)
                        }
                    };

                    info!(
                        "Portal activated! Activation ID: {}, Barrier ID: {}, Position: ({:.2}, {:.2})",
                        activation_id, barrier_id, cursor.x, cursor.y
                    );

                    // Look up client info from barrier map
                    let barrier_lookup = barrier_map_clone.read().await;
                    let (client_name, client_position) = match barrier_lookup.get(&barrier_id) {
                        Some((name, position)) => (name.clone(), *position),
                        None => {
                            warn!("Unknown barrier ID: {} (no client mapping found)", barrier_id);
                            continue;
                        }
                    };
                    drop(barrier_lookup);

                    // Send the activated event through the channel
                    let event = crate::server::Event::Activated {
                        client_name,
                        client_position,
                        activation_id,
                        cursor,
                    };

                    if portal_to_server_tx.send(event).is_err() {
                        warn!("Failed to send activated event (receiver dropped)");
                        break;
                    }
                    info!("✓ Activation event forwarded to server");

                    // Start polling EIS events now that capture is active
                    poll_eis = true;
                }

                // Handle zones_changed events from the portal
                Some(zones_changed) = zones_changed_stream.next() => {
                    info!(
                        "Zones changed: session={:?}, zone_set={:?}",
                        zones_changed.session_handle(),
                        zones_changed.zone_set()
                    );

                    // Re-setup barriers with the new zones
                    match setup_barriers(&proxy, &session, &client_configs_arc).await {
                        Ok((map, bounds)) => {
                            *barrier_map_clone.write().await = map;
                            *desktop_bounds_clone.write().await = bounds;
                            info!("✓ Barriers re-configured after zone change");

                            // Re-enable the session after reconfiguring barriers
                            if let Err(e) = proxy.enable(&session, EnableOptions::default()).await {
                                warn!("Failed to re-enable InputCapture session after zone change: {}", e);
                            } else {
                                info!("✓ InputCapture session re-enabled");
                            }
                        }
                        Err(e) => {
                            warn!("Failed to re-setup barriers after zone change: {}", e);
                        }
                    }
                }

                // Handle events from server
                Some(event) = server_to_portal_rx.recv() => {
                    match event {
                        crate::server::Event::ReleaseCapture { activation_id, cursor } => {
                            info!("Releasing InputCapture session: activation_id={:?}, cursor={:?}", activation_id, cursor);
                            let cursor_tuple = cursor.map(|p| (p.x, p.y));
                            let opts = ReleaseOptions::default().set_activation_id(activation_id).set_cursor_position(cursor_tuple);
                            if let Err(e) = proxy.release(&session, opts).await {
                                warn!("Failed to release InputCapture session: {}", e);
                            } else {
                                info!("✓ InputCapture session released");
                            }

                            // Stop polling EIS events since capture is no longer active
                            poll_eis = false;
                        }
                        crate::server::Event::EnableCapture if !is_enabled => {
                            info!("Enabling InputCapture session...");
                            if let Err(e) = proxy.enable(&session, EnableOptions::default()).await {
                                warn!("Failed to enable InputCapture session: {}", e);
                            } else {
                                is_enabled = true;
                                info!("✓ InputCapture session enabled");
                            }
                        }
                        crate::server::Event::DisableCapture if is_enabled => {
                            info!("Disabling InputCapture session...");
                            if let Err(e) = proxy.disable(&session, DisableOptions::default()).await {
                                warn!("Failed to disable InputCapture session: {}", e);
                            } else {
                                is_enabled = false;
                                info!("✓ InputCapture session disabled");
                            }
                        }
                        crate::server::Event::EnableCapture | crate::server::Event::DisableCapture => {
                            // Ignore redundant enable/disable commands
                            debug!("Ignoring redundant {:?} command (is_enabled={})", event, is_enabled);
                        }
                        _ => {
                            warn!("Portal received unexpected event: {:?}", event);
                        }
                    }
                }

                else => {
                    info!("All streams ended");
                    break;
                }
            }
        }

        warn!("Portal monitor task exiting!");
    });

    info!(
        "Portal task spawned, task will monitor for zone changes, activations, and release requests"
    );

    // Wait for the fd from the spawned task
    let raw_fd = fd_rx
        .await
        .context("Failed to receive fd from portal task")?;
    let owned_fd = unsafe { OwnedFd::from_raw_fd(raw_fd) };

    let portal_session = Session {
        _owned_fd: owned_fd,
        event_rx: portal_to_server_rx,
        event_tx: server_to_portal_tx,
    };

    Ok((portal_session, barrier_map, desktop_bounds))
}
