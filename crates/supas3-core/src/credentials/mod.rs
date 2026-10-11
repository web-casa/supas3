pub mod keychain;
pub mod manager;

pub use keychain::KeychainStorage;
pub use manager::{CredentialType, Profile, ProfileManager};

use crate::error::Result;
use crate::host::AppHandle;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;

pub(crate) fn write_private_file(path: &std::path::Path, content: &[u8]) -> Result<()> {
    use std::io::Write;
    let parent = path
        .parent()
        .ok_or_else(|| crate::error::AppError::IoError("Missing storage directory".to_string()))?;
    std::fs::create_dir_all(parent)?;
    let mut replacement = tempfile::NamedTempFile::new_in(parent)?;
    replacement.write_all(content)?;
    replacement.as_file().sync_all()?;
    replacement
        .persist(path)
        .map_err(|error| crate::error::AppError::IoError(error.error.to_string()))?;
    Ok(())
}

fn portable_data_dir() -> Option<PathBuf> {
    let env_portable = std::env::var("SUPAS3_PORTABLE")
        .map(|value| matches!(value.to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
        .unwrap_or(false);

    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(|parent| parent.to_path_buf()))?;

    if env_portable || exe_dir.join("supas3.portable").exists() {
        Some(exe_dir.join("data"))
    } else {
        None
    }
}

/// Initialize the credentials manager and register it as app state.
/// This must happen before any profile command runs. `default_config_dir` is
/// used unless portable mode redirects storage next to the executable.
pub fn init(app: &AppHandle, default_config_dir: PathBuf) -> Result<PathBuf> {
    // When both Linux backends are compiled, make the persistent desktop
    // Secret Service selection explicit. Kernel keyutils is retained only so
    // KeychainStorage can migrate credentials written by older supaS3 builds.
    #[cfg(target_os = "linux")]
    keyring::set_default_credential_builder(keyring::secret_service::default_credential_builder());

    let portable_config_dir = portable_data_dir();
    let force_secret_fallback = portable_config_dir.is_some();
    let config_dir = match portable_config_dir {
        Some(path) => path,
        None => default_config_dir,
    };

    // Ensure config directory exists
    std::fs::create_dir_all(&config_dir)?;

    let manager = ProfileManager::new(config_dir.clone(), force_secret_fallback)?;
    let state = Arc::new(RwLock::new(manager));

    app.manage(state);

    log::info!(
        "Credentials manager initialized{}",
        if force_secret_fallback {
            " in portable mode"
        } else {
            ""
        }
    );
    Ok(config_dir)
}
