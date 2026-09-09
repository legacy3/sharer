//! Short-lived completion sound with no idle audio resources.

use std::time::Duration;

use anyhow::{Context as _, Result};
use rodio::{DeviceSinkBuilder, Player, Source as _, source::SineWave};

pub(super) fn play_completion_async() {
    let _ = std::thread::Builder::new()
        .name("sharer-completion-sound".to_owned())
        .spawn(|| {
            let _ = play_completion();
        });
}

fn play_completion() -> Result<()> {
    let output = DeviceSinkBuilder::open_default_sink().context("no audio output is available")?;
    let player = Player::connect_new(output.mixer());

    player.append(
        SineWave::new(660.0)
            .take_duration(Duration::from_millis(65))
            .amplify(0.10),
    );
    player.append(
        SineWave::new(880.0)
            .take_duration(Duration::from_millis(90))
            .amplify(0.10),
    );
    player.sleep_until_end();

    Ok(())
}
