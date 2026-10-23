mod app;
mod autoview;
mod data;
mod engine;
mod theme;
mod view;
mod widgets;

#[cfg(test)]
mod testing;
#[cfg(test)]
mod tests;

use std::io;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ccswap_claude::actions::Actions;
use ccswap_core::appearance::{self, Background};
use ccswap_core::env::Environment;
use ccswap_core::settings::Settings;
use ratatui::crossterm::event::{self, Event, KeyEventKind};

pub use app::{App, AppConfig};

const FRAME: Duration = Duration::from_millis(50);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Start {
    Dashboard,
    Watch,
}

pub fn run(actions: Arc<dyn Actions>, env: &Environment, start: Start) -> i32 {
    // Probed even for an explicit theme so cycling to `auto` works; the
    // terminal must still be in cooked mode here.
    let detected = appearance::detect_terminal_background(env);
    let mut config = AppConfig::load(&Settings::in_dir(
        &actions.backup_dir(),
        ccswap_claude::SETTINGS,
    ));
    config.detected_light = detected.map(|b| b == Background::Light);
    appearance::drain_stdin();
    let mut app = App::new(actions, config).with_engine_factory(engine::live_factory(env.clone()));
    if start == Start::Watch {
        app.open_watch();
    }
    let mut terminal = ratatui::init();
    let result = event_loop(&mut terminal, &mut app);
    app.shutdown();
    ratatui::restore();
    match result {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("Error: {e}");
            1
        }
    }
}

fn event_loop(terminal: &mut ratatui::DefaultTerminal, app: &mut App) -> io::Result<()> {
    let poll_every = Duration::from_secs_f64(app::POLL_INTERVAL_S);
    let status_every = Duration::from_secs(1);
    app.tick();
    let (mut next_poll, mut next_status) =
        (Instant::now() + poll_every, Instant::now() + status_every);
    while !app.should_quit {
        app.drain();
        let now = Instant::now();
        if now >= next_poll {
            app.tick();
            next_poll = now + poll_every;
        }
        if now >= next_status {
            app.update_refresh_status();
            next_status = now + status_every;
        }
        app.expire_notifications(now);
        terminal.draw(|frame| view::draw(frame, app))?;
        if event::poll(FRAME)?
            && let Event::Key(key) = event::read()?
            && key.kind != KeyEventKind::Release
        {
            app.handle_key(key);
        }
    }
    Ok(())
}
