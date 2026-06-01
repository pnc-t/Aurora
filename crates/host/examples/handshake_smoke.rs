//! ddd-host が立ち上がっている前提で、TCP 経由で Noise XX-PSK ハンドシェイクし
//! 最初の HostInfo を受信して表示する結合テスト。
//!
//! 使い方:
//! ```text
//! # term1
//! cargo run -p ddd-host --release
//! # term2 (表示されたパスワードを引数に)
//! cargo run -p ddd-host --release --example handshake_smoke -- <password>
//! ```

use anyhow::{Context, Result, anyhow};
use bincode::Options;
use ddd_common::noise::{HandshakeState, psk_from_password};
use ddd_common::proto::ControlMsg;
use std::io::{Read, Write};
use std::net::TcpStream;

fn read_frame(stream: &mut TcpStream) -> Result<Vec<u8>> {
    let mut len = [0u8; 2];
    stream.read_exact(&mut len)?;
    let n = u16::from_be_bytes(len) as usize;
    let mut buf = vec![0u8; n];
    stream.read_exact(&mut buf)?;
    Ok(buf)
}

fn write_frame(stream: &mut TcpStream, data: &[u8]) -> Result<()> {
    let len = (data.len() as u16).to_be_bytes();
    stream.write_all(&len)?;
    stream.write_all(data)?;
    Ok(())
}

fn main() -> Result<()> {
    let password = std::env::args()
        .nth(1)
        .ok_or_else(|| anyhow!("usage: handshake_smoke <password>"))?;
    let psk = psk_from_password(&password);

    let mut stream = TcpStream::connect("127.0.0.1:7000").context("connect 127.0.0.1:7000")?;
    let mut hs = HandshakeState::new_initiator(&psk)?;
    let mut out = Vec::new();
    let mut tmp = Vec::new();

    // -> e
    hs.write_msg(&[], &mut out)?;
    write_frame(&mut stream, &out)?;
    // <- e, ee, s, es
    let msg = read_frame(&mut stream)?;
    hs.read_msg(&msg, &mut tmp)?;
    // -> s, se, psk
    hs.write_msg(&[], &mut out)?;
    write_frame(&mut stream, &out)?;

    let mut session = hs.into_transport()?;
    println!("handshake complete");

    let ct = read_frame(&mut stream)?;
    let mut plain = Vec::new();
    session.decrypt(&ct, &mut plain)?;
    let msg: ControlMsg = bincode::DefaultOptions::new()
        .with_limit(64 * 1024)
        .deserialize(&plain)
        .context("decode HostInfo")?;
    println!("received: {msg:?}");
    Ok(())
}
