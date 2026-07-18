#![allow(dead_code)]

use std::{
    io,
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow, bail};

use crate::{
    lsp_process::{ProcessIo, SpawnedLanguageServer},
    navigation::TrustedServer,
};

pub(crate) struct OwnedProcessTree {
    child: Option<Child>,
    pgid: libc::pid_t,
    terminated: bool,
}

impl OwnedProcessTree {
    pub(crate) fn id(&self) -> u32 {
        self.child.as_ref().map_or(0, Child::id)
    }

    pub(crate) fn try_wait(&mut self) -> Result<Option<std::process::ExitStatus>> {
        self.child
            .as_mut()
            .ok_or_else(|| anyhow!("language server child already reaped"))?
            .try_wait()
            .context("cannot query language server process")
    }

    pub(crate) fn wait_for_exit(&mut self, deadline: Instant) -> Result<bool> {
        while Instant::now() < deadline {
            if self.poll_exit()? {
                return Ok(true);
            }
            thread::sleep(Duration::from_millis(10));
        }
        self.poll_exit()
    }

    pub(crate) fn force_cleanup(&mut self) -> Result<()> {
        if self.terminated {
            return Ok(());
        }
        self.begin_force_cleanup()?;
        let term_deadline = Instant::now() + Duration::from_millis(250);
        while Instant::now() < term_deadline {
            if self.poll_force_cleanup()? {
                return Ok(());
            }
            thread::sleep(Duration::from_millis(10));
        }
        self.escalate_force_cleanup()?;
        let kill_deadline = Instant::now() + Duration::from_secs(1);
        while Instant::now() < kill_deadline {
            if self.poll_force_cleanup()? {
                return Ok(());
            }
            thread::sleep(Duration::from_millis(10));
        }
        if self.poll_force_cleanup()? {
            return Ok(());
        }
        bail!("language server process group did not terminate and reap within deadline")
    }

    /// Observe and reap without waiting. A true result means both the direct
    /// child and every ordinary member of its process group are gone.
    pub(crate) fn poll_exit(&mut self) -> Result<bool> {
        if self.terminated {
            return Ok(true);
        }
        self.reap_if_exited()?;
        if self.child.is_none() && !group_exists(self.pgid)? {
            self.terminated = true;
            return Ok(true);
        }
        Ok(false)
    }

    /// Start the cooperative forced-cleanup phase without waiting.
    pub(crate) fn begin_force_cleanup(&mut self) -> Result<()> {
        if self.terminated {
            return Ok(());
        }
        // On macOS, signaling a group whose only member is an unreaped zombie
        // returns EPERM. Reap an already-exited direct child first; descendants
        // (if any) keep the group observable and are still terminated below.
        self.reap_if_exited()?;
        if self.child.is_none() && !group_exists(self.pgid)? {
            self.terminated = true;
            return Ok(());
        }
        signal_group(self.pgid, libc::SIGTERM)
    }

    /// Escalate the already-started cleanup to SIGKILL without waiting.
    pub(crate) fn escalate_force_cleanup(&mut self) -> Result<()> {
        if self.terminated {
            return Ok(());
        }
        self.reap_if_exited()?;
        if group_exists(self.pgid)? {
            signal_group(self.pgid, libc::SIGKILL)?;
        }
        if let Some(child) = self.child.as_mut()
            && child
                .try_wait()
                .context("cannot query direct language server child")?
                .is_none()
        {
            child
                .kill()
                .context("cannot kill direct language server child")?;
        }
        Ok(())
    }

    pub(crate) fn poll_force_cleanup(&mut self) -> Result<bool> {
        self.poll_exit()
    }

    fn reap_if_exited(&mut self) -> Result<()> {
        let exited = match self.child.as_mut() {
            Some(child) => child
                .try_wait()
                .context("cannot query direct language server child")?
                .is_some(),
            None => false,
        };
        if exited {
            let mut child = self.child.take().expect("exited child is present");
            child
                .wait()
                .context("cannot reap exited direct language server child")?;
        }
        Ok(())
    }
}

impl Drop for OwnedProcessTree {
    fn drop(&mut self) {
        if !self.terminated {
            let _ = self.force_cleanup();
        }
    }
}

