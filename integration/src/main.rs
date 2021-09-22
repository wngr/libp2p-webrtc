use std::process::Stdio;

use futures::future;
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::{Child, Command},
};

async fn start_signaling_server() -> anyhow::Result<Child> {
    let mut cmd = Command::new("cargo");
    cmd.args(&["run", "--bin", "signaling-server", "--", "127.0.0.1:8001"])
        .current_dir("..")
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true);
    let mut server = cmd.spawn()?;
    let stdout = server.stdout.take().unwrap();
    let mut reader = BufReader::new(stdout).lines();
    while let Some(line) = reader.next_line().await? {
        if line.starts_with("Listening on") {
            break;
        }
    }
    println!("Signaling server started!");
    server.stdout.replace(reader.into_inner().into_inner());
    Ok(server)
}

async fn wasm_wasm() -> anyhow::Result<()> {
    let mut _server = start_signaling_server().await?;

    let mut cmd = Command::new("wasm-pack");
    cmd.args(&[
        "test",
        "--headless",
        "--firefox",
        "--chrome",
        "--",
        "--",
        "wasm_wasm",
    ])
    .current_dir("..")
    .stdout(Stdio::inherit())
    .stderr(Stdio::inherit())
    .kill_on_drop(true);
    let mut child = cmd.spawn()?;
    let exit = child.wait().await?;
    anyhow::ensure!(exit.success());
    Ok(())
}

async fn wasm_native() -> anyhow::Result<()> {
    let mut _server = start_signaling_server().await?;

    let mut cmd = Command::new("cargo");
    cmd.args(&[
        "run",
        "--target",
        "x86_64-unknown-linux-gnu",
        "--example",
        "interop",
    ])
    .current_dir("..")
    .stdout(Stdio::piped())
    .stderr(Stdio::inherit())
    .kill_on_drop(true);
    let mut native = cmd.spawn()?;

    let stdout = native.stdout.take().unwrap();
    let mut reader = BufReader::new(stdout).lines();
    let mut peer = None;
    while let Some(line) = reader.next_line().await? {
        if line.starts_with("/p2p/") {
            println!("native peer: {}", line);
            peer.replace(line);
            break;
        }
    }

    let mut cmd = Command::new("wasm-pack");
    cmd.args(&["test", "--headless", "--chrome", "--", "--", "interop"])
        .current_dir("..")
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .env("TEST_PEER", peer.unwrap())
        .kill_on_drop(true);

    let mut wasm = cmd.spawn()?;
    let (exit_native, exit_wasm) = future::try_join(native.wait(), wasm.wait()).await?;
    anyhow::ensure!(exit_native.success() && exit_wasm.success());
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    wasm_wasm().await?;
    wasm_native().await?;
    Ok(())
}
