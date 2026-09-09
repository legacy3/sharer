//! Per-user desktop login startup registration.

use anyhow::{Context as _, Result};
use auto_launch::{AutoLaunch, AutoLaunchBuilder};

fn registration() -> Result<AutoLaunch> {
    let executable = std::env::current_exe().context("could not locate the ShareR executable")?;
    let executable = executable
        .to_str()
        .context("ShareR's executable path is not valid Unicode")?;
    let mut builder = AutoLaunchBuilder::new();

    builder
        .set_app_name("ShareR")
        .set_app_path(executable)
        .set_args(&["--background"]);

    #[cfg(windows)]
    builder.set_windows_enable_mode(auto_launch::WindowsEnableMode::CurrentUser);
    #[cfg(target_os = "linux")]
    builder.set_linux_launch_mode(auto_launch::LinuxLaunchMode::XdgAutostart);
    #[cfg(target_os = "macos")]
    builder.set_macos_launch_mode(auto_launch::MacOSLaunchMode::LaunchAgent);

    builder.build().context("could not configure login startup")
}

pub(super) fn set_enabled(enabled: bool) -> Result<()> {
    let registration = registration()?;

    if enabled {
        registration
            .enable()
            .context("could not enable login startup")
    } else {
        registration
            .disable()
            .context("could not disable login startup")
    }
}

pub(super) fn is_enabled() -> Result<bool> {
    registration()?
        .is_enabled()
        .context("could not inspect login startup")
}
