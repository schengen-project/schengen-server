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
use crate::portal;

const DEFAULT_PORT: u16 = 24801;

/// Clamp cursor position to client bounds based on entry position
fn clamp_cursor_to_client(
    mut cursor: portal::Point,
    position: crate::config::Position,
    coord_offset: portal::Point,
    client_size: portal::Size,
) -> portal::Point {
    let client_min_x = coord_offset.x;
    let client_max_x = coord_offset.x + client_size.width - 1.0;
    let client_min_y = coord_offset.y;
    let client_max_y = coord_offset.y + client_size.height - 1.0;

    match position {
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

    cursor
}

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
    /// Input events from EIS
    /// Pointer absolute motion
    PointerAbsolute { x: f64, y: f64 },
    /// Pointer relative motion
    PointerRelative { dx: f64, dy: f64 },
    /// Mouse button press/release
    Button { button: u32, is_press: bool },
    /// Keyboard key press/release
    Key {
        keysym: u32,
        is_press: bool,
        mask: u16,
        button: u16,
    },
    /// Scroll wheel
    Scroll { x: f64, y: f64 },
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

    /// Attach a connected portal to the server
    ///
    /// # Arguments
    ///
    /// * `portal` - The connected portal instance
    pub fn with_portal(
        self,
        portal: portal::InputCapturePortal,
    ) -> Server<portal::InputCapturePortal> {
        Server {
            port: self.port,
            client_configs: self.client_configs,
            state: portal,
        }
    }
}

