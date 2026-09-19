# Stage 5 — Daemon and Automation

**Status: complete and verified.**

Turn the pipeline into the plug-and-play behaviour the project set out to build:
plug the tablet in and a monitor appears, unplug it and the monitor disappears.

## Device detection: `adb track-devices`, not udev

The ADB server exposes `host:track-devices`, which streams a fresh device list
every time one changes. The daemon holds that connection open and blocks on it.

This was chosen over udev for a concrete reason found in Stage 0: **the tablet's
USB product ID is not stable.** It moved `2717:ff40` → `2717:ff48` simply from
enabling USB debugging, and changing the USB mode moves it again. A udev rule
matching on product ID would silently stop firing.

`track-devices` matches on ADB's own notion of device state, which is what
actually determines whether we can forward to it. It also distinguishes
`device` from `unauthorized` and `offline` — states that look connected but
reject every request.

Cost of holding the connection: nothing measurable. The daemon is blocked in a
read.

### Protocol

ADB frames every message as a 4-digit hex length followed by the payload:

```
-> 0012host:track-devices
<- OKAY
<- 0016ABCD1234<TAB>device<LF>      (repeated on every change)
```

Implemented directly against port 5037 in `tracker.rs` rather than shelling out
to `adb devices` on a timer — no polling latency, no wakeups.

## Lifecycle

Everything the daemon creates is RAII-scoped, so a session cleans up however it
ends — normally, by error, or by signal:

| Resource | Guard | Removed by |
|---|---|---|
| Hyprland virtual output | `VirtualOutput` | `Drop` → `hyprctl output remove` |
| `adb forward` rule | `adb::Forward` | `Drop` → `adb forward --remove` |
| Encoder / capture session | `Encoder`, `Capture` | `Drop` |
| Tablet app | explicit | `am force-stop` at session end |

`SIGTERM` and `SIGINT` set a shutdown flag that the capture loop checks, so
stopping the service unwinds those guards rather than killing the process
mid-session. **This matters:** a hard kill leaves Hyprland with a phantom
monitor that has to be removed by hand. The unit sets `TimeoutStopSec=15` to
give the unwind room.

## Verification

### Automatic detection

```
INFO padplay: watching for devices
INFO padplay: device ABCD1234 connected
INFO padplay::session: virtual output padplay at 1920x1200@60
INFO padplay::session: streaming to ABCD1234
```

### Recovery from device loss

Killing the ADB server mid-session is the same failure shape as a physical
unplug: the stream breaks and the tracker's connection dies. Observed:

```
streaming to ABCD1234
removed output padplay
session ended: sending frame: Broken pipe (os error 32)
device tracker lost: reading ADB length; reconnecting
device ABCD1234 connected
streaming to ABCD1234            <- resumed with no intervention
removed output padplay
session ended cleanly
shutting down
```

After `SIGTERM`:

```
monitors with padplay: 0
adb forwards: []
```

No residue.

> Physical unplug/replug was not tested here — it needs hands on the cable.
> The failure path is identical (device disappears from `track-devices`, socket
> breaks), and that path is verified, but the physical case is worth confirming
> once.

### Steady state

Three consecutive 8-second runs under identical conditions:

| Run | min | median |
|---|---|---|
| 1 | 19.93 ms | 23.81 ms |
| 2 | 19.51 ms | 23.81 ms |
| 3 | 19.90 ms | 23.76 ms |

Reproducible to within 0.05 ms. An earlier run measured 33.4 ms — that was a
**cold-start outlier**, taken immediately after launching the content window,
and is not representative. Recording it here rather than quietly keeping the
better number: the honest figure is ~23.8 ms with occasional cold-start
excursions.

Tablet panel confirmed at **144 Hz** during streaming.

## Installing

```bash
./install.sh
```

Nothing needs root and nothing is written outside `$HOME`:

- `~/.local/bin/padplay`
- `~/.config/systemd/user/padplay.service`

The installer deliberately **does not build the APK** — that needs the Android
SDK, and a stale APK is worse than an absent one. It checks whether the app is
installed and prints the commands if not.

Start on login:

```bash
systemctl --user enable --now padplay.service
journalctl --user -u padplay -f
```

The unit is `PartOf=graphical-session.target`, so it starts with the session and
stops with it. It needs a running compositor — `hyprctl` has to find the
instance, and the daemon connects to Wayland as a capture client.

## Usage

```
padplay                 watch for the tablet; stream whenever plugged in
padplay --once          stream one session, then exit
padplay --seconds 15    stop after 15 s, print latency statistics
padplay --stats         print latency statistics on exit
padplay --max-width 2400        cap the auto-detected width
padplay --native                stream the tablet's full panel resolution
padplay --width 2400 --height 1500   pin an explicit size (both required)
padplay --bitrate 30000
```

## Code

```
crates/daemon/
  src/main.rs         daemon loop, CLI, signal handling
  src/tracker.rs      ADB host:track-devices client
  src/session.rs      one streaming session, RAII-scoped
  src/output.rs       virtual output guard, per-compositor
  src/usage.txt
systemd/padplay.service
install.sh
```
