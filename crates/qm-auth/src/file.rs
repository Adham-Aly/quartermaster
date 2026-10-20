//! The credential file: `<data dir>/auth.json`, mode 0600.
//!
//! ```json
//! {"version":1,"credentials":{"chatgpt:dev":{"type":"oauth","access_token":"…",
//!   "refresh_token":"…","expires_at":"…","account_id":null,"client_id":"…"}}}
//! ```
//!
//! - Opening refuses a file whose mode isn't exactly 0600 ([`AuthError::Permissions`]), or that
//!   isn't a regular file (a symlink, say); a missing file is an empty store.
//! - Every `put` and `remove` rewrites the whole file: a temporary file in the same folder created
//!   with mode 0600 and then set to 0600 explicitly (so no umask, however permissive or strict,
//!   changes it), written, `fsync`ed, renamed over the old one, then the folder `fsync`ed. A crash
//!   leaves the old file or the new one, never half of either; a temporary file a crash left
//!   behind is removed at the next write.
//! - Each read-modify-write holds an exclusive `flock` on `.auth.json.lock` beside the file, on top
//!   of an in-process mutex, so two stores on the same file (two sessions, or two handles in one
//!   process) never lose each other's update: concurrent token refreshes of different accounts both
//!   land.
//! - A folder the store creates gets mode 0700.
//! - Every read goes to the file, so a refresh written by another session is seen at once.
//! - The contents never reach a log, an event or an agent's environment: errors name the file and
//!   what is wrong with it, never a value.

use crate::{AuthError, Credential, CredentialRef, CredentialStore, Secret};
use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::io::Write as _;
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

const FORMAT_VERSION: u32 = 1;

/// A credential store backed by one JSON file only you can read.
#[derive(Debug)]
pub struct FileCredentialStore {
    path: PathBuf,
    lock: Mutex<()>,
}

#[derive(Serialize, Deserialize)]
struct StoredFile {
    version: u32,
    #[serde(default)]
    credentials: BTreeMap<String, StoredCredential>,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum StoredCredential {
    ApiKey {
        key: String,
    },
    #[serde(rename = "oauth")]
    OAuth {
        access_token: String,
        #[serde(default)]
        refresh_token: Option<String>,
        #[serde(default)]
        expires_at: Option<Timestamp>,
        #[serde(default)]
        account_id: Option<String>,
        /// The client ID Sign in with ChatGPT issued; absent in older files.
        #[serde(default)]
        client_id: Option<String>,
    },
}

impl StoredCredential {
    fn from_credential(credential: &Credential) -> Self {
        match credential {
            Credential::ApiKey { key } => Self::ApiKey {
                key: key.expose().to_owned(),
            },
            Credential::OAuth {
                access_token,
                refresh_token,
                expires_at,
                account_id,
                client_id,
            } => Self::OAuth {
                access_token: access_token.expose().to_owned(),
                refresh_token: refresh_token.as_ref().map(|s| s.expose().to_owned()),
                expires_at: *expires_at,
                account_id: account_id.clone(),
                client_id: client_id.clone(),
            },
        }
    }

    fn into_credential(self) -> Credential {
        match self {
            Self::ApiKey { key } => Credential::ApiKey {
                key: Secret::new(key),
            },
            Self::OAuth {
                access_token,
                refresh_token,
                expires_at,
                account_id,
                client_id,
            } => Credential::OAuth {
                access_token: Secret::new(access_token),
                refresh_token: refresh_token.map(Secret::new),
                expires_at,
                account_id,
                client_id,
            },
        }
    }
}

impl FileCredentialStore {
    /// Opens the store at `path` (normally `DataDir::auth_file()`). A missing file is an empty
    /// store, created on the first `put`. An existing file must have mode 0600 and parse.
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, AuthError> {
        let store = Self {
            path: path.into(),
            lock: Mutex::new(()),
        };
        store.read()?;
        Ok(store)
    }

    /// Where the file is.
    pub fn path(&self) -> &Path {
        &self.path
    }

    fn guard(&self) -> MutexGuard<'_, ()> {
        // The lock guards no data of its own; a panic elsewhere leaves nothing half-done here.
        self.lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The credential keys in the file (never the secrets), sorted.
    pub fn keys(&self) -> Result<Vec<CredentialRef>, AuthError> {
        let _guard = self.guard();
        let file = self.read()?;
        Ok(file
            .credentials
            .keys()
            .filter_map(|k| CredentialRef::new(k.clone()).ok())
            .collect())
    }

