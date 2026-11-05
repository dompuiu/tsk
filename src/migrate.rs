//! One-time move of a pre-XDG `~/.tsk` into the XDG data and config directories.
//!
//! The store opens as-is before this runs: `store::default_state_dir` keeps a legacy
//! `~/.tsk` winning while it exists. `tsk migrate` is the only thing that moves it, so a
//! user with a live board decides when the board may move under them.

use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};

use crate::store::{self, CONFIG_FILE};

/// What one migration did, for the output line.
#[derive(Debug)]
pub struct Report {
    pub legacy: PathBuf,
    pub data: PathBuf,
    pub config_file: PathBuf,
    /// The store directory moved.
    pub store_moved: bool,
    /// The store or its settings moved.
    pub moved: bool,
}

/// `tsk migrate`: move `~/.tsk` into the XDG directories.
///
/// An explicit `TSK_STATE_DIR` or `TSK_CONFIG_DIR` already says where things live; there is
/// no default layout left to migrate, so that is refused instead of moving something else.
pub fn run() -> Result<Report, String> {
    if store::env_dir("TSK_STATE_DIR").is_some() || store::env_dir("TSK_CONFIG_DIR").is_some() {
        return Err(
            "TSK_STATE_DIR or TSK_CONFIG_DIR is set; migrate moves only the default layout".into(),
        );
    }
    let missing = || {
        "no home directory is set; set TSK_STATE_DIR and TSK_CONFIG_DIR to where the store and its settings live".to_string()
    };
    let Some(legacy) = store::legacy_state_dir() else {
        return Err(missing());
    };
    let Some(data) = store::default_data_dir() else {
        return Err(missing());
    };
    let Some(config) = store::default_config_dir() else {
        return Err(missing());
    };
    run_at(&legacy, &data, &config)
}

pub fn run_at(legacy: &Path, data: &Path, config: &Path) -> Result<Report, String> {
    let config_file = config.join(CONFIG_FILE);
    let mut report = Report {
        legacy: legacy.to_path_buf(),
        data: data.to_path_buf(),
        config_file: config_file.clone(),
        store_moved: false,
        moved: false,
    };
    // A failed earlier run may have moved the store and left its settings behind; that
    // remainder is still migrated, so a retry finishes the job.
    let pending_config = data.join(CONFIG_FILE).exists() && !config_file.exists();
    if !legacy.is_dir() && !pending_config {
        return Ok(report);
    }
    if config_file.exists() {
        return Err(format!(
            "{} already exists; move it away and run tsk migrate again",
            config_file.display()
        ));
    }
    // Windows keeps the store in the same place before and after the split; only the
    // settings file moves out of it.
    let in_place = legacy == data;
    let legacy_present = legacy.is_dir();
    if legacy_present && !in_place {
        empty_destination(data)?;
    }
    let _locks = if legacy_present {
        Some(Locks::acquire(legacy)?)
    } else {
        None
    };
    if legacy_present && !in_place {
        if let Some(parent) = data.parent() {
            fs::create_dir_all(parent)
                .map_err(|error| format!("could not create {}: {error}", parent.display()))?;
        }
        move_dir(legacy, data).map_err(|error| {
            format!(
                "could not move {} to {}: {error}",
                legacy.display(),
                data.display()
            )
        })?;
        report.store_moved = true;
    }
    let mut moved_config = false;
    let source = data.join(CONFIG_FILE);
    if source.exists() {
        crate::fsperm::ensure_private_dir(config)
            .and_then(|()| move_file(&source, &config_file))
            .map_err(|error| {
                format!(
                    "store is at {}, but config.toml did not move: {error}; move it to {} by hand",
                    data.display(),
                    config_file.display()
                )
            })?;
        crate::fsperm::tighten_file(&config_file);
        moved_config = true;
    }
    report.moved = report.store_moved || moved_config;
    Ok(report)
}

/// A destination with anything in it is never merged into: two stores would diverge. An
/// empty directory left by a failed attempt is removed so the rename can land.
fn empty_destination(data: &Path) -> Result<(), String> {
    match fs::read_dir(data) {
        Ok(mut entries) => {
            if entries.next().is_some() {
                return Err(format!(
                    "{} is not empty; move its contents away and run tsk migrate again",
                    data.display()
                ));
            }
            drop(entries);
            fs::remove_dir(data)
                .map_err(|error| format!("could not clear {}: {error}", data.display()))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("could not read {}: {error}", data.display())),
    }
}

