//! The helper's `--event=mirror_stream` bridge, driven as the Pi extension
//! drives it: JSON lines on its pipes, framed msgpack on the socket.

use std::process::Stdio;

use scribe_common::framing::{read_message, write_message};
use scribe_common::ids::SessionId;
use scribe_common::protocol::{ClientMessage, ServerMessage, TerminalSize};
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::net::UnixListener;
use tokio::process::Command;

// @lat: [[test#Test Harness#AI Hook Helper#Mirror stream bridges pipes and socket]]
#[tokio::test]
async fn mirror_stream_bridges_frames_up_and_commands_down() {
    let session_id = SessionId::new();
    let socket_path = std::env::temp_dir().join(format!(
        "sh-mirror-{}-{}.sock",
        std::process::id(),
        &session_id.to_full_string()[..8]
    ));
    let listener = UnixListener::bind(&socket_path).expect("test listener should bind");

    let mut helper = Command::new(env!("CARGO_BIN_EXE_scribe-hook-helper"))
        .args(["--provider=pi", "--event=mirror_stream"])
        .env("SCRIBE_HOOK_SOCK", &socket_path)
        .env("SCRIBE_SESSION_ID", session_id.to_full_string())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("helper should start");
    let mut stdin = helper.stdin.take().expect("piped stdin");
    let mut stdout = BufReader::new(helper.stdout.take().expect("piped stdout")).lines();

    let (mut socket, _) =
        tokio::time::timeout(std::time::Duration::from_secs(5), listener.accept())
            .await
            .expect("helper connected")
            .expect("accept succeeded");
    let attach: ClientMessage = read_message(&mut socket).await.expect("attach frame");
    assert!(
        matches!(attach, ClientMessage::MirrorStreamAttach { session_id: id } if id == session_id)
    );

    // A viewport goes down as one JSON line.
    let size = TerminalSize { cols: 160, rows: 50, ..TerminalSize::default() };
    write_message(&mut socket, &ServerMessage::MirrorViewport { session_id, size })
        .await
        .expect("viewport written");
    let viewport = stdout.next_line().await.expect("stdout readable").expect("a command line");
    assert_eq!(viewport, r#"{"cols":160,"rows":50}"#);

    // A frame goes up as one msgpack message carrying the grid it was drawn for.
    stdin
        .write_all(b"{\"cols\":160,\"rows\":50,\"frame\":\"\\u001b[2Jhello\"}\n")
        .await
        .expect("frame line written");
    let frame: ClientMessage = read_message(&mut socket).await.expect("frame");
    match frame {
        ClientMessage::MirrorFrame { session_id: id, size: got, data } => {
            assert_eq!(id, session_id);
            assert_eq!(got, size);
            assert_eq!(data, b"\x1b[2Jhello");
        }
        other => panic!("expected a MirrorFrame, got {other:?}"),
    }

    // A wheel step goes down too; other server traffic is ignored.
    write_message(&mut socket, &ServerMessage::Bell { session_id }).await.expect("bell written");
    write_message(&mut socket, &ServerMessage::MirrorScroll { session_id, delta: -2 })
        .await
        .expect("scroll written");
    let scroll = stdout.next_line().await.expect("stdout readable").expect("a command line");
    assert_eq!(scroll, r#"{"scroll":-2}"#);

    // The extension closing its pipe ends the bridge, silently and with exit 0.
    drop(stdin);
    let status = tokio::time::timeout(std::time::Duration::from_secs(5), helper.wait())
        .await
        .expect("helper exited")
        .expect("exit status");
    assert!(status.success());
    drop(std::fs::remove_file(&socket_path));
}