pub(crate) fn spawn(
    server: &TrustedServer,
    workspace_root: &std::path::Path,
    server_root: &std::path::Path,
) -> Result<SpawnedLanguageServer> {
    use std::os::unix::process::CommandExt;

    server.revalidate_before_spawn(workspace_root)?;
    let mut child = Command::new(server.program())
        .args(server.args())
        .current_dir(server_root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()
        .with_context(|| {
            format!(
                "cannot start language server {}",
                server.program().display()
            )
        })?;
    let pid = libc::pid_t::try_from(child.id()).context("child pid cannot be represented")?;
    // SAFETY: getpgid only observes the live child pid.
    let pgid = unsafe { libc::getpgid(pid) };
    if pgid != pid {
        let mut tree = OwnedProcessTree {
            child: Some(child),
            pgid: if pgid > 0 { pgid } else { pid },
            terminated: false,
        };
        let _ = tree.force_cleanup();
        bail!("language server did not enter its own process group");
    }
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| anyhow!("missing child stdin"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("missing child stdout"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| anyhow!("missing child stderr"))?;
    Ok(SpawnedLanguageServer {
        tree: OwnedProcessTree {
            child: Some(child),
            pgid,
            terminated: false,
        },
        io: ProcessIo {
            stdin: Box::new(stdin),
            stdout: Box::new(stdout),
            stderr: Box::new(stderr),
        },
    })
}

fn signal_group(pgid: libc::pid_t, signal: libc::c_int) -> Result<()> {
    // SAFETY: a negative pgid targets the contained process group.
    let result = unsafe { libc::kill(-pgid, signal) };
    if result == 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        return Ok(());
    }
    Err(error).context("cannot signal language server process group")
}

fn group_exists(pgid: libc::pid_t) -> Result<bool> {
    // SAFETY: signal zero probes process-group existence without mutation.
    let result = unsafe { libc::kill(-pgid, 0) };
    if result == 0 {
        return Ok(true);
    }
    let error = io::Error::last_os_error();
    match error.raw_os_error() {
        Some(libc::ESRCH) => Ok(false),
        Some(libc::EPERM) => Ok(true),
        _ => Err(error).context("cannot inspect language server process group"),
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, os::unix::fs::PermissionsExt, sync::Arc};

    use super::*;
    use crate::navigation::{TrustedServer, validate_executable};

    #[test]
    fn production_spawner_uses_own_group_and_reaps_child() {
        let container = tempfile::tempdir().unwrap();
        let workspace = container.path().join("workspace");
        let tools = container.path().join("tools");
        fs::create_dir_all(&workspace).unwrap();
        fs::create_dir_all(&tools).unwrap();
        let script = tools.join("server");
        fs::write(&script, "#!/bin/sh\nsleep 30\n").unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        let workspace = workspace.canonicalize().unwrap();
        let script = script.canonicalize().unwrap();
        let validated = validate_executable(&script, &workspace).unwrap();
        let server = TrustedServer {
            program: validated.path,
            args: Arc::from([]),
            identity: validated.identity,
        };
        let mut spawned = spawn(&server, &workspace, &workspace).unwrap();
        let pid = spawned.tree.id();
        assert!(pid > 0);
        spawned.tree.force_cleanup().unwrap();
        assert_eq!(spawned.tree.id(), 0);
        assert!(spawned.tree.try_wait().is_err());
        spawned.tree.force_cleanup().unwrap();

        let dropped = spawn(&server, &workspace, &workspace).unwrap();
        let dropped_group = libc::pid_t::try_from(dropped.tree.id()).unwrap();
        drop(dropped.io);
        drop(dropped.tree);
        assert!(!group_exists(dropped_group).unwrap());
        signal_group(dropped_group, libc::SIGTERM).unwrap();

        let exit_started = tools.join("exit-started");
        fs::write(&script, "#!/bin/sh\nprintf started > \"$1\"\nexit 0\n").unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        let validated = validate_executable(&script, &workspace).unwrap();
        let exit_server = TrustedServer {
            program: validated.path,
            args: Arc::from([exit_started.to_string_lossy().into_owned()]),
            identity: validated.identity,
        };
        let mut exited = spawn(&exit_server, &workspace, &workspace).unwrap();
        let start_deadline = Instant::now() + Duration::from_secs(5);
        while !exit_started.exists() {
            assert!(
                Instant::now() < start_deadline,
                "exit-only child was not scheduled within the bounded test deadline"
            );
            thread::yield_now();
        }
        assert!(
            exited
                .tree
                .wait_for_exit(Instant::now() + Duration::from_secs(1))
                .unwrap()
        );
        assert_eq!(exited.tree.id(), 0);
    }
}
