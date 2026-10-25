mod support;

use support::TestEnv;

#[test]
fn enabling_without_the_menu_bar_does_not_change_settings() {
    let env = TestEnv::new();
    let path = env.claude_config_home().join("settings.json");
    let before = "{\"theme\":\"dark\",\"env\":{\"KEEP\":\"mine\"}}\n";
    std::fs::write(&path, before).unwrap();
    let out = env.run(["claude", "openai", "on"]);
    out.assert_code(1);
    assert!(out.stderr.contains("menubar"), "{}", out.stderr);
    assert_eq!(std::fs::read_to_string(path).unwrap(), before);
    assert!(!env.store_dir().join("openai").exists());
}

use serde_json::json;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};

static PORT: Mutex<()> = Mutex::new(());
const TOKEN: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

struct MenuBar {
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
    _port: MutexGuard<'static, ()>,
}

impl MenuBar {
    fn start(env: &TestEnv) -> Self {
        let port = PORT.lock().unwrap();
        let listener = TcpListener::bind(ccswap::openai::PROXY_ADDRESS).unwrap();
        env.write_file(env.store_dir().join("openai/proxy-token"), TOKEN);
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        let worker = thread::spawn(move || {
            for stream in listener.incoming() {
                let mut stream = stream.unwrap();
                if stopping.load(Ordering::SeqCst) {
                    break;
                }
                let mut reader = BufReader::new(&stream);
                let mut headers = String::new();
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap() == 0 || line == "\r\n" {
                        break;
                    }
                    headers.push_str(&line);
                }
                assert!(headers.starts_with("GET /health "));
                assert!(
                    headers
                        .to_ascii_lowercase()
                        .contains(&format!("authorization: bearer {TOKEN}"))
                );
                stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 15\r\nConnection: close\r\n\r\n{\"status\":\"ok\"}").unwrap();
            }
        });
        Self {
            stop,
            worker: Some(worker),
            _port: port,
        }
    }
}

impl Drop for MenuBar {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(ccswap::openai::PROXY_ADDRESS);
        self.worker.take().unwrap().join().unwrap();
    }
}

