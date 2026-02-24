//! TCP server module for handling Synergy client connections using schengen::server
//!
//! This module uses the schengen::server API to accept and manage Synergy protocol connections,
//! and integrates with the InputCapture portal to forward input events.

use anyhow::{Context, Result};
use log::{debug, info, warn};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::config::ClientConfig;
use crate::ei;
use crate::portal;

const DEFAULT_PORT: u16 = 24801;

/// Events that flow between the portal and server
#[derive(Debug, Clone)]
pub enum Event {
    /// Barrier was activated - cursor crossed into client area
    Activated {
        /// Which client's barrier was crossed
        client_name: String,
        /// Client's position relative to server
        client_position: crate::config::Position,
        /// Activation ID for releasing the capture
        activation_id: u32,
        /// Cursor position where the barrier was crossed
        cursor: portal::Point,
    },
    /// A Synergy client disconnected
    ClientDisconnected {
        /// Name of the disconnected client
        client_name: String,
    },
    /// Request to enable the InputCapture portal
    EnableCapture,
    /// Request to disable the InputCapture portal
    DisableCapture,
    /// Request to release the current capture session
    ReleaseCapture {
        /// Activation ID to release
        activation_id: Option<u32>,
        /// Optional cursor position for release
        cursor: Option<portal::Point>,
    },
}

/// Information about an active client receiving input
struct ActiveClient {
    /// The schengen client ID
    id: schengen::server::ClientId,
    /// The client's name
    name: String,
    /// The client's position relative to the server
    position: crate::config::Position,
    /// The client's screen dimensions
    size: portal::Size,
    /// Coordinate offset for mapping desktop coords to client coords
    /// client_coord = desktop_coord - coord_offset
    coord_offset: portal::Point,
}

/// Typestate marker for a server that has not yet connected to the portal
pub struct NotConnected;

/// Typestate representing a server that has successfully connected to the portal
pub struct PortalConnected {
    portal_session: portal::Session,
    ei_context: ei::EiContext,
    desktop_bounds: Arc<RwLock<portal::DesktopBounds>>,
}

/// Server for managing Synergy protocol connections with InputCapture portal integration
pub struct Server<State> {
    port: u16,
    client_configs: HashMap<String, ClientConfig>,
    state: State,
}

impl Server<NotConnected> {
    /// Create a new Server instance
    ///
    /// # Arguments
    ///
    /// * `client_configs` - Map of client names to their configurations
    pub fn new(client_configs: HashMap<String, ClientConfig>) -> Self {
        let mut server = Self {
            port: 0,
            client_configs,
            state: NotConnected,
        };
        server.set_port(DEFAULT_PORT);
        server
    }

    /// Set the port to listen on
    ///
    /// # Arguments
    ///
    /// * `port` - The port number
    pub fn set_port(&mut self, port: u16) {
        self.port = port;
    }

    /// Connect to the InputCapture portal and set up libei with barriers
    ///
    /// # Errors
    ///
    /// Returns an error if portal connection or barrier setup fails
    pub async fn connect_portal(self) -> Result<Server<PortalConnected>> {
        info!("Connecting to InputCapture portal and libei...");
        let (portal_session, ei_context, _barrier_map, desktop_bounds) =
            portal::connect_input_capture(&self.client_configs).await?;

        Ok(Server {
            port: self.port,
            client_configs: self.client_configs,
            state: PortalConnected {
                portal_session,
                ei_context,
                desktop_bounds,
            },
        })
    }
}