/// A board or a saving verb holds one of these locks; moving the directory under it would
/// leave the running process writing to a moved inode.
#[derive(Debug)]
struct Locks {
    files: Vec<File>,
}

impl Locks {
    fn acquire(dir: &Path) -> Result<Self, String> {
        let mut files = Vec::new();
        for name in [
            "board.lock",
            "tsk.json.lock",
            "backup.lock",
            "backup.pending.lock",
        ] {
            let path = dir.join(name);
            if !path.exists() {
                continue;
            }
            let file = crate::fsperm::open_lock_file(&path)
                .map_err(|error| format!("could not open {}: {error}", path.display()))?;
            match file.try_lock() {
                Ok(()) => files.push(file),
                Err(std::fs::TryLockError::WouldBlock) => {
                    return Err(format!(
                        "{} is in use; quit the board and run tsk migrate again",
                        dir.display()
                    ))
                }
                Err(std::fs::TryLockError::Error(error)) => {
                    return Err(format!("could not lock {}: {error}", path.display()))
                }
            }
        }
        Ok(Self { files })
    }
}

impl Drop for Locks {
    fn drop(&mut self) {
        for file in &self.files {
            let _ = file.unlock();
        }
    }
}

fn move_dir(from: &Path, to: &Path) -> io::Result<()> {
    match fs::rename(from, to) {
        Ok(()) => Ok(()),
        Err(error) if cross_device(&error) => {
            if let Err(copy_error) = copy_tree(from, to) {
                let _ = fs::remove_dir_all(to);
                return Err(copy_error);
            }
            fs::remove_dir_all(from)
        }
        Err(error) => Err(error),
    }
}

fn move_file(from: &Path, to: &Path) -> io::Result<()> {
    match fs::rename(from, to) {
        Ok(()) => Ok(()),
        Err(error) if cross_device(&error) => {
            fs::copy(from, to)?;
            crate::fsperm::tighten_file(to);
            fs::remove_file(from)
        }
        Err(error) => Err(error),
    }
}

fn cross_device(error: &io::Error) -> bool {
    #[cfg(unix)]
    {
        error.raw_os_error() == Some(18)
    }
    #[cfg(windows)]
    {
        error.raw_os_error() == Some(17)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = error;
        false
    }
}

