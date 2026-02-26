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
use std::os::fd::AsRawFd;
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

/// A connected InputCapture portal with event channels and desktop bounds
pub struct InputCapturePortal {
    /// Receiver for events from the portal
    pub event_rx: tokio::sync::mpsc::UnboundedReceiver<crate::server::Event>,
    /// Sender to send events to the portal
    pub event_tx: tokio::sync::mpsc::UnboundedSender<crate::server::Event>,
    /// Desktop bounds for coordinate mapping
    pub desktop_bounds: Arc<RwLock<DesktopBounds>>,
}

impl InputCapturePortal {
    /// Create a new InputCapturePortal by connecting to the portal and setting up EIS
    ///
    /// This function uses ashpd to establish a connection to the InputCapture portal,
    /// requesting permission to capture mouse and keyboard events. It creates a session,
    /// obtains a file descriptor for the EI connection, sets up the EI context,
    /// and configures pointer barriers based on client configurations.
    ///
    /// # Arguments
    ///
    /// * `client_configs` - Slice of client configurations
    ///
    /// # Returns
    ///
    /// Returns an InputCapturePortal instance
    ///
    /// # Errors
    ///
    /// Returns an error if the portal connection fails or permissions are denied
    pub async fn new(client_configs: &[ClientConfig]) -> Result<Self> {
        info!("Connecting to InputCapture portal");

        let proxy = InputCapture::new()
            .await
            .context("Failed to create InputCapture proxy")?;

        // Create shared state for the portal task
        let barrier_map = Arc::new(RwLock::new(HashMap::new()));
        let desktop_bounds = Arc::new(RwLock::new(DesktopBounds {
            min_x: 0,
            min_y: 0,
            max_x: 1920, // Default, will be updated after setup_barriers
            max_y: 1080,
        }));

        let (portal_to_server_tx, portal_to_server_rx) = tokio::sync::mpsc::unbounded_channel();
        let (server_to_portal_tx, server_to_portal_rx) = tokio::sync::mpsc::unbounded_channel();

        Self::spawn_portal_monitor_task(
            proxy,
            Arc::clone(&barrier_map),
            Arc::clone(&desktop_bounds),
            client_configs.to_vec(),
            portal_to_server_tx,
            server_to_portal_rx,
        );

        info!(
            "Portal task spawned, task will monitor for zone changes, activations, and release requests"
        );

        Ok(InputCapturePortal {
            event_rx: portal_to_server_rx,
            event_tx: server_to_portal_tx,
            desktop_bounds,
        })
    }

    /// Spawn the background task that monitors the portal
    fn spawn_portal_monitor_task(
        proxy: InputCapture,
        barrier_map: Arc<RwLock<HashMap<u32, (String, Position)>>>,
        desktop_bounds: Arc<RwLock<DesktopBounds>>,
        client_configs: Vec<ClientConfig>,
        portal_to_server_tx: tokio::sync::mpsc::UnboundedSender<crate::server::Event>,
        mut server_to_portal_rx: tokio::sync::mpsc::UnboundedReceiver<crate::server::Event>,
    ) {
        tokio::spawn(async move {
            if let Err(e) = Self::run_portal_monitor(
                proxy,
                barrier_map,
                desktop_bounds,
                client_configs,
                portal_to_server_tx,
                &mut server_to_portal_rx,
            )
            .await
            {
                warn!("Portal monitor task error: {}", e);
            }
            warn!("Portal monitor task exiting!");
        });
    }