impl Server<PortalConnected> {
    /// Run the TCP server using schengen::server API
    ///
    /// This function builds a schengen server with the configured clients and manages
    /// the integration with the InputCapture portal for barrier-based input forwarding.
    ///
    /// # Errors
    ///
    /// Returns an error if the server fails to start or encounters a fatal error
    pub async fn run(self) -> Result<()> {
        let portal_session = self.state.portal_session;
        let ei_context = self.state.ei_context;
        let desktop_bounds = self.state.desktop_bounds;

        let listen_addr: SocketAddr = format!("0.0.0.0:{}", self.port)
            .parse()
            .context("Failed to parse listen address")?;
        info!("Step 2/2: Starting TCP server on {}...", listen_addr);

        let mut builder = schengen::server::Builder::new().port(self.port);

        // Add all clients that are positioned relative to "self" (the server)
        let mut pending_clients: Vec<(String, ClientConfig)> = vec![];
        let mut added_clients: HashMap<String, schengen::server::NewClient> = HashMap::new();

        for (name, config) in &self.client_configs {
            if config.reference == "self" {
                let client = schengen::server::ClientBuilder::new(name)
                    .position(config.position.to_schengen())
                    .build();

                builder = builder
                    .add_client(client.clone())
                    .with_context(|| format!("Failed to add client '{}'", name))?;

                added_clients.insert(name.clone(), client);
                info!("Added client '{}' at {:?} of server", name, config.position);
            } else {
                pending_clients.push((name.clone(), config.clone()));
            }
        }

        // Then, add clients positioned relative to other clients
        // We may need multiple passes if clients reference each other in a chain
        while !pending_clients.is_empty() {
            let mut progress = false;
            let mut still_pending = vec![];

            for (name, config) in pending_clients {
                if let Some(referenced_client) = added_clients.get(&config.reference) {
                    let client = schengen::server::ClientBuilder::new(&name)
                        .position(config.position.to_schengen())
                        .relative_to(referenced_client)
                        .build();

                    builder = builder
                        .add_client(client.clone())
                        .with_context(|| format!("Failed to add client '{}'", name))?;

                    added_clients.insert(name.clone(), client);
                    info!(
                        "Added client '{}' at {:?} of '{}'",
                        name, config.position, config.reference
                    );
                    progress = true;
                } else {
                    still_pending.push((name, config));
                }
            }

            if !progress && !still_pending.is_empty() {
                anyhow::bail!("Circular client reference detected or missing reference client");
            }

            pending_clients = still_pending;
        }

        let server = builder
            .listen()
            .await
            .context("Failed to start schengen server")?;

        info!("Schengen server listening on {}", listen_addr);

        let server = Arc::new(server);

        // Split portal session to extract components
        let (_owned_fd, mut event_rx, event_tx) = portal_session.split();

        // Clone server for the barrier event handler
        let barrier_server = Arc::clone(&server);
        let event_tx_clone = event_tx.clone();

        // Spawn a task to handle barrier events
        let barrier_task = tokio::spawn(async move {
            // Keep _owned_fd alive for the lifetime of the task
            let _keep_alive = _owned_fd;
            handle_barrier_events(
                barrier_server,
                &mut event_rx,
                event_tx_clone,
                ei_context,
                desktop_bounds,
            )
            .await
        });

        // Main event loop - handle server events
        let mut portal_enabled = false;
        loop {
            match server.recv_event().await {
                Ok(schengen::server::ServerEvent::ClientConnected {
                    client_id,
                    name,
                    width,
                    height,
                }) => {
                    info!("✓ Client '{name}' ({client_id:?}) connected ({width}x{height})");
                    // Enable portal on first client connection
                    if !portal_enabled {
                        if let Err(e) = event_tx.send(Event::EnableCapture) {
                            warn!("Failed to send enable command to portal: {}", e);
                        } else {
                            debug!("First client connected, enabling InputCapture portal");
                            portal_enabled = true;
                        }
                    }
                }

                Ok(schengen::server::ServerEvent::ClientDisconnected { client_id, name }) => {
                    info!("✗ Client '{name}' ({client_id:?}) disconnected");
                    // Notify barrier task about the disconnection
                    if let Err(e) = event_tx.send(Event::ClientDisconnected {
                        client_name: name.clone(),
                    }) {
                        warn!(
                            "Failed to notify barrier task about client '{name}' disconnection: {e}",
                        );
                    }

                    // Disable portal if this was the last client
                    let connected = server.clients().await;
                    if connected.is_empty() && portal_enabled {
                        if let Err(e) = event_tx.send(Event::DisableCapture) {
                            warn!("Failed to send disable command to portal: {}", e);
                        } else {
                            info!("Last client disconnected, disabling InputCapture portal");
                            portal_enabled = false;
                        }
                    }
                }

                Ok(schengen::server::ServerEvent::ClipboardData {
                    client_id, data, ..
                }) => {
                    debug!(
                        "📋 Clipboard data from {:?}: {} bytes",
                        client_id,
                        data.len()
                    );
                }

                Ok(schengen::server::ServerEvent::ScreenSaverChanged { client_id, active }) => {
                    let state = if active { "activated" } else { "deactivated" };
                    info!("💤 Screen saver {} on {:?}", state, client_id);
                }

                Ok(schengen::server::ServerEvent::ClientInfoUpdated {
                    client_id,
                    width,
                    height,
                }) => {
                    info!("ℹ️ Client {client_id:?} updated dimensions: {width}x{height}",);
                }

                Err(e) => {
                    warn!("Server error: {}", e);
                    break;
                }
            }
        }

        // Wait for barrier task to complete
        let _ = barrier_task.await;

        Ok(())
    }
}