impl Server<portal::InputCapturePortal> {
    /// Run the TCP server using schengen::server API
    ///
    /// This function builds a schengen server with the configured clients and manages
    /// the integration with the InputCapture portal for barrier-based input forwarding.
    ///
    /// # Errors
    ///
    /// Returns an error if the server fails to start or encounters a fatal error
    pub async fn run(self) -> Result<()> {
        let input_capture = self.state;
        let desktop_bounds = Arc::clone(&input_capture.desktop_bounds);

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

        // Clone the event sender for the main server task
        let event_tx = input_capture.event_tx.clone();

        // Clone server for the barrier event handler
        let barrier_server = Arc::clone(&server);

        // Spawn a task to handle barrier events
        let barrier_task = tokio::spawn(async move {
            handle_barrier_events(barrier_server, input_capture, desktop_bounds).await
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
/// This function monitors events from the portal (barrier activations, client disconnects, input events)
/// and forwards the appropriate input events to clients using the schengen server API.
///
/// # Arguments
///
/// * `server` - The schengen server for sending messages to clients
/// * `input_capture` - InputCapture portal containing event channels
/// * `desktop_bounds` - Desktop bounds for checking if pointer is back on server
///
/// # Errors
///
/// Returns an error if receiving events fails
async fn handle_barrier_events(
    server: Arc<schengen::server::Server>,
    mut input_capture: portal::InputCapturePortal,
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
            // Handle all events from portal (including input events)
            Some(event) = input_capture.event_rx.recv() => {
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

                        // Guard against division by zero
                        if desktop_width <= 0.0 || desktop_height <= 0.0 {
                            warn!("Invalid desktop dimensions ({}x{}), ignoring barrier activation", desktop_width, desktop_height);
                            continue;
                        }

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
                                    if let Err(e) = input_capture.event_tx.send(Event::ReleaseCapture {
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

                    Event::PointerAbsolute { x, y } => {
                        cursor = portal::Point::new(x, y);
                        debug!("ei: PointerAbsolute event: ({:.2}, {:.2})", x, y);

                        let Some(ref client) = active_client else {
                            debug!("ei: No active client, discarding pointer absolute event");
                            continue;
                        };

                        // Clamp cursor to client bounds based on client position
                        cursor = clamp_cursor_to_client(cursor, client.position, client.coord_offset, client.size);

                        // Check if pointer is within desktop bounds
                        let bounds = desktop_bounds.read().await;
                        if cursor.x >= bounds.min_x as f64 && cursor.x < bounds.max_x as f64 &&
                           cursor.y >= bounds.min_y as f64 && cursor.y < bounds.max_y as f64 {
                            drop(bounds);
                            info!("Pointer (abs) at ({:.2}, {:.2}) is back within desktop bounds", cursor.x, cursor.y);

                            // Send cursor left message to client
                            if let Err(e) = server.send_cursor_left(client.id).await {
                                warn!("Failed to send cursor left to client: {}", e);
                            }

                            // Release the InputCapture session
                            if let Some(activation_id) = current_activation_id {
                                info!("Releasing InputCapture (activation_id={}, cursor=({:.2}, {:.2}))", activation_id, cursor.x, cursor.y);
                                if let Err(e) = input_capture.event_tx.send(Event::ReleaseCapture {
                                    activation_id: Some(activation_id),
                                    cursor: Some(cursor),
                                }) {
                                    warn!("Failed to send release request: {:?}", e);
                                }
                            }

                            // Clear active client and activation ID
                            active_client = None;
                            current_activation_id = None;
                            cursor = portal::Point::new(0.0, 0.0);
                            info!("Input focus returned to server");
                        } else {
                            drop(bounds);
                            // Map desktop coordinates to client coordinates using the offset
                            let client_x = ((cursor.x - client.coord_offset.x).clamp(0.0, client.size.width - 1.0)) as i16;
                            let client_y = ((cursor.y - client.coord_offset.y).clamp(0.0, client.size.height - 1.0)) as i16;
                            debug!("Sending mouse move to client: ({}, {}) [from desktop ({:.2}, {:.2})]",
                                client_x, client_y, cursor.x, cursor.y);
                            if let Err(e) = server.send_mouse_move(client.id, client_x, client_y).await {
                                warn!("Failed to send mouse move: {}", e);
                                // Client disconnected, clear active client and release portal
                                if let Some(activation_id) = current_activation_id {
                                    info!("Releasing InputCapture due to client disconnect (activation_id={})", activation_id);
                                    let _ = input_capture.event_tx.send(Event::ReleaseCapture {
                                        activation_id: Some(activation_id),
                                        cursor: None,
                                    });
                                }
                                active_client = None;
                                current_activation_id = None;
                                cursor = portal::Point::new(0.0, 0.0);
                            }
                        }
                    }

                    Event::PointerRelative { dx, dy } => {
                        cursor.x += dx;
                        cursor.y += dy;
                        debug!("ei: PointerRelative event: delta=({:.2}, {:.2}), cursor=({:.2}, {:.2})", dx, dy, cursor.x, cursor.y);

                        let Some(ref client) = active_client else {
                            debug!("ei: No active client, discarding pointer relative event");
                            continue;
                        };

                        // Clamp cursor to client bounds based on client position
                        cursor = clamp_cursor_to_client(cursor, client.position, client.coord_offset, client.size);

                        // Check if pointer is within desktop bounds
                        let bounds = desktop_bounds.read().await;
                        if cursor.x >= bounds.min_x as f64 && cursor.x < bounds.max_x as f64 &&
                           cursor.y >= bounds.min_y as f64 && cursor.y < bounds.max_y as f64 {
                            drop(bounds);
                            info!("Pointer (rel) at ({:.2}, {:.2}) is back within desktop bounds", cursor.x, cursor.y);

                            // Send cursor left message to client
                            if let Err(e) = server.send_cursor_left(client.id).await {
                                warn!("Failed to send cursor left to client: {}", e);
                            }

                            // Release the InputCapture session
                            if let Some(activation_id) = current_activation_id {
                                info!("Releasing InputCapture (activation_id={}, cursor=({:.2}, {:.2}))", activation_id, cursor.x, cursor.y);
                                if let Err(e) = input_capture.event_tx.send(Event::ReleaseCapture {
                                    activation_id: Some(activation_id),
                                    cursor: Some(cursor),
                                }) {
                                    warn!("Failed to send release request: {:?}", e);
                                }
                            }

                            // Clear active client and activation ID
                            active_client = None;
                            current_activation_id = None;
                            cursor = portal::Point::new(0.0, 0.0);
                            info!("Input focus returned to server");
                        } else {
                            drop(bounds);
                            // Map desktop coordinates to client coordinates using the offset
                            let client_x = ((cursor.x - client.coord_offset.x).clamp(0.0, client.size.width - 1.0)) as i16;
                            let client_y = ((cursor.y - client.coord_offset.y).clamp(0.0, client.size.height - 1.0)) as i16;
                            debug!("Sending mouse move to client: ({}, {}) [from desktop ({:.2}, {:.2})]",
                                client_x, client_y, cursor.x, cursor.y);
                            if let Err(e) = server.send_mouse_move(client.id, client_x, client_y).await {
                                warn!("Failed to send mouse move: {}", e);
                                // Client disconnected, clear active client and release portal
                                if let Some(activation_id) = current_activation_id {
                                    info!("Releasing InputCapture due to client disconnect (activation_id={})", activation_id);
                                    let _ = input_capture.event_tx.send(Event::ReleaseCapture {
                                        activation_id: Some(activation_id),
                                        cursor: None,
                                    });
                                }
                                active_client = None;
                                current_activation_id = None;
                                cursor = portal::Point::new(0.0, 0.0);
                            }
                        }
                    }

                    Event::Button { button, is_press } => {
                        let Some(ref client) = active_client else {
                            debug!("ei: No active client, discarding button event");
                            continue;
                        };

                        debug!("Forwarding button {} ({}) to client", button, if is_press { "press" } else { "release" });
                        let result = if is_press {
                            server.send_mouse_button_down(client.id, button as u8).await
                        } else {
                            server.send_mouse_button_up(client.id, button as u8).await
                        };
                        if let Err(e) = result {
                            warn!("Failed to send mouse button: {}", e);
                            // Client disconnected, clear active client and release portal
                            if let Some(activation_id) = current_activation_id {
                                info!("Releasing InputCapture due to client disconnect (activation_id={})", activation_id);
                                let _ = input_capture.event_tx.send(Event::ReleaseCapture {
                                    activation_id: Some(activation_id),
                                    cursor: None,
                                });
                            }
                            active_client = None;
                            current_activation_id = None;
                            cursor = portal::Point::new(0.0, 0.0);
                        }
                    }

                    Event::Key { keysym, is_press, mask, button } => {
                        let Some(ref client) = active_client else {
                            debug!("ei: No active client, discarding key event");
                            continue;
                        };

                        debug!("Forwarding keysym 0x{:x} ({}) to client", keysym, if is_press { "press" } else { "release" });
                        let result = if is_press {
                            server.send_key_down(client.id, keysym as u16, mask, button).await
                        } else {
                            server.send_key_up(client.id, keysym as u16, mask, button).await
                        };
                        if let Err(e) = result {
                            warn!("Failed to send key: {}", e);
                            // Client disconnected, clear active client and release portal
                            if let Some(activation_id) = current_activation_id {
                                info!("Releasing InputCapture due to client disconnect (activation_id={})", activation_id);
                                let _ = input_capture.event_tx.send(Event::ReleaseCapture {
                                    activation_id: Some(activation_id),
                                    cursor: None,
                                });
                            }
                            active_client = None;
                            current_activation_id = None;
                            cursor = portal::Point::new(0.0, 0.0);
                        }
                    }

                    Event::Scroll { x, y } => {
                        let Some(ref client) = active_client else {
                            debug!("ei: No active client, discarding scroll event");
                            continue;
                        };

                        debug!("Forwarding scroll ({}, {}) to client", x, y);
                        let xdelta = (x * 120.0) as i16;
                        let ydelta = (y * 120.0) as i16;
                        if let Err(e) = server.send_mouse_wheel(client.id, xdelta, ydelta).await {
                            warn!("Failed to send mouse wheel: {}", e);
                            // Client disconnected, clear active client and release portal
                            if let Some(activation_id) = current_activation_id {
                                info!("Releasing InputCapture due to client disconnect (activation_id={})", activation_id);
                                let _ = input_capture.event_tx.send(Event::ReleaseCapture {
                                    activation_id: Some(activation_id),
                                    cursor: None,
                                });
                            }
                            active_client = None;
                            current_activation_id = None;
                            cursor = portal::Point::new(0.0, 0.0);
                        }
                    }

                    _ => {
                        warn!("Unexpected event in barrier handler: {:?}", event);
                    }
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
