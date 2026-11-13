//! Noise_XX encrypted channel over TCP. Each frame is a u16 length followed by one
//! Noise message. The caller decides whether the remote static key is trusted.

use anyhow::{anyhow, Result};
use snow::{params::NoiseParams, Builder, HandshakeState, TransportState};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::timeout;

pub const NOISE_PARAMS: &str = "Noise_XX_25519_ChaChaPoly_BLAKE2s";
const MAX_MSG: usize = 65535;
const IDLE_TIMEOUT: Duration = Duration::from_secs(60);

pub struct SecureChannel {
    stream: TcpStream,
    transport: TransportState,
    pub remote_static: Vec<u8>,
    buf: Vec<u8>,
}

async fn write_frame(stream: &mut TcpStream, data: &[u8]) -> Result<()> {
    stream.write_u16(data.len() as u16).await?;
    stream.write_all(data).await?;
    Ok(())
}

async fn read_frame(stream: &mut TcpStream, buf: &mut Vec<u8>) -> Result<usize> {
    let n = stream.read_u16().await? as usize;
    if buf.len() < n {
        buf.resize(n, 0);
    }
    stream.read_exact(&mut buf[..n]).await?;
    Ok(n)
}

fn finish(stream: TcpStream, hs: HandshakeState) -> Result<SecureChannel> {
    let remote_static = hs
        .get_remote_static()
        .ok_or_else(|| anyhow!("peer sent no identity key"))?
        .to_vec();
    let transport = hs.into_transport_mode()?;
    Ok(SecureChannel {
        stream,
        transport,
        remote_static,
        buf: vec![0u8; MAX_MSG],
    })
}

impl SecureChannel {
    /// We connected out: send first.
    pub async fn initiate(mut stream: TcpStream, private_key: &[u8]) -> Result<SecureChannel> {
        let params: NoiseParams = NOISE_PARAMS.parse()?;
        let mut hs = Builder::new(params)
            .local_private_key(private_key)
            .build_initiator()?;
        let mut out = vec![0u8; MAX_MSG];
        let mut inp = vec![0u8; MAX_MSG];

        let n = hs.write_message(&[], &mut out)?;
        write_frame(&mut stream, &out[..n]).await?;
        let n = read_frame(&mut stream, &mut inp).await?;
        hs.read_message(&inp[..n], &mut out)?;
        let n = hs.write_message(&[], &mut out)?;
        write_frame(&mut stream, &out[..n]).await?;
        finish(stream, hs)
    }

    /// Someone connected to us: read first.
    pub async fn accept(mut stream: TcpStream, private_key: &[u8]) -> Result<SecureChannel> {
        let params: NoiseParams = NOISE_PARAMS.parse()?;
        let mut hs = Builder::new(params)
            .local_private_key(private_key)
            .build_responder()?;
        let mut out = vec![0u8; MAX_MSG];
        let mut inp = vec![0u8; MAX_MSG];

        let n = read_frame(&mut stream, &mut inp).await?;
        hs.read_message(&inp[..n], &mut out)?;
        let n = hs.write_message(&[], &mut out)?;
        write_frame(&mut stream, &out[..n]).await?;
        let n = read_frame(&mut stream, &mut inp).await?;
        hs.read_message(&inp[..n], &mut out)?;
        finish(stream, hs)
    }

    /// `data` must be at most 65519 bytes.
    pub async fn send(&mut self, data: &[u8]) -> Result<()> {
        let mut out = vec![0u8; data.len() + 16];
        let n = self.transport.write_message(data, &mut out)?;
        write_frame(&mut self.stream, &out[..n]).await
    }

    pub async fn recv(&mut self) -> Result<Vec<u8>> {
        let n = timeout(IDLE_TIMEOUT, read_frame(&mut self.stream, &mut self.buf)).await??;
        let mut plain = vec![0u8; n];
        let m = self.transport.read_message(&self.buf[..n], &mut plain)?;
        plain.truncate(m);
        Ok(plain)
    }
}
