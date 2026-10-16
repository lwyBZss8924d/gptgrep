use anyhow::{Result, anyhow, ensure};
use std::time::Duration;
use tokio::process::{Child, Command};

pub(crate) fn configure(command: &mut Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.as_std_mut().process_group(0);
    }
}

/// Owns only the fresh process group created for this invocation.
pub(crate) struct OwnedGroup {
    #[cfg(unix)]
    id: i32,
    armed: bool,
}
impl OwnedGroup {
    pub fn new(child: &Child) -> Result<Self> {
        let id = child
            .id()
            .ok_or_else(|| anyhow!("host_codex_process_identity_unavailable"))?;
        ensure!(
            id > 1 && id <= i32::MAX as u32,
            "host_codex_process_identity_invalid"
        );
        Ok(Self {
            #[cfg(unix)]
            id: id as i32,
            armed: true,
        })
    }
    pub async fn stop(&mut self, child: &mut Child) -> Result<()> {
        #[cfg(unix)]
        {
            self.signal_or_quiescent(libc::SIGTERM).await?;
            // Keep the leader unreaped until both group signals complete, preventing PID reuse.
            tokio::time::sleep(Duration::from_millis(200)).await;
            self.signal_or_quiescent(libc::SIGKILL).await?;
        }
        #[cfg(not(unix))]
        {
            let _ = child.start_kill();
        }
        reap_outcome(tokio::time::timeout(Duration::from_secs(5), child.wait()).await)?;
        self.armed = false;
        Ok(())
    }
    #[cfg(unix)]
    fn signal(&self, signal: i32) -> std::io::Result<()> {
        // SAFETY: id belongs to the fresh group created with process_group(0), never a caller PID.
        let result = unsafe { libc::kill(-self.id, signal) };
        if result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
            return Ok(());
        }
        Err(std::io::Error::last_os_error())
    }
    #[cfg(unix)]
    async fn signal_or_quiescent(&self, signal: i32) -> Result<()> {
        if let Err(error) = self.signal(signal) {
            // Darwin can return EPERM for a group containing only an unreaped zombie.
            if error.raw_os_error() != Some(libc::EPERM) || self.has_live_members().await? {
                return Err(anyhow!("host_codex_process_group_signal_failed"));
            }
        }
        Ok(())
    }
    #[cfg(unix)]
    async fn has_live_members(&self) -> Result<bool> {
        let output = tokio::time::timeout(
            Duration::from_secs(1),
            Command::new("/bin/ps")
                .args(["-axo", "pid=,pgid=,stat="])
                .kill_on_drop(true)
                .output(),
        )
        .await
        .map_err(|_| anyhow!("host_codex_process_group_check_timeout"))?
        .map_err(|_| anyhow!("host_codex_process_group_check_failed"))?;
        ensure!(
            output.status.success() && output.stdout.len() <= 512 * 1024,
            "host_codex_process_group_check_unavailable"
        );
        let text = std::str::from_utf8(&output.stdout)
            .map_err(|_| anyhow!("host_codex_process_metadata_invalid"))?;
        for line in text.lines() {
            let mut fields = line.split_whitespace();
            let _pid = fields
                .next()
                .and_then(|v| v.parse::<i32>().ok())
                .ok_or_else(|| anyhow!("host_codex_process_metadata_invalid"))?;
            let group = fields
                .next()
                .and_then(|v| v.parse::<i32>().ok())
                .ok_or_else(|| anyhow!("host_codex_process_metadata_invalid"))?;
            let status = fields
                .next()
                .ok_or_else(|| anyhow!("host_codex_process_metadata_invalid"))?;
            if group == self.id && !status.starts_with('Z') {
                return Ok(true);
            }
        }
        Ok(false)
    }
}
impl Drop for OwnedGroup {
    fn drop(&mut self) {
        #[cfg(unix)]
        if self.armed {
            // Cancellation cannot await. Kill the entire owned group; Child.kill_on_drop reaps
            // the leader through Tokio's child lifecycle.
            let _ = self.signal(libc::SIGKILL);
        }
    }
}

/// The owned leader must be reaped within the cleanup limit. A reap that is
/// still pending at the limit and a wait that fails are reported apart; the
/// group stays armed in both cases.
fn reap_outcome<T>(
    reaped: std::result::Result<std::io::Result<T>, tokio::time::error::Elapsed>,
) -> Result<T> {
    reaped
        .map_err(|_| anyhow!("host_codex_process_reap_timeout"))?
        .map_err(|_| anyhow!("host_codex_process_reap_failed"))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_failed_reap_wait_is_not_reported_as_a_reap_timeout() {
        let mut child = Command::new("/bin/sleep").arg("30").spawn().unwrap();
        let pid = child.id().unwrap() as libc::pid_t;
        // Reap the child outside Tokio, so its owned wait fails at once.
        // SAFETY: pid is this test's own child and is not yet reaped.
        unsafe {
            assert_eq!(libc::kill(pid, libc::SIGKILL), 0);
            let mut status = 0;
            assert_eq!(libc::waitpid(pid, &mut status, 0), pid);
        }
        let reaped = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;
        assert!(matches!(reaped, Ok(Err(_))));
        assert_eq!(
            format!("{:#}", reap_outcome(reaped).unwrap_err()),
            "host_codex_process_reap_failed"
        );
        let elapsed = tokio::time::timeout(Duration::ZERO, std::future::pending::<()>())
            .await
            .unwrap_err();
        assert_eq!(
            format!("{:#}", reap_outcome::<()>(Err(elapsed)).unwrap_err()),
            "host_codex_process_reap_timeout"
        );
        assert_eq!(reap_outcome(Ok(Ok(7))).unwrap(), 7);
    }
}