    /// Initialize or re-initialize the portal session
    ///
    /// Creates a new session, connects to EIS, sets up barriers, and subscribes to signals.
    /// Returns everything needed for the event loop.
    async fn initialize_session(
        proxy: &InputCapture,
        client_configs: &[ClientConfig],
        barrier_map: &Arc<RwLock<HashMap<u32, (String, Position)>>>,
        desktop_bounds: &Arc<RwLock<DesktopBounds>>,
    ) -> Result<(
        ashpd::desktop::Session<InputCapture>,
        ei::EiContext,
        impl futures_util::Stream<Item = ashpd::desktop::input_capture::ZonesChanged>,
        impl futures_util::Stream<Item = ashpd::desktop::input_capture::Activated>,
        impl futures_util::Stream<Item = ashpd::desktop::input_capture::Deactivated>,
        impl futures_util::Stream<Item = ashpd::desktop::input_capture::Disabled>,
    )> {
        info!("Initializing InputCapture portal session");

        let capabilities = ashpd::desktop::input_capture::Capabilities::Keyboard
            | ashpd::desktop::input_capture::Capabilities::Pointer;

        let opts = CreateSessionOptions::default().set_capabilities(capabilities);
        let (session, _caps) = proxy
            .create_session(None, opts)
            .await
            .context("Failed to create InputCapture session")?;

        let _owned_fd = proxy
            .connect_to_eis(&session, ConnectToEISOptions::default())
            .await
            .context("Failed to connect to EIS")?;

        let raw_fd = _owned_fd.as_raw_fd();
        info!(
            "InputCapture portal connected successfully (fd: {})",
            raw_fd
        );

        let ei_context = ei::connect_with_fd(raw_fd)
            .await
            .context("Failed to connect to EI")?;

        // Keep the fd alive (it will be dropped when the EI context is dropped)
        std::mem::forget(_owned_fd);

        let (map, bounds) = Self::setup_barriers(proxy, &session, client_configs)
            .await
            .context("Failed to setup barriers")?;

        *barrier_map.write().await = map;
        *desktop_bounds.write().await = bounds;

        let zones_changed_stream = proxy
            .receive_zones_changed()
            .await
            .context("Failed to subscribe to zones_changed signal")?;

        let activated_stream = proxy
            .receive_activated()
            .await
            .context("Failed to subscribe to activated signal")?;

        let deactivated_stream = proxy
            .receive_deactivated()
            .await
            .context("Failed to subscribe to deactivated signal")?;

        let disabled_stream = proxy
            .receive_disabled()
            .await
            .context("Failed to subscribe to disabled signal")?;

        info!("✓ InputCapture portal session initialized");

        Ok((
            session,
            ei_context,
            zones_changed_stream,
            activated_stream,
            deactivated_stream,
            disabled_stream,
        ))
    }

    /// Main portal monitoring loop
    async fn run_portal_monitor(
        proxy: InputCapture,
        barrier_map: Arc<RwLock<HashMap<u32, (String, Position)>>>,
        desktop_bounds: Arc<RwLock<DesktopBounds>>,
        client_configs: Vec<ClientConfig>,
        portal_to_server_tx: tokio::sync::mpsc::UnboundedSender<crate::server::Event>,
        server_to_portal_rx: &mut tokio::sync::mpsc::UnboundedReceiver<crate::server::Event>,
    ) -> Result<()> {
        let mut is_enabled = false;
        let mut poll_eis = false;

        // Initialize the session for the first time
        let (
            mut session,
            mut ei_context,
            mut zones_changed_stream,
            mut activated_stream,
            mut deactivated_stream,
            mut disabled_stream,
        ) = Self::initialize_session(&proxy, &client_configs, &barrier_map, &desktop_bounds)
            .await?;

        // Run the main event loop
        debug!("Portal task starting event loop");

        loop {
            use futures_util::StreamExt;

            tokio::select! {
                // Poll EIS events when capture is active
                _ = ei_context.recv_event(), if poll_eis => {
                    if !Self::handle_ei_events(&mut ei_context, &portal_to_server_tx) {
                        break;
                    }
                }

                // Handle activated events from the portal
                Some(activated) = activated_stream.next() => {
                    if Self::handle_activated_event(activated, &barrier_map, &portal_to_server_tx).await {
                        // Start polling EIS events now that capture is active
                        poll_eis = true;
                    }
                }

                // Handle deactivated events from the portal
                Some(deactivated) = deactivated_stream.next() => {
                    Self::handle_deactivated_event(deactivated);
                    // Stop polling EIS events since capture is no longer active
                    // FIXME: potential for a race condition here because we might receive that
                    // signal before all EI events have been processed. Fix when needed
                    poll_eis = false;
                }

                // Handle disabled events from the portal
                Some(disabled) = disabled_stream.next() => {
                    warn!(
                        "InputCapture portal session was disabled (session: {:?}). Re-initializing...",
                        disabled.session_handle()
                    );

                    is_enabled = false;
                    poll_eis = false;

                    // Drop the current session and all streams
                    drop(session);
                    drop(ei_context);
                    drop(zones_changed_stream);
                    drop(activated_stream);
                    drop(deactivated_stream);
                    drop(disabled_stream);

                    // Re-initialize the session
                    match Self::initialize_session(&proxy, &client_configs, &barrier_map, &desktop_bounds).await {
                        Ok((new_session, new_ei_context, new_zones_changed_stream, new_activated_stream, new_deactivated_stream, new_disabled_stream)) => {
                            session = new_session;
                            ei_context = new_ei_context;
                            zones_changed_stream = new_zones_changed_stream;
                            activated_stream = new_activated_stream;
                            deactivated_stream = new_deactivated_stream;
                            disabled_stream = new_disabled_stream;
                            info!("✓ InputCapture portal session re-initialized successfully");
                        }
                        Err(e) => {
                            warn!("Failed to re-initialize InputCapture session: {}", e);
                            warn!("Portal monitor task will exit");
                            break;
                        }
                    }
                }

                // Handle zones_changed events from the portal
                Some(zones_changed) = zones_changed_stream.next() => {
                    Self::handle_zones_changed_event(
                        zones_changed,
                        &proxy,
                        &session,
                        &client_configs,
                        &barrier_map,
                        &desktop_bounds,
                    ).await;
                }

                // Handle events from server
                Some(event) = server_to_portal_rx.recv() => {
                    let (new_is_enabled, new_poll_eis) = Self::handle_server_event(
                        event,
                        &proxy,
                        &session,
                        is_enabled,
                        poll_eis,
                    ).await;
                    is_enabled = new_is_enabled;
                    poll_eis = new_poll_eis;
                }

                else => {
                    info!("All streams ended");
                    break;
                }
            }
        }

        warn!("Portal monitor task exiting!");
        Ok(())
    }

