//! Choices the window keeps between runs, in
//! `%APPDATA%\gamepass-downloader\settings.json`. They belong to the account
//! that was signed in when they were made: kept only while signed in, and
//! deleted on sign-out.

use std::path::PathBuf;

use serde_json::{Value, json};

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Settings {
    /// The signed-in Microsoft account these choices were made under.
    pub account: Option<String>,
    /// The Store market picked in the list; `None` follows the account.
    pub market: Option<String>,
    pub parallel: Option<usize>,
    pub keep_download: Option<bool>,
}

fn path() -> Option<PathBuf> {
    Some(PathBuf::from(std::env::var_os("APPDATA")?).join("gamepass-downloader").join("settings.json"))
}

/// What was saved, with anything missing or malformed left at `None`.
pub fn load() -> Settings {
    let Some(value) = path().and_then(|p| std::fs::read(p).ok()).and_then(|b| serde_json::from_slice::<Value>(&b).ok()) else {
        return Settings::default();
    };
    let market = value["market"].as_str().filter(|c| c.len() == 2 && c.bytes().all(|b| b.is_ascii_uppercase()));
    Settings {
        account: value["account"].as_str().map(str::to_owned),
        market: market.map(str::to_owned),
        parallel: value["parallel"].as_u64().filter(|n| (1..=32).contains(n)).map(|n| n as usize),
        keep_download: value["keepDownload"].as_bool(),
    }
}

pub fn save(settings: &Settings) -> Result<(), String> {
    let path = path().ok_or("APPDATA is not set")?;
    let dir = path.parent().expect("settings file has a folder");
    std::fs::create_dir_all(dir).map_err(|e| format!("cannot save settings: {e}"))?;
    let value = json!({
        "account": settings.account,
        "market": settings.market,
        "parallel": settings.parallel,
        "keepDownload": settings.keep_download,
    });
    let temp = path.with_extension("json.tmp");
    std::fs::write(&temp, value.to_string()).map_err(|e| format!("cannot save settings: {e}"))?;
    std::fs::rename(&temp, &path).map_err(|e| format!("cannot save settings: {e}"))
}

/// Forgets every saved choice (on sign-out).
pub fn clear() {
    if let Some(path) = path() {
        std::fs::remove_file(path).ok();
    }
}
