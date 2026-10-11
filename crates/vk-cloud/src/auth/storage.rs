//! Explicit cloud-token file storage. A signed-out marker prevents an older keychain
//! token from becoming active again when a locked keychain later becomes available.

use super::{Secret, item};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use vk_store::keychain::{Keychain, KeychainError};

#[derive(Debug, Clone)]
pub struct Credentials {
    pub(super) keychain: Keychain,
    directory: PathBuf,
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("{0}")]
    Keychain(KeychainError),
    #[error("cloud credential file: {0}")]
    File(KeychainError),
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "storage", rename_all = "snake_case")]
pub(super) enum Saved {
    File { token: String },
    SignedOut,
}

impl Credentials {
    pub fn new(keychain: Keychain, directory: PathBuf) -> Self {
        Self {
            keychain,
            directory,
        }
    }

    fn file(&self, provider: &str) -> Result<Keychain, KeychainError> {
        if provider.is_empty()
            || !provider
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        {
            return Err(KeychainError::Invalid("invalid cloud provider name".into()));
        }
        Ok(Keychain::File(
            self.directory.join(format!("{provider}.json")),
        ))
    }

    pub(super) fn saved(&self, provider: &str) -> Result<Option<Saved>, KeychainError> {
        if !self.check_directory(false)? {
            return Ok(None);
        }
        self.file(provider)?
            .get_item("credential")?
            .map(|s| {
                serde_json::from_str(&s).map_err(|_| {
                    KeychainError::Failed("the cloud credential file is invalid".into())
                })
            })
            .transpose()
    }

    fn check_directory(&self, create: bool) -> Result<bool, KeychainError> {
        use std::os::unix::fs::{DirBuilderExt, MetadataExt};
        if create {
            if let Some(parent) = self.directory.parent() {
                std::fs::DirBuilder::new()
                    .recursive(true)
                    .mode(0o700)
                    .create(parent)
                    .map_err(|_| {
                        KeychainError::Failed(
                            "could not create the cloud credential directory".into(),
                        )
                    })?;
            }
            match std::fs::DirBuilder::new()
                .mode(0o700)
                .create(&self.directory)
            {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(_) => {
                    return Err(KeychainError::Failed(
                        "could not create the cloud credential directory".into(),
                    ));
                }
            }
        }
        let md = match std::fs::symlink_metadata(&self.directory) {
            Ok(md) => md,
            Err(e) if !create && e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(_) => {
                return Err(KeychainError::Failed(
                    "could not read the cloud credential directory".into(),
                ));
            }
        };
        // SAFETY: getuid has no preconditions.
        if !md.is_dir() || md.uid() != unsafe { libc::getuid() } || md.mode() & 0o077 != 0 {
            return Err(KeychainError::Permission(
                "the cloud credential directory must be owned by you with mode 0700".into(),
            ));
        }
        Ok(true)
    }

    fn save(&self, provider: &str, value: &Saved) -> Result<(), KeychainError> {
        self.check_directory(true)?;
        let json = serde_json::to_string(value)
            .map_err(|_| KeychainError::Failed("could not encode the cloud credential".into()))?;
        self.file(provider)?.set_item("credential", &json)
    }

    /// Used only after an explicit request for file storage. Does not touch the OS keychain.
    pub fn store_file(&self, provider: &str, secret: &Secret) -> Result<(), KeychainError> {
        self.save(
            provider,
            &Saved::File {
                token: secret.expose().to_string(),
            },
        )
    }

    pub fn store(&self, provider: &str, secret: &Secret) -> Result<(), StoreError> {
        // Refuse an unsafe or corrupt file before changing the keychain.
        let saved = self.saved(provider).map_err(StoreError::File)?;
        self.keychain
            .set_item(&item(provider), secret.expose())
            .map_err(StoreError::Keychain)?;
        if saved.is_some() {
            self.file(provider)
                .map_err(StoreError::File)?
                .delete_item("credential")
                .map_err(StoreError::File)?;
        }
        Ok(())
    }

    // A locked keychain must not prevent file sign-out. Keep the marker even after
    // successful deletion; another keychain in the search list may hold an older item.
    fn clear_old_keychain(&self, provider: &str) -> Result<bool, KeychainError> {
        match self.keychain.delete_item(&item(provider)) {
            Err(
                KeychainError::Unsupported(_) | KeychainError::Failed(_) | KeychainError::Timeout,
            ) => Ok(false),
            result => result,
        }
    }