#[test]
fn enabling_writes_only_the_mode_keys_and_preserves_key_order() {
    let env = TestEnv::new();
    let _menu_bar = MenuBar::start(&env);
    let path = env.claude_config_home().join("settings.json");
    env.write_file(&path, r#"{"z":1,"env":{"Z":"first","A":"second"},"a":2}"#);
    env.run(["claude", "openai", "on"]).assert_code(0);
    let settings = env.read_json(&path);
    assert_eq!(
        settings
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        ["z", "env", "a"]
    );
    let values = settings["env"].as_object().unwrap();
    assert_eq!(
        values
            .keys()
            .take(2)
            .map(String::as_str)
            .collect::<Vec<_>>(),
        ["Z", "A"]
    );
    assert_eq!(values["Z"], "first");
    assert_eq!(values["A"], "second");
    assert_eq!(values["ANTHROPIC_BASE_URL"], "http://127.0.0.1:18347");
    assert_eq!(values["ANTHROPIC_AUTH_TOKEN"], TOKEN);
    assert_eq!(values["ANTHROPIC_DEFAULT_OPUS_MODEL"], "gpt-6.1-sol");
    assert_eq!(values["ANTHROPIC_DEFAULT_SONNET_MODEL"], "gpt-6.1-sol");
    assert_eq!(values["ANTHROPIC_DEFAULT_HAIKU_MODEL"], "gpt-6-luna");
    assert_eq!(values["ANTHROPIC_DEFAULT_FABLE_MODEL"], "gpt-6.1-sol");
    assert_eq!(values.len(), 8);
    assert!(!values.contains_key("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC"));
}

#[test]
fn enabling_rejects_all_conflicting_keys_without_writing() {
    let env = TestEnv::new();
    let _menu_bar = MenuBar::start(&env);
    let path = env.claude_config_home().join("settings.json");
    let before = r#"{"env":{"ANTHROPIC_BASE_URL":"https://mine.example","ANTHROPIC_DEFAULT_HAIKU_MODEL":"mine"}}"#;
    env.write_file(&path, before);
    let out = env.run(["claude", "openai", "on"]);
    out.assert_code(1);
    assert!(out.stderr.contains("ANTHROPIC_BASE_URL"), "{}", out.stderr);
    assert!(
        out.stderr.contains("ANTHROPIC_DEFAULT_HAIKU_MODEL"),
        "{}",
        out.stderr
    );
    assert_eq!(std::fs::read_to_string(path).unwrap(), before);
}

#[test]
fn disabling_removes_only_unchanged_mode_values() {
    let env = TestEnv::new();
    let _menu_bar = MenuBar::start(&env);
    let path = env.claude_config_home().join("settings.json");
    env.run(["claude", "openai", "on"]).assert_code(0);
    let mut settings = env.read_json(&path);
    settings["env"]["ANTHROPIC_DEFAULT_HAIKU_MODEL"] = "my-model".into();
    settings["env"]["KEEP"] = "mine".into();
    env.write_json(&path, &settings);
    env.run(["claude", "openai", "off"]).assert_code(0);
    assert_eq!(
        env.read_json(&path),
        json!({"env":{"ANTHROPIC_DEFAULT_HAIKU_MODEL":"my-model","KEEP":"mine"}})
    );
}

#[test]
fn status_reads_the_inference_destination_from_settings_each_time() {
    let env = TestEnv::new();
    let path = env.claude_config_home().join("settings.json");
    env.write_json(
        &path,
        &json!({"env":{"ANTHROPIC_BASE_URL":"http://127.0.0.1:18347"}}),
    );
    let out = env.run(["claude", "openai", "status"]);
    out.assert_code(0);
    assert!(
        out.stdout.contains("Inference destination: OpenAI"),
        "{}",
        out.stdout
    );
    env.write_json(&path, &json!({}));
    let out = env.run(["claude", "openai", "status"]);
    out.assert_code(0);
    assert!(
        out.stdout.contains("Inference destination: Anthropic"),
        "{}",
        out.stdout
    );
}

#[test]
fn mode_changes_follow_a_settings_symlink_and_keep_it() {
    let env = TestEnv::new();
    let _menu_bar = MenuBar::start(&env);
    let target = env.root().join("dotfiles/claude-settings.json");
    env.write_json(&target, &json!({"theme":"dark"}));
    let link = env.claude_config_home().join("settings.json");
    std::os::unix::fs::symlink(&target, &link).unwrap();
    env.run(["claude", "openai", "on"]).assert_code(0);
    assert!(link.is_symlink());
    assert_eq!(
        env.read_json(&target)["env"]["ANTHROPIC_DEFAULT_SONNET_MODEL"],
        "gpt-6.1-sol"
    );
    env.run(["claude", "openai", "on"]).assert_code(0);
    env.run(["claude", "openai", "off"]).assert_code(0);
    assert!(link.is_symlink());
    assert_eq!(env.read_json(&target), json!({"theme":"dark","env":{}}));
}

#[test]
fn malformed_settings_are_left_untouched() {
    let env = TestEnv::new();
    let _menu_bar = MenuBar::start(&env);
    let path = env.claude_config_home().join("settings.json");
    for before in ["{broken", "[]", r#"{"env":null}"#] {
        env.write_file(&path, before);
        env.run(["claude", "openai", "on"]).assert_code(1);
        env.run(["claude", "openai", "off"]).assert_code(1);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
    }
}

#[test]
fn uninstalling_the_menu_bar_removes_only_owned_mode_values() {
    let env = TestEnv::new();
    env.write_file(env.store_dir().join("openai/proxy-token"), TOKEN);
    let path = env.claude_config_home().join("settings.json");
    env.write_json(
        &path,
        &json!({"env":{
            "ANTHROPIC_BASE_URL":"http://127.0.0.1:18347", "ANTHROPIC_AUTH_TOKEN":TOKEN,
            "ANTHROPIC_DEFAULT_SONNET_MODEL":"my-model", "KEEP":"mine"
        }}),
    );
    env.launchctl().load("state = running\npid = 123\n");
    env.run(["menubar", "--uninstall-service"]).assert_code(0);
    assert!(!env.launchctl().is_loaded());
    assert_eq!(
        env.read_json(&path),
        json!({"env":{"ANTHROPIC_DEFAULT_SONNET_MODEL":"my-model","KEEP":"mine"}})
    );
}
