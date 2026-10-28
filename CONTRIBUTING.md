# Contributing to logitray

Bug reports, device reports, and pull requests are welcome.

## Reporting a device that doesn't work

Exit the tray app, open PowerShell in the folder with the exe, and run:

```powershell
.\logitray.exe --diag | Tee-Object diag.txt
```

Attach `diag.txt` to an issue, along with the device and receiver model. The exe is a GUI app, so the pipe is what makes PowerShell wait for the run to finish. Redirecting with `>` leaves the file empty. The output lists the Logitech HID interfaces Windows exposes, which of them answer HID++, the receiver's notifications, and what each paired device replies to the battery probes.

## Building

With [Rust](https://rustup.rs/) installed:

```sh
cargo build --release   # build
cargo run               # start the tray app
cargo run -- --once     # print battery levels to stdout
cargo run -- --diag     # hardware diagnostics, see above
cargo test              # unit tests, including a simulated receiver
```

Releases and CI use the MSVC toolchain (`x86_64-pc-windows-msvc`). The GNU toolchain also works, but needs a full MinGW-w64 on `PATH`: the `windows` crates call `dlltool` and `as`, and `build.rs` calls `windres` to embed the icon and version info. rustup's bundled MinGW doesn't include all of them.

## Pull requests

- **One concern per PR.** A fix, a feature, and a refactor go in separate PRs, even if they touch the same code. If a change builds on another open PR, stack it on top of that branch instead of combining them.
- **Use [Conventional Commits](https://www.conventionalcommits.org/) for the PR title**, for example `fix: keep the pinned device after a port change`. The prefix (`feat`, `fix`, `docs`, `chore`, `ci`, `refactor`, `build`, or `test`) labels the PR and decides where it shows up in the release notes. PRs are squash-merged, so the title becomes the commit message on `master`.
- Run `cargo fmt` and `cargo test` before pushing. CI checks both.

## CI

The workflows live in [`.github/workflows`](.github/workflows):

- `pr-build` runs `cargo fmt --check`, `cargo test --all-targets`, and a release build on every PR and on pushes to `master`. Changes that only touch Markdown files or `docs/` skip it.
- `label-release-notes` labels each PR from its title prefix. A `chore: release vX.Y.Z` PR is left out of the release notes.
- `release-on-tag` publishes a GitHub release with the exe and a SHA256 checksum when a `v*.*.*` tag is pushed. It fails if the tag doesn't match the version in `Cargo.toml`, so bump that first.

## How it works

logitray talks to devices over Logitech's HID++ protocol through hidapi. On Windows, a receiver exposes its HID++ interface as two collections: a **short** one (usage `0x0001`, 7-byte reports) and a **long** one (usage `0x0002`, 20-byte reports).

- **One worker per receiver** ([`src/hid/client.rs`](src/hid/client.rs)) blocks on the short collection, where the receiver pushes device-connection notifications. When a device links up, the worker reads its battery and sends an update to the tray. A timer re-reads all devices as a backstop (the poll interval) and re-enables notifications after sleep. Receivers without a short collection fall back to a bounded ping sweep.
- **HID++ 2.0 devices** are read on the long collection. The worker looks up the device's battery feature (`0x1000` level, `0x1001` voltage, or `0x1004` unified battery) and its name (`0x0005`). Voltage readings are converted to a percentage with a lookup table, so they are estimates.
- **HID++ 1.0 devices** (for example the original K800) have no feature table. When enumeration fails, the worker reads their battery registers on the short collection instead. Most report only a coarse level (90, 50, 20, or 5%), and since they can't report a name, [`src/device_map.rs`](src/device_map.rs) names the known ones.
- **Direct USB devices:** an interface that doesn't answer receiver setup is pinged at device index `0xFF`. If a HID++ 2.0 device answers, it gets its own worker that reads it directly.
- **Device cache:** the battery source (HID++ 2.0 feature or HID++ 1.0 register) and name of each device are cached in `%APPDATA%\logitray\devices.toml`, so cold starts can skip feature enumeration. Deleting it is safe.

Receiver register access and the HID++ 1.0 frame format are in [`src/hid/receiver.rs`](src/hid/receiver.rs), the HID++ 2.0 frames in [`src/hid/protocol.rs`](src/hid/protocol.rs). [`src/hid/sim.rs`](src/hid/sim.rs) simulates a receiver with up to six paired devices, routed the way real hardware does it, so the worker can be tested without hardware.

Only one device has been tested on real hardware so far (a G502 X PLUS over LIGHTSPEED). HID++ 1.0 support has only been tested against the simulated receiver.
