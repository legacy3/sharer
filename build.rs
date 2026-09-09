#[cfg(feature = "desktop")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
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
