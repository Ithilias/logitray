// Embed a Windows version resource (the Details tab in the file's properties).
// An exe without one looks like anonymous malware to Windows Defender's
// heuristics, which have flagged logitray as a trojan. winresource fills in the
// version numbers, ProductName and FileDescription from Cargo.toml; the rest is
// set here. FileDescription stays the bare name because Task Manager shows it
// as the process name.
fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=Cargo.toml");

    // Build scripts run on the host, so ask about the target, not cfg!(windows).
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    let mut res = winresource::WindowsResource::new();
    res.set("OriginalFilename", "logitray.exe")
        .set("InternalName", "logitray")
        .set("CompanyName", "Ithilias")
        .set("LegalCopyright", "Copyright (c) 2026 Ithilias, MIT License");
    if let Err(err) = res.compile() {
        panic!("failed to embed the Windows version resource: {err}");
    }
}
