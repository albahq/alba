//! Scripted fake plugin binary driving `PluginExecutor`'s integration
//! tests (`tests/plugin.rs`).
//!
//! Its behavior is selected by `argv[1]` (default `ok`) rather than by an
//! `open` message or an environment variable — see
//! `PluginExecutor::with_args`'s doc comment for why.

use std::env;

use alba_executors::protocol::{HostMessage, PluginMessage, WireStream};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines, Stdin, Stdout};

#[tokio::main]
async fn main() {
    let mode = env::args().nth(1).unwrap_or_else(|| "ok".to_string());
    match mode.as_str() {
        "die" => std::process::exit(3),
        "mute" => mute(&mut reader()).await,
        "garbage" => garbage().await,
        "refuse" => refuse().await,
        "deaf" => deaf().await,
        _ => ok().await,
    }
}

fn reader() -> Lines<BufReader<Stdin>> {
    BufReader::new(tokio::io::stdin()).lines()
}

async fn write(out: &mut Stdout, message: &PluginMessage) {
    let mut line = serde_json::to_string(message).expect("PluginMessage always serializes");
    line.push('\n');
    let _ = out.write_all(line.as_bytes()).await;
    let _ = out.flush().await;
}

/// Reads and discards every line forever, answering nothing. This is
/// `mute`'s whole behavior, and also the tail every other misbehaving mode
/// falls into once it has done whatever it does before going silent, so
/// the host's eventual kill always finds the process blocked in a read
/// rather than racing an unrelated exit.
async fn mute(lines: &mut Lines<BufReader<Stdin>>) {
    while let Ok(Some(_line)) = lines.next_line().await {}
}

async fn garbage() {
    let mut lines = reader();
    let mut out = tokio::io::stdout();
    let _ = out.write_all(b"this is not json\n").await;
    let _ = out.flush().await;
    mute(&mut lines).await;
}

async fn refuse() {
    let mut lines = reader();
    // The one message expected here is `open`; its content does not
    // matter for this scenario, only that the answer is `error`.
    if lines.next_line().await.unwrap_or(None).is_none() {
        return;
    }
    let mut out = tokio::io::stdout();
    write(
        &mut out,
        &PluginMessage::Error {
            message: "protocol 1 not supported".to_string(),
        },
    )
    .await;
    mute(&mut lines).await;
}

async fn deaf() {
    let mut lines = reader();
    if lines.next_line().await.unwrap_or(None).is_none() {
        return;
    }
    let mut out = tokio::io::stdout();
    write(&mut out, &PluginMessage::Ready).await;
    // Ready at open, then silence: every later message (an `execute`, a
    // `cancel`, anything) is read and discarded, never answered. This is
    // what lets a test exercise a plugin that acknowledges the handshake
    // but never reacts to anything afterwards, including cancellation.
    mute(&mut lines).await;
}

async fn ok() {
    let mut lines = reader();
    let Some(open_line) = lines.next_line().await.unwrap_or(None) else {
        return;
    };
    if serde_json::from_str::<HostMessage>(&open_line).is_err() {
        return;
    }
    let mut out = tokio::io::stdout();
    write(&mut out, &PluginMessage::Ready).await;

    loop {
        let Some(line) = lines.next_line().await.unwrap_or(None) else {
            return;
        };
        let Ok(message) = serde_json::from_str::<HostMessage>(&line) else {
            continue;
        };
        match message {
            HostMessage::Execute { command, .. } => {
                execute(&command, &mut lines, &mut out).await;
            }
            HostMessage::Close => return,
            HostMessage::Cancel | HostMessage::Open { .. } => {}
        }
    }
}

async fn execute(command: &str, lines: &mut Lines<BufReader<Stdin>>, out: &mut Stdout) {
    if let Some(text) = command.strip_prefix("echo ") {
        write(
            out,
            &PluginMessage::Output {
                stream: WireStream::Stdout,
                text: text.to_string(),
            },
        )
        .await;
        write(out, &PluginMessage::Exit { code: 0 }).await;
    } else if let Some(code) = command.strip_prefix("fail ") {
        let code: i32 = code.trim().parse().unwrap_or(1);
        write(out, &PluginMessage::Exit { code }).await;
    } else if command == "stderr-probe" {
        eprintln!("raw stderr line");
        write(out, &PluginMessage::Exit { code: 0 }).await;
    } else if command == "hang" {
        loop {
            let Some(line) = lines.next_line().await.unwrap_or(None) else {
                return;
            };
            if let Ok(HostMessage::Cancel) = serde_json::from_str::<HostMessage>(&line) {
                write(out, &PluginMessage::Exit { code: -1 }).await;
                return;
            }
            // Anything else while hanging is ignored, per the brief.
        }
    } else {
        write(out, &PluginMessage::Exit { code: 1 }).await;
    }
}
