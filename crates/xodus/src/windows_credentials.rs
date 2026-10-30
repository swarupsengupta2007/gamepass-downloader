//! Windows credential storage.
//!
//! Secrets that fit in a generic credential go to Credential Manager. Larger
//! ones are protected with DPAPI and written under the user profile, with a
//! short marker left in Credential Manager so a later process can find them.

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use windows_sys::Win32::Foundation::LocalFree;
use windows_sys::Win32::Security::Cryptography::{
    CryptProtectData, CryptUnprotectData, CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN,
};

use crate::secrets::{SERVICE_NAME, get_entry};

const CREDENTIAL_LIMIT: usize = 2560;
const MARKER: &[u8] = b"XODUS_DPAPI_V1";
const ENTROPY: &[u8] = b"Xodus Service";
const SAMPLE_KEYS: &[&str] = &["device-tokens", "user-DA", "user-session"];

pub fn store_secret(user: &str, secret: &[u8]) -> Result<(), keyring_core::Error> {
    if secret.len() <= CREDENTIAL_LIMIT {
        get_entry(user)?.set_secret(secret)?;
        if let Some(path) = credential_file(user)
            && path.exists()
        {
            std::fs::remove_file(path).map_err(platform)?;
        }
        return Ok(());
    }
    let protected = protect(secret).map_err(platform)?;
    let path = credential_file(user).ok_or_else(missing_profile)?;
    write_private(&path, &protected).map_err(platform)?;
    get_entry(user)?.set_secret(MARKER)?;
    Ok(())
}

pub fn load_secret(user: &str) -> Result<Vec<u8>, keyring_core::Error> {
    let stored = get_entry(user)?.get_secret()?;
    if stored.as_slice() != MARKER {
        return Ok(stored);
    }
    let path = credential_file(user).ok_or_else(missing_profile)?;
    let protected = std::fs::read(&path).map_err(platform)?;
    unprotect(&protected).map_err(platform)
}

pub fn delete_secret(user: &str) -> Result<(), keyring_core::Error> {
    let removed = match get_entry(user)?.delete_credential() {
        Ok(()) | Err(keyring_core::Error::NoEntry) => Ok(()),
        Err(error) => Err(error),
    };
    if let Some(path) = credential_file(user)
        && path.exists()
    {
        std::fs::remove_file(path).map_err(platform)?;
    }
    removed
}

pub fn read_sample_entries() -> Option<(PathBuf, Vec<(String, Vec<u8>)>)> {
    let path = std::env::temp_dir().join("keyring-sample-store.ron");
    if !path.is_file() {
        return None;
    }
    let path_str = path.to_str()?.to_string();
    let store = keyring_core::sample::Store::new_with_backing(&path_str).ok()?;
    keyring_core::set_default_store(store);
    let mut found = Vec::new();
    for key in SAMPLE_KEYS {
        match keyring_core::Entry::new(SERVICE_NAME, key) {
            Ok(entry) => match entry.get_secret() {
                Ok(bytes) => found.push(((*key).to_string(), bytes)),
                Err(keyring_core::Error::NoEntry) => {}
                Err(_) => return None,
            },
            Err(_) => return None,
        }
    }
    Some((path, found))
}

pub fn import_sample_entries(saved: Option<(PathBuf, Vec<(String, Vec<u8>)>)>) {
    let Some((path, entries)) = saved else {
        return;
    };
    for (key, bytes) in &entries {
        if store_secret(key, bytes).is_err() {
            eprintln!("Could not move the saved sign-in into Windows Credential Manager. Sign in again.");
            return;
        }
    }
    let _ = std::fs::remove_file(path);
}

pub fn credential_file(user: &str) -> Option<PathBuf> {
    let mut path = profile_dir()?;
    path.push(".xodus");
    path.push("credentials");
    path.push(file_name(user));
    Some(path)
}

fn profile_dir() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("LOCALAPPDATA"))
        .map(PathBuf::from)
}

fn file_name(user: &str) -> String {
    let digest = Sha256::digest(user.as_bytes());
    let mut name = String::from("cred-");
    for byte in digest {
        name.push_str(&format!("{byte:02x}"));
    }
    name
}

fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension("tmp");
    std::fs::write(&temporary, bytes)?;
    if path.exists() {
        std::fs::remove_file(path)?;
    }
    if let Err(error) = std::fs::rename(&temporary, path) {
        let _ = std::fs::remove_file(&temporary);
        return Err(error);
    }
    Ok(())
}

fn protect(secret: &[u8]) -> std::io::Result<Vec<u8>> {
    transform(secret, true)
}

fn unprotect(secret: &[u8]) -> std::io::Result<Vec<u8>> {
    transform(secret, false)
}

fn transform(secret: &[u8], protect_data: bool) -> std::io::Result<Vec<u8>> {
    let mut input_bytes = secret.to_vec();
    let mut entropy = ENTROPY.to_vec();
    let mut input = blob(&mut input_bytes);
    let mut entropy_blob = blob(&mut entropy);
    let mut output = CRYPT_INTEGER_BLOB {
        cbData: 0,
        pbData: std::ptr::null_mut(),
    };
    let status = unsafe {
        if protect_data {
            CryptProtectData(
                &input,
                std::ptr::null(),
                &entropy_blob,
                std::ptr::null_mut(),
                std::ptr::null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output,
            )
        } else {
            CryptUnprotectData(
                &mut input,
                std::ptr::null_mut(),
                &mut entropy_blob,
                std::ptr::null_mut(),
                std::ptr::null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output,
            )
        }
    };
    if status == 0 {
        return Err(std::io::Error::last_os_error());
    }
    if output.pbData.is_null() {
        return Err(std::io::Error::other("Windows could not protect the saved sign-in."));
    }
    let protected = unsafe { std::slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec() };
    unsafe {
        LocalFree(output.pbData as *mut _);
    }
    Ok(protected)
}

fn blob(bytes: &mut [u8]) -> CRYPT_INTEGER_BLOB {
    CRYPT_INTEGER_BLOB {
        cbData: bytes.len() as u32,
        pbData: bytes.as_mut_ptr(),
    }
}

fn platform(error: std::io::Error) -> keyring_core::Error {
    keyring_core::Error::PlatformFailure(Box::new(error))
}

fn missing_profile() -> keyring_core::Error {
    keyring_core::Error::Invalid("profile".into(), "The user profile is unavailable.".into())
}
