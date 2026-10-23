//! The engine sits behind a factory so tests can put a stand-in engine there.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;

use ccswap_claude::anthropic_client::AnthropicClient;
use ccswap_claude::autoswitch::{
    AutoSwitchEngine, AutoSwitchEvent, AutoSwitchSettings, LoopHandle, SwitcherHost,
};
use ccswap_claude::secret_store::{Keychain, SecretStore};
use ccswap_core::endpoints::Endpoints;
use ccswap_core::env::Environment;
use ccswap_core::paths::Paths;
use ccswap_core::presentation::{Style, Theme};

pub trait EngineControl: Send {
    fn stop(&self);
    fn wake(&self);
    /// Takes effect from the engine's next pass.
    fn apply_threshold(&self, threshold: f64);
}

impl EngineControl for LoopHandle {
    fn stop(&self) {
        LoopHandle::stop(self);
    }

    fn wake(&self) {
        LoopHandle::wake(self);
    }

    fn apply_threshold(&self, threshold: f64) {
        LoopHandle::apply_threshold(self, threshold);
    }
}

pub enum EngineReport {
    Event(AutoSwitchEvent),
    /// The engine thread died; the loop itself never ends on an error.
    Crashed(String),
}

pub type EngineSink = Arc<dyn Fn(EngineReport) + Send + Sync>;

pub struct EngineStart {
    pub settings: AutoSwitchSettings,
    pub dry_run: bool,
    pub sink: EngineSink,
}

pub type EngineFactory = Arc<dyn Fn(EngineStart) -> Box<dyn EngineControl> + Send + Sync>;

pub fn idle_factory() -> EngineFactory {
    Arc::new(|_| Box::new(LoopHandle::new()))
}

fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|s| s.to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "engine thread panicked".into())
}

/// The host is built on the engine thread because it borrows its secret store
/// and client.
pub fn live_factory(env: Environment) -> EngineFactory {
    Arc::new(move |start: EngineStart| {
        let handle = LoopHandle::new();
        let (control, env) = (handle.clone(), env.clone());
        std::thread::spawn(move || {
            let sink = start.sink;
            let events = sink.clone();
            let run = catch_unwind(AssertUnwindSafe(move || {
                let paths = Paths::new(&env);
                let endpoints = Endpoints::new(&env);
                let keychain = Keychain::new(endpoints.security.clone(), &env);
                let secrets = SecretStore::new(keychain, paths.clone(), &env);
                let client = AnthropicClient::new(endpoints);
                let style = Style::new(false, Theme::Dark);
                let host = SwitcherHost::new(&paths, &env, &secrets, &client, &style);
                AutoSwitchEngine::new(
                    &host,
                    start.settings,
                    Box::new(move |event| events(EngineReport::Event(event))),
                )
                .dry_run(start.dry_run)
                .loop_handle(handle)
                .run_loop();
            }));
            if let Err(payload) = run {
                sink(EngineReport::Crashed(panic_message(payload.as_ref())));
            }
        });
        Box::new(control) as Box<dyn EngineControl>
    })
}
