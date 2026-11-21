# Try Rust Remote Desktop on Cutefish / Debian 13

This is a prototype for controlled testing. It has passed local automated tests, a Zorin X11 screen capture/codec check, and a native window smoke test. A real two-laptop session, an Internet session, and this exact Cutefish installation have not been verified yet.

## Start with Cutefish as the viewer

The current implementation can display and control a remote X11/Windows host from its native viewer. It cannot capture or control a **Wayland host**. Current Cutefish uses a Wayland compositor, so the first useful test is:

**Zorin X11 shares its screen → Cutefish connects as the viewer.**

If your Cutefish machine uses an older X11 desktop, hosting is implemented too, but still needs a real session test. Check your actual session rather than assuming from the OS name:

```bash
echo "$XDG_SESSION_TYPE"
uname -m
```

This bundle contains an **x86_64 Linux GNU** binary. It does not run on ARM or 32-bit machines. Rust is not required on the test laptop. The build uses Zorin's glibc 2.39 toolchain/runtime; Debian 13 ships a newer glibc, so the base runtime is expected to be compatible. GPU/display-library compatibility still needs testing on the laptop.

Upstream references: [Cutefish compositor](https://github.com/cutefishos/cutefish-compositor), [Cutefish on Debian 13](https://github.com/cutefishos/getting-started), [Debian 13 libc6](https://packages.debian.org/trixie/libc6).

## Copy and launch

Copy `rust-remote-desktop-0.1.0-linux-x86_64.tar.gz` to the laptop using a USB drive, file transfer, or an existing SSH connection. Nothing has been uploaded to a public release page.

If you place it in Downloads:

```bash
cd ~/Downloads
tar -xzf rust-remote-desktop-0.1.0-linux-x86_64.tar.gz
cd rust-remote-desktop-0.1.0-linux-x86_64
sha256sum -c SHA256SUMS
./check-system.sh
./rust-remote-desktop
```

Run the application as your normal desktop user, without sudo. `check-system.sh` is read-only. It reports architecture, session type and startup-linked libraries; it does not install anything, start sharing or change system settings. Wayland viewer support is compiled in, but this specific Cutefish system remains untested.

If launch fails, keep the terminal error. For a missing shared library, identify that exact library before installing its Debian runtime package. A successful `ldd` check alone does not validate graphics drivers or libraries loaded later by the GUI.

## Test both machines on the same LAN first

1. Open the application on Zorin. Leave **Use a self-hosted relay** unchecked on both machines.
2. On Zorin, set **Listen address** to `0.0.0.0:47832`, then click **Start sharing**. This explicitly listens on the Zorin machine's IPv4 interfaces.
3. Find Zorin's LAN address with `hostname -I`. Choose the address on the network shared with the laptop. Privately copy the app's generated pairing secret to the Cutefish viewer.
4. On Cutefish, enter the Zorin address followed by the port, for example `192.168.1.20:47832`. Do not enter `0.0.0.0` or `127.0.0.1` as the other computer's address. Those are not the Zorin machine's reachable LAN address.
5. Enter the pairing secret, choose whether to request keyboard/mouse control, and click **Connect** within 60 seconds.
6. Accept **Allow viewing** on Zorin first. Confirm the image updates. Then disconnect and repeat with control requested and **Allow mouse and keyboard** accepted. Test in a blank document or another harmless test window.
7. Verify Escape releases control, viewer disconnect releases held inputs, and **Stop sharing** ends the host session.

If the connection fails, confirm the IP/port, sharing state and firewall. Guest Wi-Fi or client isolation may prevent two devices from reaching one another even at the same location. The project does not edit network or firewall settings automatically.

If Cutefish reports `wayland`, its **Start sharing** path cannot currently provide its desktop to Zorin. XWayland availability does not make a whole Wayland desktop capturable by this X11 adapter. That requires a new Wayland capture/input implementation.

## Different Wi-Fi / Internet

Different Wi-Fi names are not inherently a problem; the devices must have a usable network route. Direct connections work only when the viewer can reach the host address/port, for example on a shared routed network or through an existing VPN.

For separate Internet connections without direct reachability, both machines need our relay at an address reachable from both. The project includes the relay program but **no relay is deployed for you**. Binding a laptop to `0.0.0.0` alone does not make it reachable across the Internet.

On a relay machine you control, run:

```bash
./rust-remote-desktop relay --bind 0.0.0.0:47833
```

Allow/reach TCP 47833 using that machine's network configuration. In both GUI apps, select **Use a self-hosted relay** and enter the same reachable relay hostname/IP and port. The host starts sharing, the viewer uses its pairing secret, and the host approves the session as before. Waiting peers expire after 60 seconds. The relay forwards encrypted bytes; screen/input payloads are encrypted between endpoints. This basic relay has not been verified over a real WAN or hardened as a public multi-user service.

## What readiness means here

Implemented and locally checked: native UI startup, X11 capture, JPEG encoding/decoding, authenticated encrypted connections, consent/permission logic, input cleanup logic and relay forwarding. Windows code has passed target checks; actual Windows capture/control remains untested. Automated input tests use synthetic receivers and do not establish real keyboard/mouse behavior on the laptop.

Not delivered yet: Wayland hosting, an installer/update service, automatic device IDs/NAT traversal, hardware video encoding, clipboard/file/audio transfer, independent security review and two-device/WAN reliability verification.
