#![cfg(unix)]

use std::fs::OpenOptions;
use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use fs2::FileExt;
use tp_ipc::{EventBroadcaster, IpcHandler, Method};

struct RuntimeControl {
    connected: AtomicBool,
}

#[async_trait::async_trait]
impl IpcHandler for RuntimeControl {
    async fn handle(
        &self,
        method: &str,
        _params: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        match method {
            value if value == Method::GetStatus.as_str() => Ok(serde_json::json!({
                "connected": self.connected.load(Ordering::SeqCst),
                "client_ui": { "overall": { "state": "connected" } }
            })),
            value if value == Method::Disconnect.as_str() => {
                self.connected.store(false, Ordering::SeqCst);
                Ok(serde_json::json!({ "disconnected": true }))
            }
            _ => Err(format!("unsupported method {method}")),
        }
    }
}

async fn wait_for_path(path: &Path, server: &mut tokio::task::JoinHandle<tp_ipc::Result<()>>) {
    // The integration binary may start while other workspace jobs still hold
    // CPU and linker resources. Keep the check bounded, but do not turn
    // ordinary CI load into a one-second startup race.
    for _ in 0..1_000 {
        if path.exists() {
            return;
        }
        if server.is_finished() {
            let outcome = server.await;
            panic!(
                "IPC server exited before creating {}: {outcome:?}",
                path.display()
            );
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("IPC path did not become ready: {}", path.display());
}

#[test]
fn public_help_describes_one_ui_and_headless_client() {
    let output = Command::new(env!("CARGO_BIN_EXE_lantunnel-client"))
        .arg("--help")
        .output()
        .expect("run public help command");

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    let help = String::from_utf8(output.stdout).expect("help must be UTF-8");
    assert!(help.contains("Running without a command opens the Lantunnel Client UI."));
    assert!(help.contains("--headless"));
    assert!(help.contains("same Client runtime without the UI"));
    assert!(help.contains("connect <Tunnel ID>"));
    assert!(help.contains("tunnel import <FILE.peer>"));
    assert!(help.contains("status --json"));
    assert!(!help.contains("Lantunnel App"));
    assert!(!help.contains("app/client"));
}

#[test]
fn public_headless_cli_rejects_unknown_positionals() {
    let output = Command::new(env!("CARGO_BIN_EXE_lantunnel-client"))
        .args(["--headless", "unexpected"])
        .output()
        .expect("run invalid public headless command");

    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("unknown argument \"unexpected\""));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn public_control_commands_reach_running_instance_before_single_instance_rejection() {
    let root = tempfile::tempdir().expect("temporary Client config root");
    let config_dir = root.path().join("lantunnel-client");
    std::fs::create_dir_all(&config_dir).expect("Client config dir");

    let lock_path = config_dir.join("lantunnel-client.lock");
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .expect("single-instance lock fixture");
    lock.try_lock_exclusive()
        .expect("hold the running instance lock");

    let socket_path = config_dir.join("control.sock");
    let mut server = tokio::spawn(tp_ipc::serve(
        socket_path.clone(),
        Arc::new(RuntimeControl {
            connected: AtomicBool::new(true),
        }),
        EventBroadcaster::new(),
    ));
    wait_for_path(&socket_path, &mut server).await;

    let binary = env!("CARGO_BIN_EXE_lantunnel-client");
    let status = Command::new(binary)
        .args(["status", "--json"])
        .env("TUNNEL_PROXY_APP_CONFIG_DIR", &config_dir)
        .env("TUNNEL_PROXY_SKIP_ALREADY_RUNNING_DIALOG", "1")
        .output()
        .expect("run public status command");
    assert!(
        status.status.success(),
        "{}",
        String::from_utf8_lossy(&status.stderr)
    );
    let status_json: serde_json::Value =
        serde_json::from_slice(&status.stdout).expect("status stdout must be JSON only");
    assert_eq!(status_json["connected"], true);

    let disconnect = Command::new(binary)
        .arg("disconnect")
        .env("TUNNEL_PROXY_APP_CONFIG_DIR", &config_dir)
        .env("TUNNEL_PROXY_SKIP_ALREADY_RUNNING_DIALOG", "1")
        .output()
        .expect("run public disconnect command");
    assert!(
        disconnect.status.success(),
        "{}",
        String::from_utf8_lossy(&disconnect.stderr)
    );
    assert!(disconnect.stdout.is_empty());
    assert!(
        !server.is_finished(),
        "disconnect must not stop the running process"
    );

    let status = Command::new(binary)
        .args(["status", "--json"])
        .env("TUNNEL_PROXY_APP_CONFIG_DIR", &config_dir)
        .output()
        .expect("query status after disconnect");
    assert!(status.status.success());
    let status_json: serde_json::Value =
        serde_json::from_slice(&status.stdout).expect("post-disconnect status JSON");
    assert_eq!(status_json["connected"], false);

    server.abort();
    lock.unlock().expect("release fixture lock");
}
