//! Windows-only: version resource and icon for `dbc-mcp.exe`. Same script
//! as `crates/dbc-ui/build.rs`, which explains why; the short version is
//! that every shipped exe carries its product name and version, because
//! Explorer shows them and a code-signing policy can require them.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=../dbc-ui/assets/dbc.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let mut res = winresource::WindowsResource::new();
    res.set("ProductName", "dbc");
    res.set("FileDescription", "dbc — MCP server");
    res.set("CompanyName", "Tomáš Bruckner");
    res.set("LegalCopyright", "© Tomáš Bruckner");
    res.set("OriginalFilename", "dbc-mcp.exe");
    if std::path::Path::new("../dbc-ui/assets/dbc.ico").exists() {
        res.set_icon("../dbc-ui/assets/dbc.ico");
    }
    if let Err(e) = res.compile() {
        println!("cargo:warning=version resource not embedded: {e}");
    }
}
