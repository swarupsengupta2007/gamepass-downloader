pub mod api;
pub mod auth;
pub mod clep;
pub mod hardware;
#[cfg(target_os = "linux")]
mod keyring_persistence;
#[cfg(target_os = "linux")]
pub mod keyring_session;
#[cfg(target_os = "linux")]
pub mod keyring_unlock;
pub mod licensing;
pub mod models;
pub mod secrets;
#[cfg(all(windows, not(feature = "key-chain-file")))]
mod windows_credentials;
pub mod tokens;

pub const XBOX_LIVE_PACKAGES_PC: &str = "https://packagespc.xboxlive.com";

pub use xal;

pub mod proto {
    pub mod xodus {
        include!(concat!(env!("OUT_DIR"), "/xodus.rs"));
    }
}
