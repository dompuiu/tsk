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
    let legacy_config = store::legacy_config_dir();
    run_at_with_config(&legacy, legacy_config.as_deref(), &data, &config)
}

/// [`run`] for a caller that does not need the old Windows settings directory, such as a
/// test of the Unix layout.
pub fn run_at(legacy: &Path, data: &Path, config: &Path) -> Result<Report, String> {
    run_at_with_config(legacy, None, data, config)
}

pub fn run_at_with_config(
    legacy: &Path,
    legacy_config: Option<&Path>,
    data: &Path,
    config: &Path,
) -> Result<Report, String> {
    let config_file = config.join(CONFIG_FILE);
    let mut report = Report {
        legacy: legacy.to_path_buf(),
        data: data.to_path_buf(),
        config_file: config_file.clone(),
        store_moved: false,
        moved: false,
    };
    let legacy_present = legacy.is_dir();
    // Windows keeps the store in the same place before and after the split only when the
    // XDG variables are unset; otherwise the move below is a real one.
    let in_place = store::same_dir(legacy, data);
    let moves_store = legacy_present && !in_place;
    let in_store = data.join(CONFIG_FILE);
    // Settings sit beside the store before the move, or in the settings directory an older
    // Windows install used. When that directory is also the destination there is nothing
    // to move.
    let config_target_is_legacy = legacy_config.is_some_and(|dir| store::same_dir(dir, config));
    let legacy_file = legacy_config
        .map(|dir| dir.join(CONFIG_FILE))
        .filter(|path| !config_target_is_legacy && path.exists());
    let store_file = if legacy_present {
        legacy.join(CONFIG_FILE)
    } else {
        in_store.clone()
    };
    let has_config_source = store_file.exists() || legacy_file.is_some();
    // A failed earlier run may have moved the store and left its settings behind; that
    // remainder is still migrated, so a retry finishes the job.
    let pending_config = has_config_source && !config_file.exists();
    if !legacy_present && !pending_config {
        return Ok(report);
    }
    if has_config_source && config_file.exists() {
        return Err(format!(
            "{} already exists; move it away and run tsk migrate again",
            config_file.display()
        ));
    }
    // A Windows install could leave settings in both places. Never pick one silently; an
    // identical copy is dropped once the kept one has moved.
    let mut duplicate_legacy: Option<PathBuf> = None;
    if let Some(other) = legacy_file.as_ref() {
        if store_file.exists() {
            let read = |path: &Path| {
                fs::read(path)
                    .map_err(|error| format!("could not read {}: {error}", path.display()))
            };
            if read(&store_file)? != read(other)? {
                return Err(format!(
                    "{} and {} both hold settings; keep one and run tsk migrate again",
                    store_file.display(),
                    other.display()
                ));
            }
            duplicate_legacy = Some(other.clone());
        }
    }
    if moves_store && is_inside(data, legacy) {
        return Err(format!(
            "{} is inside {}; set XDG_DATA_HOME outside the old store",
            data.display(),
            legacy.display()
        ));
    }
    if moves_store {
        empty_destination(data)?;
    }
    let locks = if legacy_present {
        Some(Locks::acquire(legacy)?)
    } else {
        None
    };
    if moves_store {
        if let Some(parent) = data.parent() {
            fs::create_dir_all(parent)
                .map_err(|error| format!("could not create {}: {error}", parent.display()))?;
        }
    }
    // Windows refuses to rename a directory that holds an open handle, and the lock files
    // are exactly that. Holding them proves no board or save is running; the rename below
    // has to happen after they close on Windows. Unix keeps the guard through both moves.
    #[cfg(windows)]
    drop(locks);
    #[cfg(not(windows))]
    let _locks = locks;
    if moves_store {
        move_dir(legacy, data).map_err(|error| {
            let hint = if error.kind() == io::ErrorKind::PermissionDenied {
                "; close any program using the store and run tsk migrate again"
            } else {
                ""
            };
            format!(
                "could not move {} to {}: {error}{hint}",
                legacy.display(),
                data.display()
            )
        })?;
        report.store_moved = true;
    }
    let mut moved_config = false;
    let source = if in_store.exists() {
        Some(in_store)
    } else {
        legacy_file
    };
    if let Some(source) = source {
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
        if let Some(other) = duplicate_legacy {
            let _ = fs::remove_file(other);
        }
    }
    report.moved = report.store_moved || moved_config;
    Ok(report)
}

