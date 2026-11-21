# Rust Remote Desktop

A working prototype for attended screen sharing and remote control on **Windows and Linux X11**. The project includes an AnyDesk investigation and an independent implementation in Rust.

The application has a native host/viewer window, encrypted direct connections, an optional self-hosted relay, local approval for each session, and separate viewing/control permissions. It does not use AnyDesk's servers, codec, or private protocol.

## Run

With Rust 1.88 or newer installed:

```bash
cargo run --release
```

For the locally prepared Linux test bundle and a Cutefish/Debian 13 walkthrough, see [the Cutefish quickstart](docs/cutefish-quickstart.md). It explains the Wayland host limitation and how to test Cutefish as a viewer without installing Rust.

The same application can share its own screen or connect to another computer. Nothing listens or captures the desktop until you explicitly start a session. The default direct listen address is loopback, `127.0.0.1:47832`.

On Windows, build with a normal Rust Windows toolchain and its required compiler/build tools. The intended Windows desktop versions are Windows 10 and 11. Windows code is cross-checked from Linux; real Windows screen/input behavior still needs a two-machine test.

On Linux, hosting requires a logged-in X11 graphical session and the XTEST extension. This includes the Zorin X11 session used for development. The viewer GUI can run under X11 or Wayland, but Wayland **hosting** requires a future portal/PipeWire adapter. A headless relay does not need a graphical session.

## Direct connection on a LAN

1. Open the app on both computers.
2. On the computer to share, set **Listen address** to its LAN address and port, for example `192.168.1.20:47832`. `0.0.0.0:47832` listens on all IPv4 interfaces when explicitly selected.
3. Click **Start sharing**. Privately share the generated 64-character pairing secret with your viewer.
4. On the viewer, enter the host's LAN address and pairing secret. Optionally select **Request mouse and keyboard control**, then click **Connect**.
5. The host chooses **Allow viewing**, **Allow mouse and keyboard**, or **Decline**. No screen frame is captured or sent before approval. Requests expire after 60 seconds.
6. When control is allowed, click the remote image to focus it. Escape releases the remote keyboard. **Disconnect** or **Stop sharing** ends the session and releases held inputs.

The **Remote shortcuts** buttons send Copy, Cut, Paste, and Windows/Super actions to the host. These act on the host's clipboard; local clipboard contents are not synchronized. Use these buttons for reliable system shortcuts, including Paste when your local clipboard is empty. The GUI framework does not expose the physical Windows/Super modifier, so combinations held with that modifier are not yet forwarded.

Both apps must be able to reach the chosen TCP port. The project does not edit firewall or router settings.

Stopping and restarting the host rotates the pairing secret. It is a high-entropy shared secret, not a short PIN, and is valid for that sharing run. Knowing it authenticates a connection; the host still approves every session separately.

## Connect through your own relay

Both computers can make outbound connections to a relay when they cannot reach one another directly.

Start the relay on a machine you control:

```bash
cargo run --release --no-default-features -- relay --bind 0.0.0.0:47833
```

The command explicitly chooses an externally reachable listen interface. Without `--bind`, the relay listens only on `127.0.0.1:47833`. Press Ctrl+C to stop it.

In both app windows, select **Use a self-hosted relay** and enter the same reachable relay address, such as `relay.example.net:47833`. Start sharing at the host, then use its pairing secret at the viewer. The same consent dialog and viewing/control rules apply. No inbound listener is opened by the host in relay mode.

The relay pairs host/viewer sockets using a domain-separated hash derived from the pairing secret. The endpoints then perform the same Noise handshake through that byte stream. The relay does not receive the secret or decrypt the screen and input payloads. It can observe addresses, room hashes, timing and traffic volume, and it can interrupt a session. The basic relay is bounded but has no account system, billing, or production abuse protection. It is intended for controlled experiments, not an anonymous public relay service.

Waiting peers expire after 60 seconds. If a relay wait expires or a connection fails, start sharing/connect again. Direct/relay selection is manual; automatic NAT traversal and fallback are not implemented.