    fn dir(&self) -> &Path {
        self.path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."))
    }

    fn file_name(&self) -> String {
        self.path
            .file_name()
            .map_or_else(|| "auth.json".into(), |n| n.to_string_lossy().into_owned())
    }

    /// Creates the folder (0700) if needed and takes the cross-process write lock; released when
    /// the returned file is dropped.
    fn lock_for_write(&self) -> Result<fs::File, AuthError> {
        let dir = self.dir();
        if !dir.exists() {
            fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(dir)?;
        }
        let lock = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(dir.join(format!(".{}.lock", self.file_name())))?;
        lock.lock()?;
        Ok(lock)
    }

    /// Reads, changes and writes the file under both locks.
    fn update(&self, change: impl FnOnce(&mut StoredFile) -> bool) -> Result<(), AuthError> {
        let _guard = self.guard();
        let _lock = self.lock_for_write()?;
        let mut file = self.read()?;
        if change(&mut file) {
            self.remove_stale_temp_files();
            self.write(&file)?;
        }
        Ok(())
    }

    /// Removes temporary files a crash left between writing and renaming. Runs under the write
    /// lock, so no other writer's temporary file is in flight.
    fn remove_stale_temp_files(&self) {
        let prefix = format!(".{}.tmp-", self.file_name());
        let Ok(entries) = fs::read_dir(self.dir()) else {
            return;
        };
        for entry in entries.flatten() {
            if entry.file_name().to_string_lossy().starts_with(&prefix) {
                let _ = fs::remove_file(entry.path());
            }
        }
    }

    fn read(&self) -> Result<StoredFile, AuthError> {
        let metadata = match fs::symlink_metadata(&self.path) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(StoredFile {
                    version: FORMAT_VERSION,
                    credentials: BTreeMap::new(),
                });
            }
            Err(e) => return Err(AuthError::Io(e)),
        };
        if !metadata.file_type().is_file() {
            return Err(AuthError::Corrupt(format!(
                "{} is not a regular file",
                self.path.display()
            )));
        }
        let mode = metadata.permissions().mode() & 0o777;
        if mode != 0o600 {
            return Err(AuthError::Permissions { mode });
        }
        let text = fs::read_to_string(&self.path)?;
        let file: StoredFile = serde_json::from_str(&text).map_err(|e| {
            // serde_json's message names a line and column, never the value.
            AuthError::Corrupt(format!(
                "{} is not valid (line {}, column {})",
                self.path.display(),
                e.line(),
                e.column()
            ))
        })?;
        if file.version != FORMAT_VERSION {
            return Err(AuthError::Corrupt(format!(
                "{} has format version {}, and this build reads version {FORMAT_VERSION}",
                self.path.display(),
                file.version
            )));
        }
        Ok(file)
    }

    fn write(&self, file: &StoredFile) -> Result<(), AuthError> {
        let dir = self.dir();
        let name = self.file_name();
        let tmp = dir.join(format!(
            ".{name}.tmp-{}-{}",
            std::process::id(),
            crate::pkce::random_hex(6)
        ));
        let json = serde_json::to_vec_pretty(file)
            .map_err(|e| AuthError::Corrupt(format!("couldn't encode the credentials: {e}")))?;
        let result = (|| -> std::io::Result<()> {
            let mut f = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&tmp)?;
            // `mode` above is filtered by the umask; this isn't.
            f.set_permissions(fs::Permissions::from_mode(0o600))?;
            f.write_all(&json)?;
            f.write_all(b"\n")?;
            f.sync_all()?;
            drop(f);
            fs::rename(&tmp, &self.path)?;
            fs::File::open(dir)?.sync_all()
        })();
        if result.is_err() {
            let _ = fs::remove_file(&tmp);
        }
        result.map_err(AuthError::Io)
    }
}

impl CredentialStore for FileCredentialStore {
    fn get(&self, key: &CredentialRef) -> Result<Option<Credential>, AuthError> {
        let _guard = self.guard();
        let mut file = self.read()?;
        Ok(file
            .credentials
            .remove(key.as_str())
            .map(StoredCredential::into_credential))
    }

    fn put(&self, key: &CredentialRef, credential: Credential) -> Result<(), AuthError> {
        let stored = StoredCredential::from_credential(&credential);
        self.update(|file| {
            file.credentials.insert(key.as_str().to_owned(), stored);
            true
        })
    }

    fn remove(&self, key: &CredentialRef) -> Result<(), AuthError> {
        self.update(|file| file.credentials.remove(key.as_str()).is_some())
    }
}
