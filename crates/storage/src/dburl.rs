//! F029 database-URL resolution: the one place that knows where the
//! database URL comes from — an explicit flag, the
//! `FISHPOND_DATABASE_URL` environment variable, or a secret mounted as
//! a file (`FISHPOND_DATABASE_URL_FILE`, the compose-secrets pattern).
//!
//! Errors and reports never carry the URL itself — secrets must not
//! reach logs or CLI output.

use std::path::PathBuf;

/// The environment variable holding the database URL.
pub const URL_ENV: &str = "FISHPOND_DATABASE_URL";

/// The environment variable holding the *path* of a mounted secret file
/// that contains the database URL (compose `secrets:` mount).
pub const URL_FILE_ENV: &str = "FISHPOND_DATABASE_URL_FILE";

/// Where a resolved database URL came from. Reports name the source,
/// never the value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DbUrlSource {
    /// The caller passed it explicitly (a flag).
    Flag,
    /// The `FISHPOND_DATABASE_URL` environment variable.
    Env,
    /// The secret file named by `FISHPOND_DATABASE_URL_FILE`.
    SecretFile,
}

impl DbUrlSource {
    /// Secret-free description for reports.
    pub fn describe(self) -> &'static str {
        match self {
            Self::Flag => "explicit flag (value withheld)",
            Self::Env => "environment variable FISHPOND_DATABASE_URL (value withheld)",
            Self::SecretFile => "secret file from FISHPOND_DATABASE_URL_FILE (value withheld)",
        }
    }
}

/// Why no usable database URL could be resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DbUrlError {
    /// None of the sources are configured.
    Missing,
    /// The env var or flag carried only whitespace.
    Empty {
        /// Which source carried the empty value.
        source: &'static str,
    },
    /// `FISHPOND_DATABASE_URL_FILE` is set but the file cannot be read.
    UnreadableFile {
        /// The path (a path is configuration, not a secret).
        path: String,
    },
    /// The secret file exists but is empty or blank.
    EmptyFile {
        /// The path of the blank secret file.
        path: String,
    },
}

impl std::fmt::Display for DbUrlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Missing => f.write_str(
                "no database url: set FISHPOND_DATABASE_URL or FISHPOND_DATABASE_URL_FILE (or pass --database-url)",
            ),
            Self::Empty { source } => write!(f, "database url from {source} is empty"),
            Self::UnreadableFile { path } => write!(f, "secret file {path:?} is unreadable"),
            Self::EmptyFile { path } => write!(f, "secret file {path:?} is empty"),
        }
    }
}

impl std::error::Error for DbUrlError {}

/// Resolves the database URL: flag first, then the env var, then the
/// mounted secret file. The URL is trimmed of surrounding whitespace
/// (secret files usually end in a newline). Never echo the value.
pub fn resolve_database_url(flag: Option<&str>) -> Result<(String, DbUrlSource), DbUrlError> {
    resolve_from(
        flag,
        std::env::var(URL_ENV).ok().as_deref(),
        std::env::var(URL_FILE_ENV).ok().as_deref(),
    )
}

/// The pure decision over the three raw sources (already read from their
/// places) — the testable core, because the process environment cannot be
/// mutated under the workspace-wide `forbid(unsafe_code)`.
pub fn resolve_from(
    flag: Option<&str>,
    env_url: Option<&str>,
    env_file: Option<&str>,
) -> Result<(String, DbUrlSource), DbUrlError> {
    if let Some(flag) = flag {
        let trimmed = flag.trim();
        if trimmed.is_empty() {
            return Err(DbUrlError::Empty {
                source: "the --database-url flag",
            });
        }
        return Ok((trimmed.to_owned(), DbUrlSource::Flag));
    }

    if let Some(raw) = env_url {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err(DbUrlError::Empty { source: URL_ENV });
        }
        return Ok((trimmed.to_owned(), DbUrlSource::Env));
    }

    let Some(path) = env_file else {
        return Err(DbUrlError::Missing);
    };
    let path = path.trim();
    if path.is_empty() {
        return Err(DbUrlError::Missing);
    }
    let content =
        std::fs::read_to_string(PathBuf::from(path)).map_err(|_| DbUrlError::UnreadableFile {
            path: path.to_owned(),
        })?;
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return Err(DbUrlError::EmptyFile {
            path: path.to_owned(),
        });
    }
    Ok((trimmed.to_owned(), DbUrlSource::SecretFile))
}
