//! F029 acceptance (url resolution side): flag > env > secret-file
//! precedence, trimming, and the secret-free error contract. Pure — no
//! database, no process-env mutation (forbidden under
//! `forbid(unsafe_code)`): the testable core [`resolve_from`] takes the
//! three raw sources as parameters.

use fishpond_storage::dburl::resolve_from;
use fishpond_storage::{DbUrlError, DbUrlSource};

#[test]
fn flag_wins_over_everything() {
    let (url, source) = resolve_from(
        Some("postgres://from-flag@db/flag"),
        Some("postgres://from-env@db/env"),
        Some("/run/secrets/db_url"),
    )
    .expect("resolves");
    assert_eq!(source, DbUrlSource::Flag);
    assert_eq!(url, "postgres://from-flag@db/flag");
}

#[test]
fn env_wins_over_the_secret_file() {
    let (url, source) = resolve_from(
        None,
        Some("postgres://from-env@db/env"),
        Some("/run/secrets/db_url"),
    )
    .expect("resolves");
    assert_eq!(source, DbUrlSource::Env);
    assert_eq!(url, "postgres://from-env@db/env");
}

#[test]
fn flag_and_values_are_trimmed() {
    let (url, source) = resolve_from(Some("  postgres://db/x \n"), None, None).expect("resolves");
    assert_eq!(source, DbUrlSource::Flag);
    assert_eq!(url, "postgres://db/x");
    assert_eq!(
        resolve_from(Some("   "), None, None),
        Err(DbUrlError::Empty {
            source: "the --database-url flag"
        })
    );
    assert_eq!(
        resolve_from(None, Some("  "), None),
        Err(DbUrlError::Empty {
            source: "FISHPOND_DATABASE_URL"
        })
    );
}

#[test]
fn missing_everything_reports_missing() {
    assert_eq!(resolve_from(None, None, None), Err(DbUrlError::Missing));
    // A blank secret-file pointer is as good as absent.
    assert_eq!(
        resolve_from(None, None, Some("  ")),
        Err(DbUrlError::Missing)
    );
}

#[test]
fn secret_file_success_and_errors() {
    let dir = std::env::temp_dir().join(format!("fishpond-dburl-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let path = dir.join("fishpond_database_url");
    let path_str = path.to_string_lossy().into_owned();

    // A mounted file whose trailing newline is trimmed away.
    std::fs::write(&path, "postgres://secret-db/x\n").expect("secret file");
    let (url, source) =
        resolve_from(None, None, Some(&path_str)).expect("resolves from the secret file");
    assert_eq!(source, DbUrlSource::SecretFile);
    assert_eq!(url, "postgres://secret-db/x");

    // Blank secret file.
    std::fs::write(&path, "   \n").expect("blank secret file");
    assert_eq!(
        resolve_from(None, None, Some(&path_str)),
        Err(DbUrlError::EmptyFile {
            path: path_str.clone()
        })
    );

    // Unreadable secret file.
    std::fs::remove_file(&path).expect("removes");
    assert_eq!(
        resolve_from(None, None, Some(&path_str)),
        Err(DbUrlError::UnreadableFile {
            path: path_str.clone()
        })
    );

    std::fs::remove_dir_all(&dir).expect("cleanup");
}

#[test]
fn the_error_contract_never_carries_a_url_value() {
    let rendered = DbUrlError::Empty {
        source: "FISHPOND_DATABASE_URL",
    }
    .to_string();
    assert!(!rendered.contains("postgres://"), "{rendered}");
    let rendered = DbUrlSource::SecretFile.describe();
    assert!(!rendered.contains("postgres://"), "{rendered}");
    assert!(rendered.contains("withheld"), "{rendered}");
}
