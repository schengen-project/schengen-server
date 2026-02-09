# schengen-server

A Synergy protocol-compatible server using the InputCapture portal and libei for
Wayland desktops.

Accepts connections from Synergy clients and forwards keyboard/mouse input
across machines. The server integrates with the XDG Desktop Portal's
InputCapture portal to receive input events when the cursor crosses screen
boundaries, then sends them to the appropriate client over the Synergy protocol.

This utility only supports Linux and only supports compositors that use the
portals. There are no plans to support Xorg/X11-based services and it's very
unlikely that Macos or Windows support will ever appear.

This crate is part of the schengen project:
- `schengen` for the protocol implementation
- `schengen-server` for a synergy-compatible server
- `schengen-client` for a client that can connect to this server
- `schengen-debugger` for a protocol debugger

## Building

Use the Rust documentation for details on cargo.

```bash
cargo build --release
```

The binary will be in `target/release/schengen-server`.

## Usage

Start the server with client positions defined relative to the server or other clients:

```bash
schengen-server --accept laptop:leftof:self --accept desktop:rightof:self
```
j
Position syntax: `name:position:reference` where position is one of `leftof`,
`rightof`, `topof`, `bottomof`. The special name `self` is reserved to refer
to the client machine, clients can be chained in a row with e.g. `--accept
otherlaptop:leftof:laptop`.

The server listens on port 24801 by default.

## Requirements

A monder

- Wayland compositor with InputCapture portal support
- libei
- Synergy-compatible clients configured to connect to this server, use e.g. 
   [deskflow](https://github.com/deskflow/deskflow/) if you don't want to
   use the `schengen-client` that is part of this project.

## License

GPLv3 or later. See the [COPYING](COPYING) file for details.