/// Cross-device fallback: copy every file with its mode, refusing symlinks so nothing links
/// out of the store, then let the caller delete the source.
fn copy_tree(from: &Path, to: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(from)?;
    if metadata.file_type().is_symlink() {
        return Err(io::Error::other(format!(
            "{} is a symlink; move it by hand",
            from.display()
        )));
    }
    if metadata.is_dir() {
        fs::create_dir_all(to)?;
        fs::set_permissions(to, metadata.permissions())?;
        for entry in fs::read_dir(from)? {
            let entry = entry?;
            copy_tree(&entry.path(), &to.join(entry.file_name()))?;
        }
        Ok(())
    } else {
        fs::copy(from, to).map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};

    static SEQ: AtomicU64 = AtomicU64::new(0);

    struct Temp(PathBuf);

    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn temp(label: &str) -> Temp {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!("tsk-migrate-{label}-{nanos}-{seq}"));
        fs::create_dir_all(&root).expect("temp");
        Temp(root)
    }

    fn legacy_with_config(root: &Path) -> (PathBuf, PathBuf, PathBuf) {
        let legacy = root.join("home/.tsk");
        let data = root.join("home/.local/share/tsk");
        let config = root.join("home/.config/tsk");
        fs::create_dir_all(&legacy).expect("legacy");
        fs::write(legacy.join("tsk.json"), "{}").expect("store");
        fs::write(legacy.join("trash.jsonl"), "").expect("trash");
        fs::create_dir_all(legacy.join("backup-repos")).expect("backups");
        fs::write(legacy.join(CONFIG_FILE), "[merge]\nmethod = \"squash\"\n").expect("config");
        (legacy, data, config)
    }

    #[test]
    fn moves_the_store_and_the_config_into_the_xdg_dirs() {
        let root = temp("move");
        let (legacy, data, config) = legacy_with_config(&root.0);

        let report = run_at(&legacy, &data, &config).expect("migrate");

        assert!(report.moved);
        assert!(!legacy.exists());
        assert_eq!(
            fs::read_to_string(data.join("tsk.json")).expect("store"),
            "{}"
        );
        assert!(data.join("trash.jsonl").exists());
        assert!(data.join("backup-repos").is_dir());
        assert!(
            !data.join(CONFIG_FILE).exists(),
            "config.toml leaves the store directory"
        );
        assert_eq!(
            fs::read_to_string(config.join(CONFIG_FILE)).expect("config"),
            "[merge]\nmethod = \"squash\"\n"
        );

        // A second run finds nothing to move and changes nothing.
        let again = run_at(&legacy, &data, &config).expect("second run");
        assert!(!again.moved);
        assert_eq!(
            fs::read_to_string(config.join(CONFIG_FILE)).expect("config"),
            "[merge]\nmethod = \"squash\"\n"
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_config_directory_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let root = temp("private");
        let (legacy, data, config) = legacy_with_config(&root.0);
        run_at(&legacy, &data, &config).expect("migrate");
        let mode = fs::metadata(&config)
            .expect("config dir")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700);
        let file = fs::metadata(config.join(CONFIG_FILE))
            .expect("config file")
            .permissions()
            .mode();
        assert_eq!(file & 0o777, 0o600);
    }

    #[test]
    fn refuses_a_destination_that_holds_anything() {
        let root = temp("occupied");
        let (legacy, data, config) = legacy_with_config(&root.0);
        fs::create_dir_all(&data).expect("data");
        fs::write(data.join("other"), "keep").expect("other");

        let error = run_at(&legacy, &data, &config).expect_err("occupied");
        assert!(error.contains("is not empty"), "{error}");
        assert!(legacy.join("tsk.json").exists(), "nothing moved");
        assert!(!config.exists());
    }

    #[test]
    fn an_empty_destination_is_reused() {
        let root = temp("empty");
        let (legacy, data, config) = legacy_with_config(&root.0);
        fs::create_dir_all(&data).expect("empty data");
        run_at(&legacy, &data, &config).expect("migrate into an empty dir");
        assert!(data.join("tsk.json").exists());
    }

    #[test]
    fn refuses_an_existing_config_file_and_leaves_the_store_in_place() {
        let root = temp("config-exists");
        let (legacy, data, config) = legacy_with_config(&root.0);
        fs::create_dir_all(&config).expect("config");
        fs::write(config.join(CONFIG_FILE), "[board]\n").expect("config");

        let error = run_at(&legacy, &data, &config).expect_err("config exists");
        assert!(error.contains("already exists"), "{error}");
        assert!(legacy.join("tsk.json").exists());
        assert!(!data.exists());
        assert_eq!(
            fs::read_to_string(config.join(CONFIG_FILE)).expect("config"),
            "[board]\n"
        );
    }

    #[test]
    fn refuses_while_a_board_holds_the_legacy_store() {
        let root = temp("board-open");
        let (legacy, data, config) = legacy_with_config(&root.0);
        let lock = crate::fsperm::open_lock_file(&legacy.join("board.lock")).expect("lock");
        lock.lock_shared().expect("shared");

        let error = run_at(&legacy, &data, &config).expect_err("board holds it");
        assert!(error.contains("in use"), "{error}");
        assert!(legacy.join("tsk.json").exists());
        assert!(!data.exists());

        lock.unlock().expect("unlock");
        run_at(&legacy, &data, &config).expect("migrate once the board quits");
    }

    #[test]
    fn a_store_already_in_place_only_loses_its_config_file() {
        // The Windows layout: the data directory did not move, the setting did.
        let root = temp("in-place");
        let data = root.0.join("local/tsk");
        let config = root.0.join("roaming/tsk");
        fs::create_dir_all(&data).expect("data");
        fs::write(data.join("tsk.json"), "{}").expect("store");
        fs::write(data.join(CONFIG_FILE), "[board]\n").expect("config");

        let report = run_at(&data, &data, &config).expect("migrate");
        assert!(report.moved);
        assert!(data.join("tsk.json").exists());
        assert!(!data.join(CONFIG_FILE).exists());
        assert_eq!(
            fs::read_to_string(config.join(CONFIG_FILE)).expect("config"),
            "[board]\n"
        );
    }

    #[test]
    fn a_retry_finishes_when_only_the_config_was_left_behind() {
        // An earlier run moved the store and failed on the settings file. The legacy
        // directory is gone, yet migrate must still place config.toml.
        let root = temp("partial");
        let legacy = root.0.join("home/.tsk");
        let data = root.0.join("home/.local/share/tsk");
        let config = root.0.join("home/.config/tsk");
        fs::create_dir_all(&data).expect("data");
        fs::write(data.join("tsk.json"), "{}").expect("store");
        fs::write(data.join(CONFIG_FILE), "[board]\n").expect("config");

        let report = run_at(&legacy, &data, &config).expect("retry");
        assert!(report.moved);
        assert!(!report.store_moved);
        assert_eq!(
            fs::read_to_string(config.join(CONFIG_FILE)).expect("config"),
            "[board]\n"
        );
        assert!(!data.join(CONFIG_FILE).exists());
    }

    #[test]
    fn a_missing_legacy_directory_is_a_no_op() {
        let root = temp("absent");
        let report = run_at(
            &root.0.join("home/.tsk"),
            &root.0.join("home/.local/share/tsk"),
            &root.0.join("home/.config/tsk"),
        )
        .expect("nothing to migrate");
        assert!(!report.moved);
        assert!(!root.0.join("home/.local/share/tsk").exists());
    }

    #[test]
    fn a_file_that_blocks_the_store_move_is_reported() {
        let root = temp("blocked");
        let (legacy, data, config) = legacy_with_config(&root.0);
        fs::create_dir_all(data.parent().expect("parent")).expect("parent");
        fs::write(&data, "not a directory").expect("blocking file");

        let error = run_at(&legacy, &data, &config).expect_err("blocked");
        assert!(error.contains("could not read"), "{error}");
        assert!(legacy.join("tsk.json").exists());
    }

    #[test]
    fn copy_tree_keeps_modes_and_refuses_symlinks() {
        let root = temp("copy");
        let from = root.0.join("from");
        let to = root.0.join("to");
        fs::create_dir_all(&from).expect("from");
        fs::write(from.join("tsk.json"), "{}").expect("store");
        copy_tree(&from, &to).expect("copy");
        assert_eq!(fs::read_to_string(to.join("tsk.json")).expect("copy"), "{}");

        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(from.join("tsk.json"), from.join("link")).expect("symlink");
            let error = copy_tree(&from, &root.0.join("again")).expect_err("symlink");
            assert!(error.to_string().contains("symlink"), "{error}");
        }
    }

    #[test]
    fn a_config_file_move_failure_names_both_paths() {
        let root = temp("config-move");
        let (legacy, data, config) = legacy_with_config(&root.0);
        // Make the config destination's parent a file, so creating the config dir fails.
        fs::create_dir_all(root.0.join("home/.config")).expect("home");
        fs::write(root.0.join("home/.config/tsk"), "not a directory").expect("blocking file");

        let error = run_at(&legacy, &data, &config).expect_err("config cannot move");
        assert!(error.contains("store is at"), "{error}");
        assert!(data.join("tsk.json").exists());
        // The config is still readable beside the store, so the next run finds it.
        assert!(data.join(CONFIG_FILE).exists());
    }

    #[test]
    fn the_lock_guard_releases_on_drop() {
        let root = temp("locks");
        let dir = root.0.join("state");
        fs::create_dir_all(&dir).expect("dir");
        let board = crate::fsperm::open_lock_file(&dir.join("board.lock")).expect("lock");
        board.lock_shared().expect("shared");
        {
            let _held = Locks::acquire(&dir).expect_err("board holds it");
            let _ = _held;
        }
        board.unlock().expect("unlock");
        {
            let _held = Locks::acquire(&dir).expect("free now");
        }
        drop(board);
    }

    #[test]
    fn written_files_survive_the_move_byte_for_byte() {
        let root = temp("bytes");
        let (legacy, data, config) = legacy_with_config(&root.0);
        let mut extra = File::create(legacy.join("machine-id")).expect("machine");
        extra.write_all(b"host-01234567\n").expect("write");
        drop(extra);
        run_at(&legacy, &data, &config).expect("migrate");
        assert_eq!(
            fs::read_to_string(data.join("machine-id")).expect("machine"),
            "host-01234567\n"
        );
    }
}
