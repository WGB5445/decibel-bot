//! Network presets for Decibel REST/WebSocket endpoints and on-chain package addresses.

use anyhow::{Result, bail};
use aptos_sdk::{Aptos, AptosConfig};

pub struct NetworkProfile {
    pub id: &'static str,
    pub decibel_api_base: &'static str,
    pub decibel_ws_url: &'static str,
    pub gas_station_url: &'static str,
    pub package_address: &'static str,
    pub default_usdc_metadata: Option<&'static str>,
    pub requires_mainnet_confirm: bool,
}

const TESTNET: NetworkProfile = NetworkProfile {
    id: "testnet",
    decibel_api_base: "https://api.testnet.aptoslabs.com/decibel/api/v1",
    decibel_ws_url: "wss://api.testnet.aptoslabs.com/decibel/ws",
    gas_station_url: "https://api.testnet.aptoslabs.com/gs/v1",
    package_address: "0xe7da2794b1d8af76532ed95f38bfdf1136abfd8ea3a240189971988a83101b7f",
    default_usdc_metadata: Some(
        "0x5428acf5c112826d0c74ae1cd2de9030f53d1d01235e6c2621d967bf914ee1c8",
    ),
    requires_mainnet_confirm: false,
};

const MAINNET: NetworkProfile = NetworkProfile {
    id: "mainnet",
    decibel_api_base: "https://api.mainnet.aptoslabs.com/decibel/api/v1",
    decibel_ws_url: "wss://api.mainnet.aptoslabs.com/decibel/ws",
    gas_station_url: "https://api.mainnet.aptoslabs.com/gs/v1",
    package_address: "0x50ead22afd6ffd9769e3b3d6e0e64a2a350d68e8b102c4e72e33d0b8cfdfdb06",
    default_usdc_metadata: None,
    requires_mainnet_confirm: true,
};

pub struct NetworkRegistry {
    profiles: &'static [NetworkProfile],
}

impl NetworkRegistry {
    pub const DEFAULT: Self = Self {
        profiles: &[TESTNET, MAINNET],
    };

    pub fn resolve(&self, name: &str) -> Result<&'static NetworkProfile> {
        let id = name.trim().to_ascii_lowercase();
        self.profiles
            .iter()
            .find(|profile| profile.id == id)
            .ok_or_else(|| {
                anyhow::anyhow!("unsupported network {name}; expected mainnet or testnet")
            })
    }

    pub fn all_ids(&self) -> &[&'static str] {
        &["testnet", "mainnet"]
    }

    pub fn aptos(&self, profile: &NetworkProfile) -> Result<Aptos> {
        Ok(Aptos::new(match profile.id {
            "mainnet" => AptosConfig::mainnet(),
            "testnet" => AptosConfig::testnet(),
            other => bail!("unsupported execution network {other}; expected mainnet or testnet"),
        })?)
    }
}

pub fn default_registry() -> &'static NetworkRegistry {
    &NetworkRegistry::DEFAULT
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn network_profiles_expose_gas_station_urls() {
        let registry = NetworkRegistry::DEFAULT;
        let testnet = registry.resolve("testnet").unwrap();
        let mainnet = registry.resolve("mainnet").unwrap();
        assert_eq!(
            testnet.gas_station_url,
            "https://api.testnet.aptoslabs.com/gs/v1"
        );
        assert_eq!(
            mainnet.gas_station_url,
            "https://api.mainnet.aptoslabs.com/gs/v1"
        );
    }
}
