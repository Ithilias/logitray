# logitray

A tiny Windows tray app that shows your wireless Logitech mouse's battery level and warns you before it dies — no Logitech G HUB or Options+ required.

## What it does

logitray sits in your system tray and talks to your Logitech wireless mouse directly over the HID++ 2.0 protocol through its USB receiver. It polls the battery in the background, shows the level on the tray icon, and pops a Windows notification when the battery gets low — all without any Logitech software running.

## Features

- Live battery level for your wireless Logitech mouse, right in the system tray
- Two view modes: a color-coded **battery glyph**, or the **percentage as text** — switch any time from the menu
- Color coding: green (healthy), orange (low), red (critical), blue (charging)
- Hover the icon for the device name and exact percentage
- Automatic low-battery notifications, with a cooldown so they don't spam you
- Multiple devices: pick which one the tray follows
- Manual "Refresh now" any time
- Optional autostart with Windows
- Resilient polling: keeps showing the last reading through brief dropouts (e.g. the mouse sleeping) and recovers quickly

## Install & run

1. Download `logitray.exe` from the [latest release](https://github.com/Ithilias/logitray/releases/latest).
2. Put it anywhere you like and double-click it — it appears in the system tray (check the `^` overflow if you don't see it).
3. (Optional) From a terminal, run `logitray.exe --once` to print the current battery level and confirm your device is detected.

No installer, no admin rights, no Logitech software needed.

## In the tray

Right-click the icon for the menu:

- **Status line** — the selected device and its battery (e.g. `G502 X Plus: 76%`)
- **Select Device** — choose which device the tray follows when more than one is paired
- **Refresh now** — poll immediately instead of waiting for the next interval
- **Show percentage as text** — toggle between the battery-glyph icon and the percentage-number icon
- **Start at login** — register/unregister autostart with Windows
- **Exit**

Icon colors: **green** ≥ 36%, **orange** 16–35%, **red** ≤ 15%, **blue** while charging.

## Low-battery alerts

When the selected device drops to or below the threshold (15% by default), logitray shows a Windows toast. To avoid nagging, it waits out a cooldown (120 minutes by default) before alerting again, and tracks each device separately.

## Supported devices

Any Logitech wireless device that speaks **HID++ 2.0** through a Logitech **Unifying**, **LIGHTSPEED**, or **Bolt** USB receiver. Devices report their own marketing name over HID++, so there's no large hardcoded device database to maintain — the name you see is the one your mouse reports. Battery is read via feature `0x1000`, `0x1001` (voltage, converted with a lookup table), or `0x1004`, whichever the device supports.

Tested against a **G502 X PLUS** over a LIGHTSPEED receiver.

## Settings

Configuration lives in `%APPDATA%\logitray\config.toml` (created on first run):

| Key | Default | Description |
| --- | --- | --- |
| `poll_interval_seconds` | `60` | How often to poll the battery |
| `low_battery_threshold` | `15` | Percent at/below which a low-battery toast fires |
| `low_battery_cooldown_minutes` | `120` | Minimum time between repeat alerts per device |
| `selected_device_id` | `""` | Which device the tray follows (set via the menu) |
| `autostart` | `true` | Start logitray when you log in |
| `log_level` | `"info"` | Log verbosity (`error`/`warn`/`info`/`debug`/`trace`) |
| `view_mode` | `"icon"` | Tray display: `icon` (battery glyph) or `text` (percentage) |

Logs are written to `%APPDATA%\logitray\logitray.log` (rotated, last few kept).

## Limitations

- Windows only.
- The device must be connected through a Logitech USB receiver — Bluetooth-only connections are not supported.
- Requires an HID++ 2.0 device (essentially all modern Logitech wireless mice/keyboards).
- For devices reporting voltage (feature `0x1001`), the percentage is an estimate from a voltage curve.
- Coexists with Logitech G HUB / Options+ if they're installed, but neither is required.

## For developers

Build from source with [Rust](https://rustup.rs/):

```sh
cargo build --release          # build
cargo run                      # start the tray app
cargo run -- --once            # print battery level to stdout
cargo run -- --diag            # dump HID++ interfaces + ping results (hardware debugging)
```

The release binary and CI build with the **MSVC** toolchain (`x86_64-pc-windows-msvc`), which works out of the box on `windows-latest`. To build locally with the **GNU** toolchain instead, you also need a full MinGW-w64 on `PATH` (the `windows` crates invoke `dlltool`/`as`, which rustup's bundled MinGW does not fully provide).

CI lives in [`.github/workflows`](.github/workflows): `pr-build` tests and builds on every push/PR, and `release-on-tag` cuts a GitHub release with the exe and a SHA256 checksum when you push a `v*.*.*` tag.

## Acknowledgements

- Structure and spirit modeled on [razertray](https://github.com/nuxencs/razertray).
- HID++ protocol details informed by [Solaar](https://github.com/pwr-Solaar/Solaar) and [LGSTrayBattery](https://github.com/andyvorld/LGSTrayBattery).

## License

[MIT](LICENSE)
