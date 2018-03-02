extern crate althea_types;
extern crate config;
extern crate eui48;
extern crate num256;
extern crate toml;

#[macro_use]
extern crate serde_derive;

extern crate serde;
extern crate serde_json;

extern crate althea_kernel_interface;

use std::net::IpAddr;
use std::path::Path;
use std::fs::File;
use std::io::Write;

use config::{Config, ConfigError, Environment};

use althea_types::{EthAddress, Identity};

use eui48::MacAddress;

use num256::Int256;

use serde::{Deserialize, Serialize};

use althea_kernel_interface::KernelInterface;

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct NetworkSettings {
    pub own_ip: IpAddr,
    pub bounty_ip: IpAddr,
    pub babel_port: u16,
    pub rita_port: u16,
    pub bounty_port: u16,
    pub wg_private_key: String,
    pub wg_start_port: u16,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct PaymentSettings {
    pub pay_threshold: Int256,
    pub close_threshold: Int256,
    pub close_fraction: Int256,
    pub buffer_period: u32,
    pub eth_address: EthAddress,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ExitNetworkSettings {
    pub wg_tunnel_port: u16,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ExitClientSettings {
    pub exit_ip: IpAddr,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct RitaSettings {
    pub payment: PaymentSettings,
    pub network: NetworkSettings,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_client: Option<ExitClientSettings>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct RitaExitSettings {
    pub payment: PaymentSettings,
    pub network: NetworkSettings,
    pub exit_network: ExitNetworkSettings,
    pub db_file: String,
}

impl RitaSettings {
    pub fn new(file_name: &str, default: &str) -> Result<Self, ConfigError> {
        let mut s = Config::new();
        s.merge(config::File::with_name(default))?;
        s.merge(config::File::with_name(file_name).required(false))?;
        let settings: Self = s.try_into()?;

        let mut ki = KernelInterface {};
        ki.create_wg_key(Path::new(&settings.network.wg_private_key));
        Ok(settings)
    }

    pub fn get_identity(&self) -> Identity {
        let mut ki = KernelInterface {};
        ki.create_wg_key(Path::new(&self.network.wg_private_key));

        Identity::new(self.network.own_ip.clone(), self.payment.eth_address.clone(),
                      ki.get_wg_pubkey(Path::new(&self.network.wg_private_key))
                                .unwrap()
        )
    }

    pub fn write(&self, file_name: &str) -> Result<(), std::io::Error> {
        let ser = toml::to_string(&self).unwrap();
        let mut file = File::create(file_name)?;
        file.write_all(ser.as_bytes())?;
        Ok(())
    }
}

impl RitaExitSettings {
    pub fn new(file_name: &str, default: &str) -> Result<Self, ConfigError> {
        let mut s = Config::new();
        s.merge(config::File::with_name(default))?;
        s.merge(config::File::with_name(file_name).required(false))?;
        let settings: Self = s.try_into()?;

        let mut ki = KernelInterface {};
        ki.create_wg_key(Path::new(&settings.network.wg_private_key));
        Ok(settings)
    }

    pub fn get_identity(&self) -> Identity {
        let mut ki = KernelInterface {};
        ki.create_wg_key(Path::new(&self.network.wg_private_key));

        Identity::new(self.network.own_ip.clone(), self.payment.eth_address.clone(),
                      ki.get_wg_pubkey(Path::new(&self.network.wg_private_key))
                                .unwrap()
        )
    }

    pub fn write(&self, file_name: &str) -> Result<(), std::io::Error> {
        let ser = toml::to_string(&self).unwrap();
        let mut file = File::create(file_name)?;
        file.write_all(ser.as_bytes())?;
        Ok(())
    }
}