## What is implemented

| Component | Current behavior |
| --- | --- |
| Application | One native Rust/egui host and viewer window |
| Authentication | Fresh 256-bit pairing secret from OS randomness; authenticated Noise handshake |
| Encryption | Noise NNpsk0, X25519, ChaCha20-Poly1305, BLAKE2s through `snow` |
| Host authorization | Local acceptance for each session; view-only or keyboard/mouse control |
| Linux capture | X11 root canvas, including the selected X11 screen's monitor layout |
| Windows capture | Primary monitor using GDI and a synchronized top-down DIB |
| Input | Mouse movement/buttons/wheel and Latin/navigation/function/modifier keys |
| Frames | JPEG at quality 70; unchanged frames are skipped; host rate capped at about 15 polls/s |
| Connection | Direct TCP or two outbound TCP sockets through our relay |
| Cleanup | Input releases on focus loss, explicit disconnect, network failure or host shutdown |
| Bounds | 8192 per dimension, 32 megapixels, 12 MiB JPEG, 16 MiB encrypted application message |

The initial codec sends complete changed frames. It does not yet send dirty tiles, hardware-encoded video or cursor shapes. Display scaling and image changes can reduce achieved frame rate below the capture cap. No latency, bandwidth, or frame-rate benchmark is claimed.

## Verification

```bash
cargo test --all-targets
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

Check local capture and JPEG round-trip without listening, connecting or injecting input:

```bash
cargo run --no-default-features -- capture-check
```

Check GUI creation and rendering, then close the window automatically:

```bash
cargo run -- --smoke-test
```

Check Windows compilation from a Linux development machine with the Windows target installed:

```bash
cargo check --target x86_64-pc-windows-gnu --all-targets
```

The tests use local loopback connections and synthetic pixels/input receivers. They cover secret mismatch, tampering, truncated/oversized messages, relay pairing and stop cleanup, JPEG bounds, consent decisions, view-only enforcement and held-input release. They do not inject input into your live desktop. `capture-check` explicitly reads the local desktop and prints only dimensions and byte counts.

A real 1920 × 1080 Zorin X11 capture and JPEG encode/decode has passed. Full interactive Windows↔Linux operation still needs a Windows machine and an approved host/viewer session. A Windows typecheck is not runtime validation.

## Investigation and design

- [AnyDesk findings and unresolved questions](research/anydesk-analysis.md)
- [Artifact and source evidence ledger](research/evidence-ledger.json)
- [Our architecture and protocol](docs/architecture.md)

The investigation uses official vendor documentation and static inspection of official Linux/Windows downloads. It identifies X11 capture and XTest input call sites in the Linux executable. The artifacts were not installed or executed. REA's workflow was read, but its runtime could not run with the existing Node versions and absent native analysis engines; ordinary shell observations are labeled accordingly. There are no claimed REA Evidence records.

AnyDesk's DeskRT implementation, private wire protocol, server internals, runtime performance and Windows embedded implementation remain unresolved. This project is a reconstruction of remote-desktop functionality, not an AnyDesk-compatible client.

## Current limits

This is a prototype with working components, not a production-ready replacement for AnyDesk. It has not had an independent security review. It currently has one viewer per host, manual addresses, no persistent device IDs or rendezvous service, no automatic NAT traversal, no Wayland host, no audio/clipboard/file transfer, and no arbitrary Unicode text entry.

The Windows adapter does not elevate privileges, access secure/UAC desktops, log in a locked computer, or capture extra monitors. Remote text follows the host keyboard layout. A host closing a connection releases the inputs tracked by that session; it does not alter unrelated physical keyboard state.

The next practical validation is an attended connection between your Zorin machine and a Windows test machine. After that, the largest performance improvements are XDamage/shared-memory capture and dirty tiles on Linux, Desktop Duplication on Windows, and a replaceable low-latency video encoder.