/// Whether `path` lies under `ancestor`, by spelling or once canonicalized.
fn is_inside(path: &Path, ancestor: &Path) -> bool {
    if path.starts_with(ancestor) {
        return true;
    }
    match (fs::canonicalize(path), fs::canonicalize(ancestor)) {
        (Ok(path), Ok(ancestor)) => path.starts_with(ancestor),
        _ => false,
    }
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

/// Cross-device fallback: copy every file with its mode, refusing links and junctions so
/// nothing points out of the store, then let the caller delete the source.
fn copy_tree(from: &Path, to: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(from)?;
    if crate::fsperm::is_reparse_or_symlink(&metadata) {
        return Err(io::Error::other(format!(
            "{} is a symlink or junction; move it by hand",
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

    #[test]
    fn moves_a_store_that_still_holds_the_lock_files() {
        // Windows refuses to rename a directory whose files are open, so the locks that
        // prove no board is running must close before the move. Unix renames either way,
        // so this test guards the Windows ordering.
        let root = temp("locks-move");
        let (legacy, data, config) = legacy_with_config(&root.0);
        for name in [
            "board.lock",
            "tsk.json.lock",
            "backup.lock",
            "backup.pending.lock",
        ] {
            fs::write(legacy.join(name), "").expect("lock");
        }

        let report = run_at(&legacy, &data, &config).expect("migrate with lock files");

        assert!(report.store_moved);
        assert!(!legacy.exists());
        assert!(data.join("board.lock").exists());
        assert_eq!(
            fs::read_to_string(config.join(CONFIG_FILE)).expect("config"),
            "[merge]\nmethod = \"squash\"\n"
        );
    }

    #[test]
    fn moves_settings_left_in_the_old_windows_settings_directory() {
        let root = temp("old-settings");
        let legacy = root.0.join("local/tsk");
        let old_config = root.0.join("roaming/tsk");
        let data = root.0.join("home/.local/share/tsk");
        let config = root.0.join("home/.config/tsk");
        fs::create_dir_all(&legacy).expect("store");
        fs::write(legacy.join("tsk.json"), "{}").expect("store file");
        fs::create_dir_all(&old_config).expect("old settings");
        fs::write(old_config.join(CONFIG_FILE), "[board]\n").expect("settings");

        let report =
            run_at_with_config(&legacy, Some(&old_config), &data, &config).expect("migrate");

        assert!(report.store_moved);
        assert!(!legacy.exists());
        assert!(!old_config.join(CONFIG_FILE).exists());
        assert_eq!(
            fs::read_to_string(config.join(CONFIG_FILE)).expect("config"),
            "[board]\n"
        );
    }

    #[test]
    fn refuses_two_settings_files_that_differ() {
        let root = temp("settings-both");
        let (legacy, data, config) = legacy_with_config(&root.0);
        let old_config = root.0.join("roaming/tsk");
        fs::create_dir_all(&old_config).expect("old settings");
        fs::write(old_config.join(CONFIG_FILE), "[board]\n").expect("other settings");

        let error = run_at_with_config(&legacy, Some(&old_config), &data, &config)
            .expect_err("two settings files");
        assert!(error.contains("both hold settings"), "{error}");
        assert!(legacy.join("tsk.json").exists(), "nothing moved");
        assert!(!data.exists());
        assert!(!config.exists());
    }

    #[test]
    fn an_identical_settings_copy_is_removed_after_the_move() {
        let root = temp("settings-same");
        let (legacy, data, config) = legacy_with_config(&root.0);
        let old_config = root.0.join("roaming/tsk");
        fs::create_dir_all(&old_config).expect("old settings");
        fs::copy(legacy.join(CONFIG_FILE), old_config.join(CONFIG_FILE)).expect("copy settings");

        run_at_with_config(&legacy, Some(&old_config), &data, &config).expect("migrate");

        assert_eq!(
            fs::read_to_string(config.join(CONFIG_FILE)).expect("config"),
            "[merge]\nmethod = \"squash\"\n"
        );
        assert!(!data.join(CONFIG_FILE).exists());
        assert!(
            !old_config.join(CONFIG_FILE).exists(),
            "the stale copy is dropped once the kept one moved"
        );
    }

    #[test]
    fn moving_only_the_store_is_enough_when_the_settings_already_sit_there() {
        // Windows with `XDG_DATA_HOME` set and `XDG_CONFIG_HOME` unset: the settings
        // directory is the same before and after, so only the store moves.
        let root = temp("data-only");
        let legacy = root.0.join("local/tsk");
        let data = root.0.join("home/.local/share/tsk");
        let config = root.0.join("roaming/tsk");
        fs::create_dir_all(&legacy).expect("store");
        fs::write(legacy.join("tsk.json"), "{}").expect("store file");
        fs::create_dir_all(&config).expect("settings");
        fs::write(config.join(CONFIG_FILE), "[board]\n").expect("settings");

        let report = run_at_with_config(&legacy, Some(&config), &data, &config).expect("migrate");

        assert!(report.store_moved);
        assert!(!legacy.exists());
        assert!(data.join("tsk.json").exists());
        assert_eq!(
            fs::read_to_string(config.join(CONFIG_FILE)).expect("settings"),
            "[board]\n"
        );
    }

    #[test]
    fn refuses_a_data_directory_inside_the_old_store() {
        let root = temp("inside");
        let (legacy, _data, config) = legacy_with_config(&root.0);
        let inside = legacy.join("share/tsk");

        let error = run_at(&legacy, &inside, &config).expect_err("inside the store");
        assert!(error.contains("is inside"), "{error}");
        assert!(legacy.join("tsk.json").exists());
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
        drop(lock);
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
