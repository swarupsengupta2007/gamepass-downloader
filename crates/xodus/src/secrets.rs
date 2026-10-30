pub static SERVICE_NAME: &str = "Xodus Service";

pub fn init_secrets() -> Result<(), keyring_core::Error> {
    #[cfg(feature = "key-chain-file")]
    {
        let store = keyring_core::sample::Store::new_with_backing(
            secrets_backing_file()
                .to_str()
                .expect("Invalid secrets backing path"),
        )?;
        keyring_core::set_default_store(store);
    }

    #[cfg(not(feature = "key-chain-file"))]
    {
        #[cfg(target_os = "linux")]
        {
            activate_desktop_keyring();
            keyring_core::set_default_store(dbus_secret_service_keyring_store::Store::new()?);
        }

        #[cfg(target_os = "macos")]
        {
            keyring_core::set_default_store(apple_native_keyring_store::keychain::Store::new()?);
        }

        #[cfg(windows)]
        {
            let saved = crate::windows_credentials::read_sample_entries();
            keyring_core::set_default_store(windows_native_keyring_store::Store::new()?);
            crate::windows_credentials::import_sample_entries(saved);
        }

        #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
        {
            let store = keyring_core::sample::Store::new_with_configuration(
                &std::collections::HashMap::from([("persist", "true")]),
            )?;
            keyring_core::set_default_store(store);
        }
    }

    Ok(())
}

// Some KDE installations advertise ksecretd only under its compatibility
// activation name. Respect an existing/default Secret Service provider first.
// This starts a packaged user service, never installs or configures a keyring.
#[cfg(all(target_os = "linux", not(feature = "key-chain-file")))]
fn activate_desktop_keyring() {
    use dbus::blocking::Connection;
    use std::time::Duration;
    let Ok(connection) = Connection::new_session() else { return; };
    let bus = connection.with_proxy("org.freedesktop.DBus", "/org/freedesktop/DBus", Duration::from_secs(3));
    let Ok((owned,)): Result<(bool,), _> = bus.method_call("org.freedesktop.DBus", "NameHasOwner", ("org.freedesktop.secrets",)) else { return; };
    if owned { return; }
    let Ok((names,)): Result<(Vec<String>,), _> = bus.method_call("org.freedesktop.DBus", "ListActivatableNames", ()) else { return; };
    if names.iter().any(|name| name == "org.freedesktop.secrets") { return; }
    if names.iter().any(|name| name == "org.kde.secretservicecompat") {
        // On failure, Store::new below still returns the genuine keyring error.
        let _: Result<(u32,), _> = bus.method_call("org.freedesktop.DBus", "StartServiceByName", ("org.kde.secretservicecompat", 0u32));
    }
}

pub fn get_entry(user: &str) -> Result<keyring_core::Entry, keyring_core::Error> {
    keyring_core::Entry::new(SERVICE_NAME, user)
}

pub fn store_secret(user: &str, secret: &[u8]) -> Result<(), keyring_core::Error> {
    #[cfg(all(windows, not(feature = "key-chain-file")))]
    {
        return crate::windows_credentials::store_secret(user, secret);
    }
    #[cfg(not(all(windows, not(feature = "key-chain-file"))))]
    {
        get_entry(user)?.set_secret(secret)
    }
}

pub fn load_secret(user: &str) -> Result<Vec<u8>, keyring_core::Error> {
    #[cfg(all(windows, not(feature = "key-chain-file")))]
    {
        return crate::windows_credentials::load_secret(user);
    }
    #[cfg(not(all(windows, not(feature = "key-chain-file"))))]
    {
        get_entry(user)?.get_secret()
    }
}

pub fn delete_secret(user: &str) -> Result<(), keyring_core::Error> {
    #[cfg(all(windows, not(feature = "key-chain-file")))]
    {
        return crate::windows_credentials::delete_secret(user);
    }
    #[cfg(not(all(windows, not(feature = "key-chain-file"))))]
    {
        get_entry(user)?.delete_credential()
    }
}

pub fn destroy_secrets() {
    keyring_core::unset_default_store();
}

#[cfg(feature = "key-chain-file")]
fn secrets_backing_file() -> std::path::PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join(".xodus-keyring.ron")
}