/// Handle portal events and forward input to clients
///
/// This function monitors events from the portal (barrier activations, client disconnects)
/// and forwards the appropriate input events to clients using the schengen server API.
///
/// # Arguments
///
/// * `server` - The schengen server for sending messages to clients
/// * `event_rx` - Receiver for events from the portal
/// * `event_tx` - Sender for events to the portal
/// * `ei_context` - The EI context for receiving input events
/// * `desktop_bounds` - Desktop bounds for checking if pointer is back on server
///
/// # Errors
///
/// Returns an error if receiving events fails
async fn handle_barrier_events(
    server: Arc<schengen::server::Server>,
    event_rx: &mut tokio::sync::mpsc::UnboundedReceiver<Event>,
    event_tx: tokio::sync::mpsc::UnboundedSender<Event>,
    mut ei_context: ei::EiContext,
    desktop_bounds: Arc<RwLock<portal::DesktopBounds>>,
) -> Result<()> {
    info!("Starting barrier event handler");

    // Track the currently active client (the one receiving input)
    let mut active_client: Option<ActiveClient> = None;

    // Track the current activation ID for releasing
    let mut current_activation_id: Option<u32> = None;

    // Track the current cursor position (in absolute desktop coordinates)
    let mut cursor = portal::Point::new(1920.0 / 2.0, 1080.0 / 2.0);

    loop {
        tokio::select! {
            // Handle portal events
            Some(event) = event_rx.recv() => {
                match event {
                    Event::Activated {
                        client_name,
                        client_position,
                        activation_id,
                        cursor: event_cursor,
                    } => {
                        info!(
                            "Barrier activated for client '{}' at cursor=({:.2}, {:.2})",
                            client_name, event_cursor.x, event_cursor.y
                        );

                        // Find the client ID from the connected clients list
                        let connected = server.clients().await;
                        let client_info = match connected.iter().find(|c| c.name() == client_name) {
                            Some(c) => c,
                            None => {
                                warn!("Client '{}' not connected, ignoring barrier activation", client_name);
                                continue;
                            }
                        };

                        let client_id = client_info.id();
                        let client_width = client_info.width;
                        let client_height = client_info.height;

                        // Map desktop barrier coordinates to client entry coordinates
                        // based on the client's position relative to the server
                        let bounds = desktop_bounds.read().await;
                        let desktop_width = (bounds.max_x - bounds.min_x) as f64;
                        let desktop_height = (bounds.max_y - bounds.min_y) as f64;
                        drop(bounds);

                        let (enter_x, enter_y) = match client_position {
                            crate::config::Position::RightOf => {
                                // Client is to the right: enter at left edge (x=0)
                                // Map y coordinate from desktop to client coordinate space
                                let y_ratio = event_cursor.y / desktop_height;
                                let client_y = (y_ratio * client_height as f64).clamp(0.0, client_height as f64 - 1.0) as i16;
                                (0, client_y)
                            }
                            crate::config::Position::LeftOf => {
                                // Client is to the left: enter at right edge (x=client_width-1)
                                // Map y coordinate from desktop to client coordinate space
                                let y_ratio = event_cursor.y / desktop_height;
                                let client_y = (y_ratio * client_height as f64).clamp(0.0, client_height as f64 - 1.0) as i16;
                                ((client_width - 1) as i16, client_y)
                            }
                            crate::config::Position::TopOf => {
                                // Client is above: enter at bottom edge (y=client_height-1)
                                // Map x coordinate from desktop to client coordinate space
                                let x_ratio = event_cursor.x / desktop_width;
                                let client_x = (x_ratio * client_width as f64).clamp(0.0, client_width as f64 - 1.0) as i16;
                                (client_x, (client_height - 1) as i16)
                            }
                            crate::config::Position::BottomOf => {
                                // Client is below: enter at top edge (y=0)
                                // Map x coordinate from desktop to client coordinate space
                                let x_ratio = event_cursor.x / desktop_width;
                                let client_x = (x_ratio * client_width as f64).clamp(0.0, client_width as f64 - 1.0) as i16;
                                (client_x, 0)
                            }
                        };

                        info!("Switching input focus to client '{}' ({}x{}), entering at ({}, {})",
                            client_name, client_width, client_height, enter_x, enter_y);
                        if let Err(e) = server.send_cursor_entered(
                            client_id,
                            enter_x,
                            enter_y,
                            0,  // sequence number
                            0,  // mask
                        ).await {
                            warn!("Failed to send cursor entered to '{}': {}", client_name, e);
                            continue;
                        }

                        // Set this client as the active one and store activation ID
                        active_client = Some(ActiveClient {
                            id: client_id,
                            name: client_name.clone(),
                            position: client_position,
                            size: portal::Size::new(client_width as f64, client_height as f64),
                            coord_offset: portal::Point::new(
                                event_cursor.x - enter_x as f64,
                                event_cursor.y - enter_y as f64,
                            ),
                        });
                        current_activation_id = Some(activation_id);

                        // Update cursor position to the activation point (in desktop coordinates)
                        cursor = event_cursor;

                        info!("Client '{}' is now active and receiving input (activation_id={}, cursor=({:.2}, {:.2}))",
                            client_name, activation_id, cursor.x, cursor.y);
                    }

                    Event::ClientDisconnected { client_name } => {
                        debug!("Received disconnect notification for client '{}'", client_name);

                        // Check if this client is currently active (has input focus)
                        if let Some(ref client) = active_client {
                            // If the disconnected client was the active one, release InputCapture
                            if client.name == client_name {
                                warn!("Active client '{}' disconnected, releasing InputCapture", client_name);

                                // Release the InputCapture session
                                if let Some(activation_id) = current_activation_id {
                                    info!("Releasing InputCapture due to active client disconnect (activation_id={})", activation_id);
                                    if let Err(e) = event_tx.send(Event::ReleaseCapture {
                                        activation_id: Some(activation_id),
                                        cursor: None,
                                    }) {
                                        warn!("Failed to send release request: {:?}", e);
                                    }
                                }

                                // Clear active client and activation ID
                                active_client = None;
                                current_activation_id = None;
                                cursor = portal::Point::new(0.0, 0.0);
                            }
                        }
                    }

                    _ => {
                        warn!("Unexpected event in barrier handler: {:?}", event);
                    }
                }
            }

            // Process EI events and forward to active client
            // Only poll EI when we have an active activation to avoid busy-looping
            _ = ei_context.recv_event(), if current_activation_id.is_some() => {
                // Always drain input events to prevent accumulation
                let events = ei_context.take_input_events();

                if events.is_empty() {
                    continue;
                }

                debug!("ei: Received {} events", events.len());

                let Some(ref client) = active_client else {
                    // No active client, discard events
                    debug!("ei: No active client, discarding {} events", events.len());
                    continue;
                };

                // Process events and check if pointer returned to server
                let mut pointer_back_on_server = false;
                let mut release_cursor_pos: Option<portal::Point> = None;
                let mut client_disconnected = false;
                let mut last_mouse_pos: Option<portal::Point> = None;
                let mut needs_mouse_update = false;

                let bounds = desktop_bounds.read().await;

                for event in events {
                    match event {
                        ei::InputEvent::PointerAbsolute { x, y } => {
                            cursor = portal::Point::new(x, y);
                            debug!("ei: PointerAbsolute event: ({:.2}, {:.2})", x, y);

                            // Clamp cursor to client bounds based on client position
                            let client_min_x = client.coord_offset.x;
                            let client_max_x = client.coord_offset.x + client.size.width - 1.0;
                            let client_min_y = client.coord_offset.y;
                            let client_max_y = client.coord_offset.y + client.size.height - 1.0;

                            match client.position {
                                crate::config::Position::RightOf => {
                                    // Entered from left - allow exit left, clamp right/top/bottom
                                    cursor.x = cursor.x.min(client_max_x);
                                    cursor.y = cursor.y.clamp(client_min_y, client_max_y);
                                }
                                crate::config::Position::LeftOf => {
                                    // Entered from right - allow exit right, clamp left/top/bottom
                                    cursor.x = cursor.x.max(client_min_x);
                                    cursor.y = cursor.y.clamp(client_min_y, client_max_y);
                                }
                                crate::config::Position::TopOf => {
                                    // Entered from bottom - allow exit bottom, clamp left/right/top
                                    cursor.x = cursor.x.clamp(client_min_x, client_max_x);
                                    cursor.y = cursor.y.max(client_min_y);
                                }
                                crate::config::Position::BottomOf => {
                                    // Entered from top - allow exit top, clamp left/right/bottom
                                    cursor.x = cursor.x.clamp(client_min_x, client_max_x);
                                    cursor.y = cursor.y.min(client_max_y);
                                }
                            }

                            // Check if pointer is within desktop bounds
                            if cursor.x >= bounds.min_x as f64 && cursor.x < bounds.max_x as f64 &&
                               cursor.y >= bounds.min_y as f64 && cursor.y < bounds.max_y as f64 {
                                info!("Pointer (abs) at ({:.2}, {:.2}) is back within desktop bounds", cursor.x, cursor.y);
                                pointer_back_on_server = true;
                                release_cursor_pos = Some(cursor);
                                break;
                            }

                            // Coalesce mouse moves - just track the latest position
                            last_mouse_pos = Some(cursor);
                            needs_mouse_update = true;
                        }
                        ei::InputEvent::PointerRelative { dx, dy } => {
                            cursor.x += dx;
                            cursor.y += dy;
                            debug!("ei: PointerRelative event: delta=({:.2}, {:.2}), cursor=({:.2}, {:.2})", dx, dy, cursor.x, cursor.y);

                            // Clamp cursor to client bounds based on client position
                            let client_min_x = client.coord_offset.x;
                            let client_max_x = client.coord_offset.x + client.size.width - 1.0;
                            let client_min_y = client.coord_offset.y;
                            let client_max_y = client.coord_offset.y + client.size.height - 1.0;

                            match client.position {
                                crate::config::Position::RightOf => {
                                    // Entered from left - allow exit left, clamp right/top/bottom
                                    cursor.x = cursor.x.min(client_max_x);
                                    cursor.y = cursor.y.clamp(client_min_y, client_max_y);
                                }
                                crate::config::Position::LeftOf => {
                                    // Entered from right - allow exit right, clamp left/top/bottom
                                    cursor.x = cursor.x.max(client_min_x);
                                    cursor.y = cursor.y.clamp(client_min_y, client_max_y);
                                }
                                crate::config::Position::TopOf => {
                                    // Entered from bottom - allow exit bottom, clamp left/right/top
                                    cursor.x = cursor.x.clamp(client_min_x, client_max_x);
                                    cursor.y = cursor.y.max(client_min_y);
                                }
                                crate::config::Position::BottomOf => {
                                    // Entered from top - allow exit top, clamp left/right/bottom
                                    cursor.x = cursor.x.clamp(client_min_x, client_max_x);
                                    cursor.y = cursor.y.min(client_max_y);
                                }
                            }

                            // Check if pointer is within desktop bounds
                            if cursor.x >= bounds.min_x as f64 && cursor.x < bounds.max_x as f64 &&
                               cursor.y >= bounds.min_y as f64 && cursor.y < bounds.max_y as f64 {
                                info!("Pointer (rel) at ({:.2}, {:.2}) is back within desktop bounds", cursor.x, cursor.y);
                                pointer_back_on_server = true;
                                release_cursor_pos = Some(cursor);
                                break;
                            }

                            // Coalesce mouse moves - just track the latest position
                            last_mouse_pos = Some(cursor);
                            needs_mouse_update = true;
                        }
                        ei::InputEvent::Button { button, is_press } => {
                            if !client_disconnected {
                                debug!("Forwarding button {} ({}) to client", button, if is_press { "press" } else { "release" });
                                let result = if is_press {
                                    server.send_mouse_button_down(client.id, button as u8).await
                                } else {
                                    server.send_mouse_button_up(client.id, button as u8).await
                                };
                                if let Err(e) = result {
                                    warn!("Failed to send mouse button: {}", e);
                                    client_disconnected = true;
                                }
                            }
                        }
                        ei::InputEvent::Key { keysym, is_press, mask, button } => {
                            if !client_disconnected {
                                debug!("Forwarding keysym 0x{:x} ({}) to client", keysym, if is_press { "press" } else { "release" });
                                let result = if is_press {
                                    server.send_key_down(client.id, keysym as u16, mask, button).await
                                } else {
                                    server.send_key_up(client.id, keysym as u16, mask, button).await
                                };
                                if let Err(e) = result {
                                    warn!("Failed to send key: {}", e);
                                    client_disconnected = true;
                                }
                            }
                        }
                        ei::InputEvent::Scroll { x, y } => {
                            if !client_disconnected {
                                debug!("Forwarding scroll ({}, {}) to client", x, y);
                                let xdelta = (x * 120.0) as i16;
                                let ydelta = (y * 120.0) as i16;
                                if let Err(e) = server.send_mouse_wheel(client.id, xdelta, ydelta).await {
                                    warn!("Failed to send mouse wheel: {}", e);
                                    client_disconnected = true;
                                }
                            }
                        }
                    }
                }
                drop(bounds);

                // Send coalesced mouse position (only the final position from all events)
                if needs_mouse_update && !client_disconnected
                    && let Some(final_pos) = last_mouse_pos
                {
                    // Map desktop coordinates to client coordinates using the offset
                    let client_x = ((final_pos.x - client.coord_offset.x).clamp(0.0, client.size.width - 1.0)) as i16;
                    let client_y = ((final_pos.y - client.coord_offset.y).clamp(0.0, client.size.height - 1.0)) as i16;
                    debug!("Sending coalesced mouse move to client: ({}, {}) [from desktop ({:.2}, {:.2})]",
                        client_x, client_y, final_pos.x, final_pos.y);
                    if let Err(e) = server.send_mouse_move(client.id, client_x, client_y).await {
                        warn!("Failed to send coalesced mouse move: {}", e);
                        client_disconnected = true;
                    }
                }

                if client_disconnected {
                    // Client has disconnected, clear active client and release portal
                    warn!("Client disconnected, clearing active client");

                    // Release the InputCapture session
                    if let Some(activation_id) = current_activation_id {
                        info!("Releasing InputCapture due to client disconnect (activation_id={})", activation_id);
                        if let Err(e) = event_tx.send(Event::ReleaseCapture {
                            activation_id: Some(activation_id),
                            cursor: None,
                        }) {
                            warn!("Failed to send release request: {:?}", e);
                        }
                    }

                    // Clear active client and activation ID
                    active_client = None;
                    current_activation_id = None;
                    cursor = portal::Point::new(0.0, 0.0);
                } else if pointer_back_on_server {
                    // Send cursor left message to client
                    if let Err(e) = server.send_cursor_left(client.id).await {
                        warn!("Failed to send cursor left to client: {}", e);
                    }

                    // Release the InputCapture session
                    if let Some(activation_id) = current_activation_id {
                        let cursor_str = release_cursor_pos.map(|p| format!("({:.2}, {:.2})", p.x, p.y))
                            .unwrap_or_else(|| "None".to_string());
                        info!("Releasing InputCapture (activation_id={}, cursor={})", activation_id, cursor_str);
                        if let Err(e) = event_tx.send(Event::ReleaseCapture {
                            activation_id: Some(activation_id),
                            cursor: release_cursor_pos,
                        }) {
                            warn!("Failed to send release request: {:?}", e);
                        }
                    } else {
                        warn!("Cannot release InputCapture: no activation_id stored");
                    }

                    // Clear active client and activation ID
                    active_client = None;
                    current_activation_id = None;
                    cursor = portal::Point::new(0.0, 0.0);
                    info!("Input focus returned to server");
                }
            }

            else => {
                info!("All event streams ended");
                break;
            }
        }
    }

    info!("Barrier event handler ended");
    Ok(())
}
