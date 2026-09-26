//! 本机凭据的读取：Factory 登录态（系统钥匙串里的 AES key + 手搓 GCM 解密）、
//! Devin token（credentials.toml / state.vscdb），以及 VS Code 系 `state.vscdb`
//! 的通用读取。拆成子模块只是为了文件别太长，`crate::probes::credentials::*` 的公开
//! 路径保持不变。

mod devin_token;
mod factory_auth;
mod gcm;
mod keyring;
mod vscdb;

pub use devin_token::load_devin_token;
pub use factory_auth::load_factory_credentials;
pub use keyring::load_factory_key;
pub(crate) use vscdb::{open_vscdb_readonly, read_vscdb_items};

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FactoryAuth {
    pub access_token: String,
    pub active_organization_id: Option<String>,
}

fn get_home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}
