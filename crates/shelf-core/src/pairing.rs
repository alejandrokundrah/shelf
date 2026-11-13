//! Pairing with a 6 digit code using SPAKE2 (a password-authenticated key exchange).
//! The code never crosses the network and a wrong guess reveals nothing that can be
//! brute-forced offline. After the key exchange both sides swap their long-term
//! public keys, authenticated with an HMAC under the shared key.

use crate::store::{Config, PairedPeer};
use anyhow::{anyhow, bail, Result};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use spake2::{Ed25519Group, Identity as SpakeIdentity, Password, Spake2};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

type HmacSha256 = Hmac<Sha256>;

#[derive(Serialize, Deserialize)]
struct Hello {
    name: String,
    public_key: String,
}

async fn write_plain(s: &mut TcpStream, data: &[u8]) -> Result<()> {
    s.write_u16(data.len() as u16).await?;
    s.write_all(data).await?;
    Ok(())
}

async fn read_plain(s: &mut TcpStream) -> Result<Vec<u8>> {
    let n = s.read_u16().await? as usize;
    if n > 4096 {
        bail!("pairing message too large");
    }
    let mut buf = vec![0u8; n];
    s.read_exact(&mut buf).await?;
    Ok(buf)
}

fn tag(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut m = HmacSha256::new_from_slice(key).expect("hmac accepts any key length");
    m.update(b"shelf-hello-v1");
    m.update(data);
    m.finalize().into_bytes().to_vec()
}

fn verify(key: &[u8], data: &[u8], expected: &[u8]) -> bool {
    let mut m = HmacSha256::new_from_slice(key).expect("hmac accepts any key length");
    m.update(b"shelf-hello-v1");
    m.update(data);
    m.verify_slice(expected).is_ok()
}

/// Both sides run exactly the same steps (SPAKE2 symmetric mode).
pub async fn run(mut stream: TcpStream, code: &str, me: &Config) -> Result<PairedPeer> {
    let (state, outbound) = Spake2::<Ed25519Group>::start_symmetric(
        &Password::new(code.as_bytes()),
        &SpakeIdentity::new(b"shelf-pairing-v1"),
    );
    write_plain(&mut stream, &outbound).await?;
    let inbound = read_plain(&mut stream).await?;
    let key = state
        .finish(&inbound)
        .map_err(|e| anyhow!("pairing failed: {e:?}"))?;

    let hello = serde_json::to_vec(&Hello {
        name: me.device_name.clone(),
        public_key: me.public_key.clone(),
    })?;
    write_plain(&mut stream, &hello).await?;
    write_plain(&mut stream, &tag(&key, &hello)).await?;

    let their_hello = read_plain(&mut stream).await?;
    let their_tag = read_plain(&mut stream).await?;
    if !verify(&key, &their_hello, &their_tag) {
        bail!("wrong pairing code");
    }

    let h: Hello = serde_json::from_slice(&their_hello)?;
    let pk = hex::decode(&h.public_key).map_err(|_| anyhow!("bad key from peer"))?;
    if pk.len() != 32 {
        bail!("bad key from peer");
    }
    let public_key = hex::encode(pk);
    if public_key == me.public_key {
        bail!("cannot pair a device with itself");
    }
    let name: String = h.name.chars().take(40).collect();
    Ok(PairedPeer { name, public_key })
}
