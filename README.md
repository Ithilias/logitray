# logitray

A tiny Windows tray app that shows the battery level of your wireless Logitech devices and warns you before they run out. No G HUB or Options+ required.

- Battery for every paired mouse, keyboard, or trackball, as a color-coded glyph or a plain percentage
- Low-battery notifications for any device, with a per-device cooldown so they don't nag
- Updates within about a second: the receiver pushes battery, charging, and connect events, so there is no busy polling
- One portable exe: no installer, no admin rights, no Logitech software
- English and Simplified Chinese

## Install

1. Download `logitray.exe` from the [latest release](https://github.com/Ithilias/logitray/releases/latest).
2. Put it anywhere and run it. The icon appears in the system tray (check the `^` overflow if you don't see it).
3. Optional: enable **Start at login** from the tray menu.

To confirm your device is detected, run `logitray.exe --once` in a terminal. It prints the current battery levels.

## Using it

Hover the icon to see each device's name and battery. Right-click for the menu: the status line, **Select Device**, **Refresh now**, and the settings below.

The icon is **green** when healthy, **orange** when getting low, **red** at or below the alert threshold, and **blue** while charging.

<details>
<summary>How the color thresholds are chosen</summary>

Red starts at the low-battery threshold, so the icon turns red exactly when a notification would fire. Orange covers the 20 points above it (never less than 35%), green everything higher. At the default threshold of 15% that is green at 36% and up, orange from 16 to 35%, red at 15% and below. With a threshold of 30%, green starts at 51%.

</details>

### Which device the icon follows

By default the icon follows **Automatic (lowest battery)**: whichever connected device is closest to running out, so one glance tells you whether anything is about to die. To pin the icon to one device instead, pick it under **Select Device**. Low-battery alerts cover every device either way.

<details>
<summary>How Automatic picks a device</summary>

- Devices that are charging are skipped, so a mouse on its cable doesn't hide a keyboard that is running low. If everything is charging, the lowest of them is shown.
- Another device only takes over the icon once it is at least 5 points lower, so two devices at similar levels don't flip back and forth.
- That margin doesn't apply at the alert threshold: a device that drops to it takes the icon right away.
- In text view the icon is a bare number, so the tooltip or status line tells you which device it belongs to.

</details>

## Settings

Everything can be changed from the tray menu and is saved to `%APPDATA%\logitray\config.toml` (**Open config file…** opens it).

| Menu item | Config key | Default | Notes |
| --- | --- | --- | --- |
| Select Device | `selected_device_id` | `"lowest"` | `"lowest"` is Automatic, otherwise a device chosen from the menu |
| Show percentage as text | `view_mode` | `"icon"` | `icon` (battery glyph) or `text` (percentage) |
| Language | `language` | `"auto"` | `auto`, `en`, or `zh-CN`. Auto picks Simplified Chinese for a Chinese (Mainland China, Singapore) Windows display language and English for everything else |
| Poll interval | `poll_interval_seconds` | `180` | Backstop re-read only, since changes are pushed. 15 seconds to 15 minutes |
| Enable low-battery notifications | `notifications_enabled` | `true` | |
| Low battery alert at | `low_battery_threshold` | `15` | Percent, 5 to 30 |
| Reminder interval | `low_battery_cooldown_minutes` | `120` | Minimum time between repeat alerts, per device. 30 minutes to 8 hours |
| Start at login | `autostart` | `false` | |
| Check for updates automatically | `check_for_updates` | `true` | See below |
| | `log_level` | `"info"` | `error`, `warn`, `info`, `debug`, or `trace` |

The log is written to `%APPDATA%\logitray\logitray.log`, rotated at 1 MB with the last three kept.

### Update check

About a minute after startup and then once a day, logitray asks GitHub for the latest release tag. GitHub sees your IP address and the app version, nothing else. If a newer release exists, you get one notification per version and an **Update available** menu entry that opens the release page. Nothing is downloaded or installed automatically. Turning the check back on checks right away.

## Supported devices

Logitech wireless devices on a **Unifying**, **LIGHTSPEED**, or **Bolt** receiver (up to six per receiver), plus modern devices connected directly with a USB cable. Bluetooth connections are not supported. G HUB and Options+ can stay installed alongside.

Modern devices report their own name, so they show up as you know them. Some devices only report their voltage, so their percentage is an estimate. Older devices such as the original K800 usually only report a coarse level (90, 50, 20, or 5%).

So far only a **G502 X PLUS** over LIGHTSPEED has been tested on real hardware. Other devices should work the same way. If yours doesn't show up, exit logitray, open PowerShell in the folder with the exe, run `.\logitray.exe --diag | Tee-Object diag.txt`, and attach `diag.txt` to an [issue](https://github.com/Ithilias/logitray/issues).

## Contributing

Build instructions and an overview of how logitray talks to the devices are in [CONTRIBUTING.md](CONTRIBUTING.md).

## Acknowledgements

- Structure and spirit modeled on [razertray](https://github.com/nuxencs/razertray).
- HID++ protocol details informed by [Solaar](https://github.com/pwr-Solaar/Solaar) and [LGSTrayBattery](https://github.com/andyvorld/LGSTrayBattery).

## License

[MIT](LICENSE)