    pub fn clear(&self, provider: &str) -> Result<bool, KeychainError> {
        match self.saved(provider)? {
            Some(Saved::File { .. }) => {
                self.save(provider, &Saved::SignedOut)?;
                self.clear_old_keychain(provider)?;
                Ok(true)
            }
            Some(Saved::SignedOut) => self.clear_old_keychain(provider),
            None => self.keychain.delete_item(&item(provider)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        CloudConfig,
        auth::{Source, resolve},
        fake::Fake,
    };
    use std::os::unix::fs::{PermissionsExt, symlink};

    #[test]
    fn file_choice_survives_restart_and_logout_does_not_revive_an_old_keychain_token() {
        let d = tempfile::tempdir().unwrap();
        let keychain = Keychain::File(d.path().join("keychain.json"));
        let directory = d.path().join("credentials");
        let c = Credentials::new(keychain.clone(), directory.clone());
        let p = Fake::new(d.path().join("fake"));
        let cfg = CloudConfig::default();
        c.store("fake", &Secret::new("old-token")).unwrap();
        c.store_file("fake", &Secret::new("new-token")).unwrap();
        assert_eq!(
            std::fs::metadata(&directory).unwrap().permissions().mode() & 0o777,
            0o700
        );
        let path = directory.join("fake.json");
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let c = Credentials::new(keychain, directory);
        let (secret, source) = resolve(&p, &cfg, &c, &|_| None).unwrap().unwrap();
        assert_eq!(secret.expose(), "new-token");
        assert_eq!(source, Source::File);
        assert!(c.clear("fake").unwrap());
        assert!(c.keychain.get_item(&item("fake")).unwrap().is_none());
        // A credential that only becomes accessible after sign-out stays suppressed,
        // and another sign-out can remove it.
        c.keychain
            .set_item(&item("fake"), "late-old-token")
            .unwrap();
        assert!(resolve(&p, &cfg, &c, &|_| None).unwrap().is_none());
        assert!(c.clear("fake").unwrap());
        assert!(!c.clear("fake").unwrap());
        assert!(resolve(&p, &cfg, &c, &|_| None).unwrap().is_none());
        assert!(
            !std::fs::read_to_string(&path)
                .unwrap()
                .contains("new-token")
        );
        c.store("fake", &Secret::new("latest-token")).unwrap();
        let (secret, source) = resolve(&p, &cfg, &c, &|_| None).unwrap().unwrap();
        assert_eq!(secret.expose(), "latest-token");
        assert_eq!(source, Source::Keychain);
    }

    #[test]
    fn file_storage_and_logout_work_without_a_usable_keychain() {
        let d = tempfile::tempdir().unwrap();
        let broken = d.path().join("bad.json");
        std::fs::write(&broken, "bad json").unwrap();
        std::fs::set_permissions(&broken, std::fs::Permissions::from_mode(0o600)).unwrap();
        let c = Credentials::new(Keychain::File(broken), d.path().join("credentials"));
        assert!(c.store("fake", &Secret::new("token")).is_err());
        assert!(!c.directory.exists());
        c.store_file("fake", &Secret::new("token")).unwrap();
        assert!(c.clear("fake").unwrap());
    }

    #[test]
    fn unsafe_files_and_directories_fail_closed() {
        let d = tempfile::tempdir().unwrap();
        let c = Credentials::new(
            Keychain::File(d.path().join("kc")),
            d.path().join("credentials"),
        );
        c.store_file("fake", &Secret::new("SENTINEL")).unwrap();
        let path = c.directory.join("fake.json");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(matches!(c.saved("fake"), Err(KeychainError::Permission(_))));
        assert!(c.store_file("fake", &Secret::new("replacement")).is_err());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let target = d.path().join("target");
        std::fs::rename(&path, &target).unwrap();
        symlink(&target, &path).unwrap();
        assert!(c.saved("fake").is_err());
        assert!(c.store_file("fake", &Secret::new("replacement")).is_err());
        assert!(
            std::fs::read_to_string(&target)
                .unwrap()
                .contains("SENTINEL")
        );
        std::fs::set_permissions(&c.directory, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(c.saved("fake").is_err());
        assert!(c.store_file("fake", &Secret::new("replacement")).is_err());
    }
}
