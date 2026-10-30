#[cfg(windows)]
pub mod adopt;
pub mod clep;
pub mod download;
pub mod extract;
pub mod extract_eappx;
pub mod login;
pub mod logout;
pub mod profile;
#[cfg(windows)]
pub mod queue;
pub mod game_activity;
pub mod achievements;
pub mod subscription;
pub mod session;
pub mod library;
#[cfg(unix)]
pub mod run;
pub mod streaming;
pub mod verify;
