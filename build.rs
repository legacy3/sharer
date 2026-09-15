#[cfg(feature = "desktop")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        // Swift bridge dependencies do not propagate their runtime search paths.
        println!("cargo::rustc-link-arg=-Wl,-rpath,/usr/lib/swift");
    }

    slint_build::compile_with_config(
        "ui/app.slint",
        slint_build::CompilerConfiguration::new().with_style("fluent-dark".into()),
    )?;

    #[cfg(windows)]
    winresource::WindowsResource::new()
        .set_icon("assets/sharer.ico")
        .compile()?;

    Ok(())
}

#[cfg(not(feature = "desktop"))]
fn main() {}
