# logitray

A tiny Windows tray app that shows your wireless Logitech mouse's battery level and warns you before it dies — no Logitech G HUB or Options+ required.

## What it does

logitray sits in your system tray and talks to your Logitech wireless mouse directly over the HID++ protocol through its USB receiver. It listens for the receiver's notifications, so battery, charging, and connect/disconnect changes show up on the tray icon within about a second (with a periodic re-read as a backstop), and it pops a Windows notification when the battery gets low — all without any Logitech software running.

## Features

- Live battery level for your wireless Logitech mouse, right in the system tray
- Two view modes: a color-coded **battery glyph**, or the **percentage as text** — switch any time from the menu
- Color coding: green (healthy), orange (low), red (critical), blue (charging)
- Hover the icon for the device name and exact percentage
- Automatic low-battery notifications, with a cooldown so they don't spam you
- Multiple devices: pick which one the tray follows
- Manual "Refresh now" any time
- Optional autostart with Windows
- Optional daily check for new releases on GitHub (notify only)
- Event-driven: connect, disconnect, charging, and battery-level changes are pushed by the receiver and reflected within ~1s — no busy polling, near-zero idle USB traffic, with a periodic re-read as a backstop

## Install & run

1. Download `logitray.exe` from the [latest release](https://github.com/Ithilias/logitray/releases/latest).
2. Put it anywhere you like and double-click it — it appears in the system tray (check the `^` overflow if you don't see it).
3. (Optional) From a terminal, run `logitray.exe --once` to print the current battery level and confirm your device is detected.

No installer, no admin rights, no Logitech software needed. logitray does not start with Windows unless you enable **Start at login** from the tray menu.

## In the tray

Right-click the icon for the menu:

- **Status line** — the selected device and its battery (e.g. `G502 X Plus: 76%`)
- **Select Device** — choose which device the tray follows when more than one is paired
- **Refresh now** — re-check all devices immediately instead of waiting for the backstop re-read
- **Show percentage as text** — toggle between the battery-glyph icon and the percentage-number icon
- **Language** — Automatic (Windows display language), English, or 简体中文
- **Poll interval** — how often to re-read all devices as a backstop (15 seconds to 15 minutes; battery changes are pushed, so this only bounds the fallback)
- **Enable low-battery notifications** — toggle the Windows toast
- **Low battery alert at** — the percentage (5% to 30%) at or below which the toast fires
- **Reminder interval** — minimum time between repeat alerts per device (30 minutes to 8 hours)
- **Start at login** — register/unregister autostart with Windows
- **Check for updates automatically** — toggle the startup and daily check for new releases on GitHub (see Update check below)
- **Open config file…** — open `config.toml` in the default editor
- **Exit**

Icon colors: **green** ≥ 36%, **orange** 16–35%, **red** ≤ 15%, **blue** while charging.

## Language

The tray menu, tooltips, and battery notifications support English and Simplified Chinese.
By default, logitray detects the Windows display language at startup. Simplified Chinese
(Mainland China and Singapore) selects Chinese; unsupported languages, including
Traditional Chinese, fall back to English.

Choose **Language → Automatic (Windows language)**, **English**, or **简体中文** in the
tray menu. Changes apply immediately and are saved. Choosing Automatic reads the current
Windows display language again. Device names remain as reported by the device.
CLI diagnostics and logs remain in English.

## Low-battery alerts

When the selected device drops to or below the threshold (15% by default), logitray shows a Windows toast. To avoid nagging, it waits out a cooldown (120 minutes by default) before alerting again, and tracks each device separately.

## Update check

By default, logitray checks about a minute after startup and then once a day whether a newer release exists. The check is a single request to the GitHub releases page that reads the latest tag from the redirect. GitHub receives your IP address and the app version (user agent `logitray/<version>`); no identifiers or device data are sent. A failed check, for example because the network is not up yet, is retried after ten minutes. When a newer release exists, a toast appears once per new version and an **Update available** entry is added to the tray menu; clicking either opens the release page in your browser. Nothing is downloaded or installed automatically.

Turn the check off with **Check for updates automatically** in the tray menu or `check_for_updates = false` in the config. Turning it back on checks right away.

## Supported devices

Any Logitech wireless device that speaks **HID++ 2.0** through a Logitech **Unifying**, **LIGHTSPEED**, or **Bolt** USB receiver. Devices report their own marketing name over HID++, so there's no large hardcoded device database to maintain — the name you see is the one your mouse reports. Battery is read via feature `0x1000`, `0x1001` (voltage, converted with a lookup table), or `0x1004`, whichever the device supports.

Tested against a **G502 X PLUS** over a LIGHTSPEED receiver.

## Settings

Configuration lives in `%APPDATA%\logitray\config.toml` (created on first run):

| Key | Default | Description |
| --- | --- | --- |
| `poll_interval_seconds` | `180` | Backstop re-read interval. Battery/charging changes are pushed via notifications; this only bounds the fallback re-read (and recovery after sleep). |
| `low_battery_threshold` | `15` | Percent at/below which a low-battery toast fires |
| `low_battery_cooldown_minutes` | `120` | Minimum time between repeat alerts per device |
| `selected_device_id` | `""` | Which device the tray follows (set via the menu) |
| `autostart` | `false` | Start logitray when you log in |
| `log_level` | `"info"` | Log verbosity (`error`/`warn`/`info`/`debug`/`trace`) |
| `language` | `"auto"` | UI language: `auto` (Windows display language), `en`, or `zh-CN`. Unknown values fall back to English. |
| `view_mode` | `"icon"` | Tray display: `icon` (battery glyph) or `text` (percentage) |
| `check_for_updates` | `true` | Look for a newer release on GitHub at startup and once a day. Notify only; nothing is downloaded. |
| `last_notified_update` | `""` | Release the update toast was last shown for (set automatically) |

Enumerated device details (which battery feature to use, the device name) are cached per device in `%APPDATA%\logitray\devices.toml` so cold starts can skip the slower HID++ feature enumeration. It's safe to delete — it rebuilds itself.

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
cargo run -- --diag            # dump HID++ interfaces, ping results + notification probe (hardware debugging)
```

The release binary and CI build with the **MSVC** toolchain (`x86_64-pc-windows-msvc`), which works out of the box on `windows-latest`. To build locally with the **GNU** toolchain instead, you also need a full MinGW-w64 on `PATH` (the `windows` crates invoke `dlltool`/`as`, which rustup's bundled MinGW does not fully provide).

CI lives in [`.github/workflows`](.github/workflows): `pr-build` tests and builds on every push/PR, and `release-on-tag` cuts a GitHub release with the exe and a SHA256 checksum when you push a `v*.*.*` tag.

## Acknowledgements

- Structure and spirit modeled on [razertray](https://github.com/nuxencs/razertray).
- HID++ protocol details informed by [Solaar](https://github.com/pwr-Solaar/Solaar) and [LGSTrayBattery](https://github.com/andyvorld/LGSTrayBattery).

## License

[MIT](LICENSE)