    /// Handle EIS input events and forward them to the server
    ///
    /// Drains all pending input events from the EI context and forwards them
    /// through the channel to the server.
    ///
    /// # Returns
    ///
    /// Returns `false` if the receiver dropped (channel closed), `true` otherwise
    fn handle_ei_events(
        ei_context: &mut ei::EiContext,
        portal_to_server_tx: &tokio::sync::mpsc::UnboundedSender<crate::server::Event>,
    ) -> bool {
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
                    ei::InputEvent::Key {
                        keysym,
                        is_press,
                        mask,
                        button,
                    } => crate::server::Event::Key {
                        keysym,
                        is_press,
                        mask,
                        button,
                    },
                    ei::InputEvent::Scroll { x, y } => crate::server::Event::Scroll { x, y },
                };

                if portal_to_server_tx.send(portal_event).is_err() {
                    warn!("Failed to send input event (receiver dropped)");
                    return false;
                }
            }
        }

        true
    }

    /// Handle activated event from the portal
    ///
    /// Processes a barrier activation event by extracting the activation details,
    /// looking up the client information, and forwarding the event to the server.
    ///
    /// # Returns
    ///
    /// Returns `true` if the event was processed successfully (EIS polling should be enabled),
    /// `false` if the channel closed or the event should be ignored
    async fn handle_activated_event(
        activated: ashpd::desktop::input_capture::Activated,
        barrier_map: &Arc<RwLock<HashMap<u32, (String, Position)>>>,
        portal_to_server_tx: &tokio::sync::mpsc::UnboundedSender<crate::server::Event>,
    ) -> bool {
        info!("Activated signal received!");

        // Extract activation ID and cursor position
        let activation_id = match activated.activation_id() {
            Some(id) => id,
            None => {
                warn!("Activated event without activation ID");
                return false;
            }
        };

        let barrier_id = match activated.barrier_id() {
            Some(ashpd::desktop::input_capture::ActivatedBarrier::Barrier(id)) => id.get(),
            Some(ashpd::desktop::input_capture::ActivatedBarrier::UnknownBarrier) => {
                warn!("Activated event with unknown barrier");
                return false;
            }
            None => {
                warn!("Activated event without barrier ID");
                return false;
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
        let barrier_lookup = barrier_map.read().await;
        let (client_name, client_position) = match barrier_lookup.get(&barrier_id) {
            Some((name, position)) => (name.clone(), *position),
            None => {
                warn!(
                    "Unknown barrier ID: {} (no client mapping found)",
                    barrier_id
                );
                return false;
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
            return false;
        }
        info!("✓ Activation event forwarded to server");

        true
    }

    /// Handle deactivated event from the portal
    ///
    /// Processes a barrier deactivation event, which indicates that the cursor
    /// has moved back from the client to the server.
    fn handle_deactivated_event(deactivated: ashpd::desktop::input_capture::Deactivated) {
        info!("Deactivated signal received!");

        let activation_id = match deactivated.activation_id() {
            Some(id) => id,
            None => {
                warn!("Deactivated event without activation ID");
                return;
            }
        };

        info!(
            "Portal deactivated! Activation ID: {}, Session: {:?}",
            activation_id,
            deactivated.session_handle()
        );
    }

    /// Handle zones_changed event from the portal
    ///
    /// Processes a zone change event by re-setting up barriers with the new zones
    /// and re-enabling the session.
    async fn handle_zones_changed_event(
        zones_changed: ashpd::desktop::input_capture::ZonesChanged,
        proxy: &InputCapture,
        session: &ashpd::desktop::Session<InputCapture>,
        client_configs: &[ClientConfig],
        barrier_map: &Arc<RwLock<HashMap<u32, (String, Position)>>>,
        desktop_bounds: &Arc<RwLock<DesktopBounds>>,
    ) {
        info!(
            "Zones changed: session={:?}, zone_set={:?}",
            zones_changed.session_handle(),
            zones_changed.zone_set()
        );

        // Re-setup barriers with the new zones
        match Self::setup_barriers(proxy, session, client_configs).await {
            Ok((map, bounds)) => {
                *barrier_map.write().await = map;
                *desktop_bounds.write().await = bounds;
                info!("✓ Barriers re-configured after zone change");

                // Re-enable the session after reconfiguring barriers
                if let Err(e) = proxy.enable(session, EnableOptions::default()).await {
                    warn!(
                        "Failed to re-enable InputCapture session after zone change: {}",
                        e
                    );
                } else {
                    info!("✓ InputCapture session re-enabled");
                }
            }
            Err(e) => {
                warn!("Failed to re-setup barriers after zone change: {}", e);
            }
        }
    }

    /// Handle server event (commands from the server task)
    ///
    /// Processes events sent from the server task, such as release capture,
    /// enable/disable session requests.
    ///
    /// # Returns
    ///
    /// Returns (updated_is_enabled, updated_poll_eis) tuple
    async fn handle_server_event(
        event: crate::server::Event,
        proxy: &InputCapture,
        session: &ashpd::desktop::Session<InputCapture>,
        is_enabled: bool,
        poll_eis: bool,
    ) -> (bool, bool) {
        match event {
            crate::server::Event::ReleaseCapture {
                activation_id,
                cursor,
            } => {
                info!(
                    "Releasing InputCapture session: activation_id={:?}, cursor={:?}",
                    activation_id, cursor
                );
                let cursor_tuple = cursor.map(|p| (p.x, p.y));
                let opts = ReleaseOptions::default()
                    .set_activation_id(activation_id)
                    .set_cursor_position(cursor_tuple);
                if let Err(e) = proxy.release(session, opts).await {
                    warn!("Failed to release InputCapture session: {}", e);
                } else {
                    info!("✓ InputCapture session released");
                }

                // Stop polling EIS events since capture is no longer active
                (is_enabled, false)
            }
            crate::server::Event::EnableCapture if !is_enabled => {
                info!("Enabling InputCapture session...");
                if let Err(e) = proxy.enable(session, EnableOptions::default()).await {
                    warn!("Failed to enable InputCapture session: {}", e);
                    (is_enabled, poll_eis)
                } else {
                    info!("✓ InputCapture session enabled");
                    (true, poll_eis)
                }
            }
            crate::server::Event::DisableCapture if is_enabled => {
                info!("Disabling InputCapture session...");
                if let Err(e) = proxy.disable(session, DisableOptions::default()).await {
                    warn!("Failed to disable InputCapture session: {}", e);
                    (is_enabled, poll_eis)
                } else {
                    info!("✓ InputCapture session disabled");
                    (false, poll_eis)
                }
            }
            crate::server::Event::EnableCapture | crate::server::Event::DisableCapture => {
                // Ignore redundant enable/disable commands
                debug!(
                    "Ignoring redundant {:?} command (is_enabled={})",
                    event, is_enabled
                );
                (is_enabled, poll_eis)
            }
            _ => {
                warn!("Portal received unexpected event: {:?}", event);
                (is_enabled, poll_eis)
            }
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
    /// * `client_configs` - Slice of client configurations
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
        client_configs: &[ClientConfig],
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

        // Create barriers only on the outer edges of *our* desktop area
        for config in client_configs.iter().filter(|c| c.reference == "self") {
            let barrier_id_nonzero = NonZeroU32::new(barrier_id).unwrap();
            let barrier = match config.position {
                Position::LeftOf => Barrier::new(barrier_id_nonzero, (min_x, min_y, min_x, max_y)),
                Position::RightOf => Barrier::new(barrier_id_nonzero, (max_x, min_y, max_x, max_y)),
                Position::TopOf => Barrier::new(barrier_id_nonzero, (min_x, min_y, max_x, min_y)),
                Position::BottomOf => {
                    Barrier::new(barrier_id_nonzero, (min_x, max_y, max_x, max_y))
                }
            };

            debug!(
                "Adding barrier (ID {}) {} self for client '{}'",
                barrier_id, config.position, &config.name
            );

            barriers.push(barrier);
            barrier_map.insert(barrier_id, (config.name.clone(), config.position));
            barrier_id += 1;
        }

        debug!(
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
}
