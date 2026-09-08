//! Host operations `sync`'s algorithm needs (files, locking-adjacent
//! checks, and `wg-quick` invocation), behind a trait so the state
//! machine can be tested with an in-memory fake instead of touching a
//! real filesystem/interface (wg-client.md §6, tested this way per
//! docs/milestones.md §3 "an explicit host-operation test adapter").

use std::path::{Path, PathBuf};

pub trait SystemOps {
    fn read_file(&self, path: &Path) -> Option<Vec<u8>>;

    /// Write `contents` to `path` atomically: stage in the same
    /// directory, fsync, rename over the target, fsync the parent
    /// directory. Mode `0600`.
    fn write_file_atomic(&self, path: &Path, contents: &[u8]) -> Result<(), String>;

    fn copy_file(&self, from: &Path, to: &Path) -> Result<(), String>;
    fn file_exists(&self, path: &Path) -> bool;

    /// Whether a WireGuard interface named `iface` currently exists.
    fn interface_exists(&self, iface: &str) -> bool;

    async fn wg_quick_up(&self, conf_path: &Path) -> Result<(), String>;
    async fn wg_quick_down(&self, conf_path: &Path) -> Result<(), String>;
}

pub struct RealSystemOps;

impl SystemOps for RealSystemOps {
    fn read_file(&self, path: &Path) -> Option<Vec<u8>> {
        std::fs::read(path).ok()
    }

    fn write_file_atomic(&self, path: &Path, contents: &[u8]) -> Result<(), String> {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;

        let tmp = tmp_path_for(path);
        {
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&tmp)
                .map_err(|e| format!("open {tmp:?}: {e}"))?;
            f.write_all(contents)
                .map_err(|e| format!("write {tmp:?}: {e}"))?;
            f.sync_all().map_err(|e| format!("fsync {tmp:?}: {e}"))?;
        }
        std::fs::rename(&tmp, path).map_err(|e| format!("rename {tmp:?} -> {path:?}: {e}"))?;
        if let Some(parent) = path.parent()
            && let Ok(dir) = std::fs::File::open(parent)
        {
            let _ = dir.sync_all();
        }
        Ok(())
    }

    fn copy_file(&self, from: &Path, to: &Path) -> Result<(), String> {
        std::fs::copy(from, to)
            .map(|_| ())
            .map_err(|e| format!("copy {from:?} -> {to:?}: {e}"))
    }

    fn file_exists(&self, path: &Path) -> bool {
        path.exists()
    }

    fn interface_exists(&self, iface: &str) -> bool {
        Path::new("/sys/class/net").join(iface).exists()
    }

    async fn wg_quick_up(&self, conf_path: &Path) -> Result<(), String> {
        run_wg_quick("up", conf_path).await
    }

    async fn wg_quick_down(&self, conf_path: &Path) -> Result<(), String> {
        run_wg_quick("down", conf_path).await
    }
}

fn tmp_path_for(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(".wg-client-tmp");
    PathBuf::from(s)
}

async fn run_wg_quick(action: &str, conf_path: &Path) -> Result<(), String> {
    let output = tokio::process::Command::new("wg-quick")
        .arg(action)
        .arg(conf_path)
        .output()
        .await
        .map_err(|e| format!("failed to spawn wg-quick {action}: {e}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "wg-quick {action} exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        ))
    }
}

#[cfg(test)]
pub mod mock {
    use super::SystemOps;
    use std::collections::{HashMap, HashSet};
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    #[derive(Default)]
    pub struct MockSystemOps {
        files: Mutex<HashMap<PathBuf, Vec<u8>>>,
        up_interfaces: Mutex<HashSet<String>>,
        /// Fail the next N `wg_quick_up`/`wg_quick_down` calls, then
        /// succeed — so a test can fail exactly the candidate's apply
        /// attempt and still let a subsequent rollback attempt succeed.
        pub fail_next_wg_quick_up: Mutex<u32>,
        pub fail_next_wg_quick_down: Mutex<u32>,
        pub calls: Mutex<Vec<String>>,
        /// Interface name to report as up/down for `interface_exists`,
        /// keyed by the conf_path's stem (tests use one interface).
        pub iface_name: Mutex<String>,
    }

    impl MockSystemOps {
        pub fn new() -> Self {
            Self::default()
        }

        pub fn set_file(&self, path: &Path, contents: Vec<u8>) {
            self.files
                .lock()
                .unwrap()
                .insert(path.to_path_buf(), contents);
        }

        pub fn get_file(&self, path: &Path) -> Option<Vec<u8>> {
            self.files.lock().unwrap().get(path).cloned()
        }

        pub fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl SystemOps for MockSystemOps {
        fn read_file(&self, path: &Path) -> Option<Vec<u8>> {
            self.files.lock().unwrap().get(path).cloned()
        }

        fn write_file_atomic(&self, path: &Path, contents: &[u8]) -> Result<(), String> {
            self.files
                .lock()
                .unwrap()
                .insert(path.to_path_buf(), contents.to_vec());
            Ok(())
        }

        fn copy_file(&self, from: &Path, to: &Path) -> Result<(), String> {
            let contents = self
                .files
                .lock()
                .unwrap()
                .get(from)
                .cloned()
                .ok_or_else(|| format!("{from:?} does not exist"))?;
            self.files
                .lock()
                .unwrap()
                .insert(to.to_path_buf(), contents);
            Ok(())
        }

        fn file_exists(&self, path: &Path) -> bool {
            self.files.lock().unwrap().contains_key(path)
        }

        fn interface_exists(&self, iface: &str) -> bool {
            self.up_interfaces.lock().unwrap().contains(iface)
        }

        async fn wg_quick_up(&self, conf_path: &Path) -> Result<(), String> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("up:{}", conf_path.display()));
            {
                let mut n = self.fail_next_wg_quick_up.lock().unwrap();
                if *n > 0 {
                    *n -= 1;
                    return Err("injected wg-quick up failure".to_string());
                }
            }
            let iface = self.iface_name.lock().unwrap().clone();
            self.up_interfaces.lock().unwrap().insert(iface);
            Ok(())
        }

        async fn wg_quick_down(&self, conf_path: &Path) -> Result<(), String> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("down:{}", conf_path.display()));
            {
                let mut n = self.fail_next_wg_quick_down.lock().unwrap();
                if *n > 0 {
                    *n -= 1;
                    return Err("injected wg-quick down failure".to_string());
                }
            }
            let iface = self.iface_name.lock().unwrap().clone();
            self.up_interfaces.lock().unwrap().remove(&iface);
            Ok(())
        }
    }
}
