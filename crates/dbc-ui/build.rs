//! Windows-only: stamps the exe with a version resource (what Explorer's
//! „Podrobnosti" tab and the Programs list show) and the app icon.
//!
//! Everything comes from `Cargo.toml` (`CARGO_PKG_VERSION`) so the
//! `chore: vX.Y.Z` bump is the only place a version is ever typed. The
//! icon is optional on purpose: a checkout without `assets/dbc.ico` still
//! builds, it just gets the default Windows icon — the build never fails
//! over a picture.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=assets/dbc.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let mut res = winresource::WindowsResource::new();
    res.set("ProductName", "dbc");
    res.set("FileDescription", "dbc — databázový klient");
    res.set("CompanyName", "Tomáš Bruckner");
    res.set("LegalCopyright", "© Tomáš Bruckner");
    res.set("OriginalFilename", "dbc-ui.exe");
    if std::path::Path::new("assets/dbc.ico").exists() {
        res.set_icon("assets/dbc.ico");
    }
    if let Err(e) = res.compile() {
        // Missing rc.exe on an odd toolchain must not block a build that
        // otherwise works; the exe merely ships without metadata.
        println!("cargo:warning=version resource not embedded: {e}");
    }
}
