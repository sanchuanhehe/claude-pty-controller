//! Full-stack: the real `controller` binary in relay/E2EE mode → relay →
//! a simulated dashboard. The dashboard runs the Noise XX handshake through the
//! relay, then DECRYPTS the controller's fanned-out channel-1 output (issue #2).
//! Proves per-dashboard encryption end-to-end and that the relay sees only
//! ciphertext.

use base64::{engine::general_purpose::STANDARD as B64, Engine};
use claude_pty_controller::e2ee::{derive_psk, derive_rendezvous_secret, room_id, current_epoch, Handshake, StaticKey};
use claude_pty_controller::relay_proto::{Envelope, Role, CONTROLLER_PEER};
use futures_util::{SinkExt, StreamExt};
use std::process::{Child, Command};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};

type Ws = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

struct Proc(Child);
impl Drop for Proc {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

async fn send(ws: &mut Ws, env: &Envelope) {
    ws.send(Message::Text(env.to_json())).await.unwrap();
}
async fn recv(ws: &mut Ws) -> Envelope {
    loop {
        match ws.next().await {
            Some(Ok(Message::Text(t))) => {
                if let Ok(e) = serde_json::from_str::<Envelope>(&t) {
                    return e;
                }
            }
            Some(Ok(_)) => continue,
            other => panic!("ws closed: {other:?}"),
        }
    }
}
async fn recv_data(ws: &mut Ws) -> String {
    loop {
        if let Envelope::Deliver { data, .. } = recv(ws).await {
            return data;
        }
    }
}

#[tokio::test]
async fn controller_fans_out_encrypted_output_to_dashboard() {
    let pid = std::process::id();
    let port = 51_000 + (pid % 9_000) as u16;
    let addr = format!("127.0.0.1:{port}");
    let url = format!("ws://{addr}");
    let relay_token = "relay-tok";
    let pairing = "0123456789abcdef0123456789abcdef-high-entropy"; // >=32 chars (#1)

    // Workspace dirs.
    let base = std::env::temp_dir().join(format!("cpc-ce2e-{pid}"));
    let _ = std::fs::remove_dir_all(&base);
    let home = base.join("home");
    let claude = base.join("claude");
    let projdir = claude.join("projects").join("-tmp"); // sanitize("/tmp") = "-tmp"
    std::fs::create_dir_all(&projdir).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    // a fake transcript so channel-2 has something (not strictly required here)
    std::fs::write(
        projdir.join("abcdef01-2345-6789-abcd-ef0123456789.jsonl"),
        "{\"type\":\"user\",\"uuid\":\"u1\",\"sessionId\":\"abcdef01-2345-6789-abcd-ef0123456789\",\"message\":{\"role\":\"user\",\"content\":\"hi\"}}\n",
    )
    .unwrap();
    // a fake agent that emits channel-1 output continuously
    let agent = base.join("agent.sh");
    std::fs::write(&agent, "#!/bin/sh\nwhile true; do printf 'TICKMARK\\n'; sleep 0.3; done\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&agent, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    // Relay.
    let _relay = Proc(
        Command::new(env!("CARGO_BIN_EXE_relay"))
            .env("RELAY_ADDR", &addr)
            .env("RELAY_TOKEN", relay_token)
            .env("RUST_LOG", "warn")
            .spawn()
            .unwrap(),
    );

    // Controller in relay/E2EE mode (enrollment on so our dashboard is accepted).
    let sock = format!("cpc-ce2e-{pid}");
    let _ctrl = Proc(
        Command::new(env!("CARGO_BIN_EXE_claude-pty-controller"))
            .env("CPC_INSECURE", "1")
            .env("REMOTE_URL", &url)
            .env("PAIRING_SECRET", pairing)
            .env("RELAY_TOKEN", relay_token)
            .env("CPC_ALLOW_ENROLL", "1")
            .env("CLAUDE_PTY_HOME", &home)
            .env("CLAUDE_CONFIG_DIR", &claude)
            .env("TMUX_SOCKET", &sock)
            .env("TMUX_SESSION", &sock)
            .env("AGENT_CMD", agent.to_string_lossy().to_string())
            .env("RUST_LOG", "warn")
            .current_dir("/tmp")
            .spawn()
            .unwrap(),
    );

    // Compute the same room id the controller derived.
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
    let room = room_id(&derive_rendezvous_secret(pairing), current_epoch(now));

    // Dashboard joins the relay (retry until relay is up).
    let mut dash: Option<Ws> = None;
    for _ in 0..60 {
        if let Ok((ws, _)) = connect_async(&url).await {
            dash = Some(ws);
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let mut dash = dash.expect("relay/controller never came up");
    send(&mut dash, &Envelope::Join { room, role: Role::Dashboard, peer: "dash-1".into(), token: Some(relay_token.into()) }).await;
    assert!(matches!(recv(&mut dash).await, Envelope::Joined));

    let psk = derive_psk(pairing);
    let dash_key = StaticKey::generate().unwrap();
    let mut hs = Handshake::initiator(&dash_key.private().unwrap(), &psk).unwrap();

    // Wait until the relay tells us the controller is present, then initiate XX.
    let transport = tokio::time::timeout(Duration::from_secs(25), async {
        loop {
            if let Envelope::PeerJoin { peer } = recv(&mut dash).await {
                if peer == CONTROLLER_PEER {
                    break;
                }
            }
        }
        let m1 = hs.write().unwrap();
        send(&mut dash, &Envelope::Msg { to: CONTROLLER_PEER.into(), data: B64.encode(&m1) }).await;
        let m2 = B64.decode(recv_data(&mut dash).await).unwrap();
        hs.read(&m2).unwrap();
        let m3 = hs.write().unwrap();
        send(&mut dash, &Envelope::Msg { to: CONTROLLER_PEER.into(), data: B64.encode(&m3) }).await;
        assert!(hs.is_finished());
        hs.into_transport().unwrap()
    })
    .await
    .expect("handshake timed out");
    let mut transport = transport;

    // Now decrypt fanned-out frames until we see channel-1 output with our marker.
    let saw_marker = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let wire = B64.decode(recv_data(&mut dash).await).unwrap();
            // relay never carries plaintext
            assert!(!wire.windows(8).any(|w| w == b"TICKMARK"));
            if let Ok(pt) = transport.decrypt(&wire) {
                let s = String::from_utf8_lossy(&pt);
                if s.contains("\"type\":\"output\"") && s.contains("TICKMARK") {
                    return true;
                }
            }
        }
    })
    .await
    .unwrap_or(false);

    assert!(saw_marker, "dashboard should decrypt the controller's channel-1 output");

    // Controller enrolled our device key.
    let authz = std::fs::read_to_string(home.join("authorized_devices.json")).unwrap_or_default();
    assert!(authz.contains(&dash_key.public_b64), "controller should have enrolled the dashboard key");

    // cleanup tmux server the controller created
    let _ = Command::new("tmux").args(["-L", &sock, "kill-server"]).status();
    let _ = std::fs::remove_dir_all(&base);
}
