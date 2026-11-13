use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct PairedPeer {
    pub name: String,
    /// Hex of the peer's 32 byte Noise static public key.
    pub public_key: String,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Config {
    pub device_name: String,
    /// Hex. Stored in plain text in config.json (see README, security notes).
    pub private_key: String,
    pub public_key: String,
    #[serde(default)]
    pub peers: Vec<PairedPeer>,
}

pub fn data_dir() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("Shelf")
}

pub fn inbox_dir() -> PathBuf {
    data_dir().join("inbox")
}

fn config_path() -> PathBuf {
    data_dir().join("config.json")
}

impl Config {
    pub fn load_or_create() -> Result<Config> {
        let path = config_path();
        if let Ok(text) = std::fs::read_to_string(&path) {
            match serde_json::from_str::<Config>(&text) {
                Ok(cfg) => return Ok(cfg),
                Err(_) => {
                    // Keep a copy of an unreadable file instead of silently destroying pairings.
                    let _ = std::fs::rename(&path, path.with_extension("json.bak"));
                }
            }
        }
        let params: snow::params::NoiseParams = crate::channel::NOISE_PARAMS.parse()?;
        let kp = snow::Builder::new(params).generate_keypair()?;
        let cfg = Config {
            device_name: std::env::var("COMPUTERNAME").unwrap_or_else(|_| "Windows PC".to_string()),
            private_key: hex::encode(&kp.private),
            public_key: hex::encode(&kp.public),
            peers: Vec::new(),
        };
        cfg.save()?;
        Ok(cfg)
    }

    pub fn save(&self) -> Result<()> {
        std::fs::create_dir_all(data_dir())?;
        let path = config_path();
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }

    pub fn find_peer(&self, public_key: &[u8]) -> Option<&PairedPeer> {
        let k = hex::encode(public_key);
        self.peers.iter().find(|p| p.public_key == k)
    }
}
