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

/// Type alias for release event sender
type ReleaseSender = tokio::sync::mpsc::UnboundedSender<(Option<u32>, Option<(f64, f64)>)>;

/// Typestate marker for a server that has not yet connected to the portal
pub struct NotConnected;

/// Typestate representing a server that has successfully connected to the portal
pub struct PortalConnected {
    portal_session: portal::Session,
    ei_context: ei::EiContext,
    barrier_map: Arc<RwLock<HashMap<u32, String>>>,
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
        let (portal_session, ei_context, barrier_map, desktop_bounds) =
            portal::connect_input_capture(&self.client_configs).await?;

        Ok(Server {
            port: self.port,
            client_configs: self.client_configs,
            state: PortalConnected {
                portal_session,
                ei_context,
                barrier_map,
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
        let barrier_map = self.state.barrier_map;
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
        let (_owned_fd, activated_rx, control_tx, release_tx) = portal_session.split();

        // Track whether portal has been enabled
        let portal_enabled = Arc::new(tokio::sync::RwLock::new(false));

        // Create a channel to notify barrier task about client disconnections
        let (disconnect_tx, disconnect_rx) = tokio::sync::mpsc::unbounded_channel::<String>();

        // Clone server for the barrier event handler
        let barrier_server = Arc::clone(&server);

        // Spawn a task to handle barrier events
        let barrier_task = tokio::spawn(async move {
            // Keep _owned_fd alive for the lifetime of the task
            let _keep_alive = _owned_fd;
            if let Err(e) = handle_barrier_events(
                barrier_server,
                activated_rx,
                ei_context,
                barrier_map,
                desktop_bounds,
                release_tx,
                disconnect_rx,
            )
            .await
            {
                warn!("Barrier event handler error: {}", e);
            }
        });

        // Main event loop - handle server events
        loop {
            match server.recv_event().await {
                Ok(schengen::server::ServerEvent::ClientConnected {
                    client_id,
                    name,
                    width,
                    height,
                }) => {
                    info!(
                        "✓ Client '{}' ({:?}) connected ({}x{})",
                        name, client_id, width, height
                    );

                    let connected = server.clients().await;
                    debug!("  Currently connected: {} client(s)", connected.len());
                    for client in &connected {
                        debug!("    - {} (ID: {:?})", client.name(), client.id());
                    }

                    // Enable portal on first client connection
                    let mut enabled = portal_enabled.write().await;
                    if !*enabled {
                        if let Err(e) = control_tx.send(portal::PortalControl::Enable) {
                            warn!("Failed to send enable command to portal: {}", e);
                        } else {
                            debug!("First client connected, enabling InputCapture portal");
                            *enabled = true;
                        }
                    }
                }

                Ok(schengen::server::ServerEvent::ClientDisconnected { client_id, name }) => {
                    info!("✗ Client '{}' ({:?}) disconnected", name, client_id);
                    // Notify barrier task about the disconnection
                    if let Err(e) = disconnect_tx.send(name.clone()) {
                        warn!(
                            "Failed to notify barrier task about client '{}' disconnection: {}",
                            name, e
                        );
                    }

                    // Disable portal if this was the last client
                    let connected = server.clients().await;
                    if connected.is_empty() {
                        let mut enabled = portal_enabled.write().await;
                        if *enabled {
                            if let Err(e) = control_tx.send(portal::PortalControl::Disable) {
                                warn!("Failed to send disable command to portal: {}", e);
                            } else {
                                info!("Last client disconnected, disabling InputCapture portal");
                                *enabled = false;
                            }
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
                    info!(
                        "ℹ️  Client {:?} updated dimensions: {}x{}",
                        client_id, width, height
                    );
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

/// Handle barrier events and forward input to clients
///
/// This function monitors the portal for activated events (when the pointer
/// crosses a barrier at a screen edge) and forwards the appropriate input
/// events to clients using the schengen server API.
///
/// # Arguments
///
/// * `server` - The schengen server for sending messages to clients
/// * `activated_rx` - Receiver for activated events from the InputCapture portal
/// * `ei_context` - The EI context for receiving input events
/// * `barrier_map` - Map of barrier IDs to client names
/// * `desktop_bounds` - Desktop bounds for checking if pointer is back on server
/// * `release_tx` - Sender to release the InputCapture session
/// * `disconnect_rx` - Receiver for client disconnection notifications
///
/// # Errors
///
/// Returns an error if receiving events fails
async fn handle_barrier_events(
    server: Arc<schengen::server::Server>,
    mut activated_rx: tokio::sync::mpsc::UnboundedReceiver<portal::ActivatedEvent>,
    mut ei_context: ei::EiContext,
    barrier_map: Arc<RwLock<HashMap<u32, String>>>,
    desktop_bounds: Arc<RwLock<portal::DesktopBounds>>,
    release_tx: ReleaseSender,
    mut disconnect_rx: tokio::sync::mpsc::UnboundedReceiver<String>,
) -> Result<()> {
    info!("Starting barrier event handler");

    // Track the currently active client (the one receiving input)
    let mut active_client_id: Option<schengen::server::ClientId> = None;

    // Track the current activation ID for releasing
    let mut current_activation_id: Option<u32> = None;

    // Track the current cursor position (in absolute desktop coordinates)
    let mut cursor_x: f64 = 3072.0; // Center of typical multi-monitor desktop
    let mut cursor_y: f64 = 864.0;

    info!(
        "Tracking cursor position starting at ({:.2}, {:.2})",
        cursor_x, cursor_y
    );

    loop {
        tokio::select! {
            // Handle barrier activation events
            Some(activated_event) = activated_rx.recv() => {
                info!(
                    "Barrier activated: id={}, cursor=({:.2}, {:.2})",
                    activated_event.barrier_id, activated_event.cursor_x, activated_event.cursor_y
                );

                // Look up which client this barrier corresponds to
                let barrier_lookup = barrier_map.read().await;
                let client_name = match barrier_lookup.get(&activated_event.barrier_id) {
                    Some(name) => name.clone(),
                    None => {
                        warn!(
                            "Unknown barrier ID: {} (no client mapping found)",
                            activated_event.barrier_id
                        );
                        continue;
                    }
                };
                drop(barrier_lookup);

                info!("Barrier {} corresponds to client '{}'", activated_event.barrier_id, client_name);

                // Find the client ID from the connected clients list
                let connected = server.clients().await;
                let client = match connected.iter().find(|c| c.name() == client_name) {
                    Some(c) => c,
                    None => {
                        warn!("Client '{}' not connected, ignoring barrier activation", client_name);
                        continue;
                    }
                };

                let client_id = client.id();
                let client_width = client.width;
                let client_height = client.height;

                // Send cursor entered message to the client
                // For now, send cursor position as center of client screen
                // TODO: Map desktop coordinates to client coordinates based on barrier position
                let enter_x = (client_width / 2) as i16;
                let enter_y = (client_height / 2) as i16;

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
                active_client_id = Some(client_id);
                current_activation_id = Some(activated_event.activation_id);

                // Update cursor position to the activation point
                cursor_x = activated_event.cursor_x;
                cursor_y = activated_event.cursor_y;

                // Store client dimensions for coordinate mapping
                // Store as Option<(width, height)> keyed by client_id
                // For now, we'll read this from connected_clients when needed

                info!("Client '{}' is now active and receiving input (activation_id={}, cursor=({:.2}, {:.2}))",
                    client_name, activated_event.activation_id, cursor_x, cursor_y);
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

                let Some(client_id) = active_client_id else {
                    // No active client, discard events
                    debug!("ei: No active client, discarding {} events", events.len());
                    continue;
                };

                // Get client dimensions for coordinate mapping
                let connected = server.clients().await;
                let client_info = connected.iter().find(|c| c.id() == client_id);
                let (client_width, client_height) = if let Some(client) = client_info {
                    (client.width as f64, client.height as f64)
                } else {
                    warn!("Active client not found in connected clients list");
                    continue;
                };

                // Process events and check if pointer returned to server
                let mut pointer_back_on_server = false;
                let mut release_cursor_pos: Option<(f64, f64)> = None;
                let mut client_disconnected = false;
                let mut last_mouse_pos: Option<(f64, f64)> = None;
                let mut needs_mouse_update = false;

                let bounds = desktop_bounds.read().await;

                for event in events {
                    match event {
                        ei::InputEvent::PointerAbsolute { x, y } => {
                            cursor_x = x;
                            cursor_y = y;
                            debug!("ei: PointerAbsolute event: ({:.2}, {:.2})", x, y);

                            // Check if pointer is within desktop bounds
                            if cursor_x >= bounds.min_x as f64 && cursor_x < bounds.max_x as f64 &&
                               cursor_y >= bounds.min_y as f64 && cursor_y < bounds.max_y as f64 {
                                info!("Pointer (abs) at ({:.2}, {:.2}) is back within desktop bounds", cursor_x, cursor_y);
                                pointer_back_on_server = true;
                                release_cursor_pos = Some((cursor_x, cursor_y));
                                break;
                            }

                            // Coalesce mouse moves - just track the latest position
                            last_mouse_pos = Some((cursor_x, cursor_y));
                            needs_mouse_update = true;
                        }
                        ei::InputEvent::PointerRelative { dx, dy } => {
                            cursor_x += dx;
                            cursor_y += dy;
                            debug!("ei: PointerRelative event: delta=({:.2}, {:.2}), cursor=({:.2}, {:.2})", dx, dy, cursor_x, cursor_y);

                            // Check if pointer is within desktop bounds
                            if cursor_x >= bounds.min_x as f64 && cursor_x < bounds.max_x as f64 &&
                               cursor_y >= bounds.min_y as f64 && cursor_y < bounds.max_y as f64 {
                                info!("Pointer (rel) at ({:.2}, {:.2}) is back within desktop bounds", cursor_x, cursor_y);
                                pointer_back_on_server = true;
                                release_cursor_pos = Some((cursor_x, cursor_y));
                                break;
                            }

                            // Coalesce mouse moves - just track the latest position
                            last_mouse_pos = Some((cursor_x, cursor_y));
                            needs_mouse_update = true;
                        }
                        ei::InputEvent::Button { button, is_press } => {
                            if !client_disconnected {
                                debug!("Forwarding button {} ({}) to client", button, if is_press { "press" } else { "release" });
                                let result = if is_press {
                                    server.send_mouse_button_down(client_id, button as u8).await
                                } else {
                                    server.send_mouse_button_up(client_id, button as u8).await
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
                                    server.send_key_down(client_id, keysym as u16, mask, button).await
                                } else {
                                    server.send_key_up(client_id, keysym as u16, mask, button).await
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
                                if let Err(e) = server.send_mouse_wheel(client_id, xdelta, ydelta).await {
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
                    && let Some((final_x, final_y)) = last_mouse_pos
                {
                    let client_x = (final_x.rem_euclid(client_width)) as i16;
                    let client_y = (final_y.rem_euclid(client_height)) as i16;
                    debug!("Sending coalesced mouse move to client: ({}, {}) [from desktop ({:.2}, {:.2})]",
                        client_x, client_y, final_x, final_y);
                    if let Err(e) = server.send_mouse_move(client_id, client_x, client_y).await {
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
                        if let Err(e) = release_tx.send((Some(activation_id), None)) {
                            warn!("Failed to send release request: {:?}", e);
                        }
                    }

                    // Clear active client and activation ID
                    active_client_id = None;
                    current_activation_id = None;
                    cursor_x = 0.0;
                    cursor_y = 0.0;
                } else if pointer_back_on_server {
                    // Send cursor left message to client
                    if let Err(e) = server.send_cursor_left(client_id).await {
                        warn!("Failed to send cursor left to client: {}", e);
                    }

                    // Release the InputCapture session
                    if let Some(activation_id) = current_activation_id {
                        let cursor_str = release_cursor_pos.map(|(x, y)| format!("({:.2}, {:.2})", x, y))
                            .unwrap_or_else(|| "None".to_string());
                        info!("Releasing InputCapture (activation_id={}, cursor={})", activation_id, cursor_str);
                        if let Err(e) = release_tx.send((Some(activation_id), release_cursor_pos)) {
                            warn!("Failed to send release request: {:?}", e);
                        }
                    } else {
                        warn!("Cannot release InputCapture: no activation_id stored");
                    }

                    // Clear active client and activation ID
                    active_client_id = None;
                    current_activation_id = None;
                    cursor_x = 0.0;
                    cursor_y = 0.0;
                    info!("Input focus returned to server");
                }
            }

            // Handle client disconnection notifications
            Some(disconnected_client_name) = disconnect_rx.recv() => {
                debug!("Received disconnect notification for client '{}'", disconnected_client_name);

                // Check if this client is currently active (has input focus)
                if let Some(client_id) = active_client_id {
                    // Look up the active client's name
                    let connected = server.clients().await;
                    let active_client_name = connected.iter()
                        .find(|c| c.id() == client_id)
                        .map(|c| c.name());

                    // If the disconnected client was the active one, release InputCapture
                    if active_client_name == Some(disconnected_client_name.as_str()) {
                        warn!("Active client '{}' disconnected, releasing InputCapture", disconnected_client_name);

                        // Release the InputCapture session
                        if let Some(activation_id) = current_activation_id {
                            info!("Releasing InputCapture due to active client disconnect (activation_id={})", activation_id);
                            if let Err(e) = release_tx.send((Some(activation_id), None)) {
                                warn!("Failed to send release request: {:?}", e);
                            }
                        }

                        // Clear active client and activation ID
                        active_client_id = None;
                        current_activation_id = None;
                        cursor_x = 0.0;
                        cursor_y = 0.0;
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
