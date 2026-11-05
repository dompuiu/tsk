//! `tsk migrate`: the one-time move from a pre-XDG `~/.tsk` to the XDG data and config
//! directories, and the store staying usable in `~/.tsk` until it runs.

use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

static NEXT: AtomicU64 = AtomicU64::new(0);

fn temp(label: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let seq = NEXT.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!("tsk-cli-migrate-{label}-{nanos}-{seq}"));
    fs::create_dir_all(&path).expect("temp");
    path
}

struct Layout {
    home: PathBuf,
    data: PathBuf,
    config: PathBuf,
}

impl Layout {
    fn new(root: &Path) -> Self {
        let home = root.join("home");
        fs::create_dir_all(&home).expect("home");
        Self {
            home,
            data: root.join("data"),
            config: root.join("config"),
        }
    }

    fn legacy(&self) -> PathBuf {
        self.home.join(".tsk")
    }

    fn data_store(&self) -> PathBuf {
        self.data.join("tsk")
    }

    fn config_file(&self) -> PathBuf {
        self.config.join("tsk/config.toml")
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_tsk"));
        command
            .env("HOME", &self.home)
            .env("XDG_DATA_HOME", &self.data)
            .env("XDG_CONFIG_HOME", &self.config)
            .env_remove("TSK_STATE_DIR")
            .env_remove("TSK_CONFIG_DIR")
            .env("TSK_NO_UPDATE_CHECK", "1");
        command
    }

    fn migrate(&self) -> Output {
        self.command()
            .arg("migrate")
            .output()
            .expect("spawn migrate")
    }
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn migrate_help_is_reference_text() {
    let root = temp("help");
    let layout = Layout::new(&root);
    let output = layout
        .command()
        .args(["migrate", "--help"])
        .output()
        .expect("spawn help");
    assert_eq!(output.status.code(), Some(0));
    assert!(stderr(&output).is_empty());
    let text = stdout(&output);
    assert!(text.contains("usage: tsk migrate"), "{text}");
    assert!(text.contains("XDG_DATA_HOME"), "{text}");
    assert!(text.contains("~/.tsk"), "{text}");
    let _ = fs::remove_dir_all(root);
}

#[test]
fn migrate_rejects_extra_arguments_as_usage() {
    let root = temp("usage");
    let layout = Layout::new(&root);
    let output = layout
        .command()
        .args(["migrate", "--now"])
        .output()
        .expect("spawn migrate");
    assert_eq!(output.status.code(), Some(2));
    assert!(
        stderr(&output).contains("usage: tsk migrate"),
        "{}",
        stderr(&output)
    );
    let _ = fs::remove_dir_all(root);
}

#[cfg(unix)]
#[test]
fn a_store_in_dot_tsk_keeps_working_until_migrated_then_moves() {
    let root = temp("move");
    let layout = Layout::new(&root);

    // A pre-XDG install: the store is `~/.tsk`.
    let seeded = layout
        .command()
        .env("TSK_STATE_DIR", layout.legacy())
        .args(["add", "-t", "before upgrade"])
        .output()
        .expect("spawn add");
    assert_eq!(seeded.status.code(), Some(0), "{}", stderr(&seeded));

    // Without an override, the legacy store still wins over the empty XDG data dir.
    let after = layout
        .command()
        .args(["add", "-t", "after upgrade"])
        .output()
        .expect("spawn add");
    assert_eq!(after.status.code(), Some(0), "{}", stderr(&after));
    assert!(fs::read_to_string(layout.legacy().join("tsk.json"))
        .expect("legacy store")
        .contains("after upgrade"));
    assert!(
        !layout.data_store().exists(),
        "the store did not move on its own"
    );

    fs::write(
        layout.legacy().join("config.toml"),
        "[merge]\nmethod = \"squash\"\n",
    )
    .expect("config");

    let moved = layout.migrate();
    assert_eq!(moved.status.code(), Some(0), "{}", stderr(&moved));
    let text = stdout(&moved);
    assert!(text.contains("Moved"), "{text}");
    assert!(text.contains("Store:"), "{text}");
    assert!(!layout.legacy().exists());
    let store = fs::read_to_string(layout.data_store().join("tsk.json")).expect("store");
    assert!(store.contains("after upgrade"));
    assert!(
        !layout.data_store().join("config.toml").exists(),
        "config.toml leaves the data directory"
    );
    assert!(fs::read_to_string(layout.config_file())
        .expect("config")
        .contains("squash"));

    // The next verb reads the migrated store.
    let listed = layout
        .command()
        .args(["list", "--json"])
        .output()
        .expect("spawn list");
    assert_eq!(listed.status.code(), Some(0), "{}", stderr(&listed));
    assert!(stdout(&listed).contains("after upgrade"));

    let again = layout.migrate();
    assert_eq!(again.status.code(), Some(0), "{}", stderr(&again));
    assert!(
        stdout(&again).contains("Nothing to migrate"),
        "{}",
        stdout(&again)
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn migrate_refuses_an_explicit_state_directory() {
    let root = temp("explicit");
    let layout = Layout::new(&root);
    let output = layout
        .command()
        .env("TSK_STATE_DIR", layout.home.join("chosen"))
        .arg("migrate")
        .output()
        .expect("spawn migrate");
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains("TSK_STATE_DIR"),
        "{}",
        stderr(&output)
    );
    let _ = fs::remove_dir_all(root);
}
