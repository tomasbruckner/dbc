//! SSH tunnel manager: spawns the system OpenSSH client (`ssh`) to set up a
//! local port forward, rather than embedding an SSH stack. Keys, known_hosts
//! and ssh-agent behave exactly as the user's own `ssh` CLI does.
//!
//! G1 supports key/agent auth only: `BatchMode=yes` makes password prompts
//! fail fast instead of hanging, so interactive password auth is not
//! supported here (surface this in the connection dialog's SSH section).

use std::io::Read;
use std::net::TcpStream;
use std::process::{Child, Command, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use dbc_state::SshTunnelConfig;

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const POLL_STEP: Duration = Duration::from_millis(100);

/// A live SSH port-forward. Killing the child on drop tears the tunnel down.
#[allow(dead_code)]
pub struct Tunnel {
    local_port: u16,
    child: Child,
}

#[allow(dead_code)]
impl Tunnel {
    pub fn local_port(&self) -> u16 {
        self.local_port
    }

    /// Picks a free local port, spawns `ssh -N -L <local>:<target_host>:<target_port> ...`
    /// against the configured bastion, and waits (up to 10s) for the forward
    /// to become connectable.
    pub fn open(
        cfg: &SshTunnelConfig,
        target_host: &str,
        target_port: u16,
    ) -> Result<Tunnel, String> {
        let local_port = pick_free_port()?;

        let program = ssh_binary()?;

        let mut args: Vec<String> = vec![
            "-N".into(),
            "-o".into(),
            "BatchMode=yes".into(),
            "-o".into(),
            "ExitOnForwardFailure=yes".into(),
        ];
        if let Some(key_path) = &cfg.key_path {
            args.push("-i".into());
            args.push(key_path.clone());
        }
        args.push("-p".into());
        args.push(cfg.port.to_string());
        args.push("-L".into());
        args.push(format!("{local_port}:{target_host}:{target_port}"));
        args.push(format!("{}@{}", cfg.user, cfg.host));

        let mut child = spawn_ssh(program, &args)?;

        let deadline = Instant::now() + CONNECT_TIMEOUT;
        loop {
            if TcpStream::connect(("127.0.0.1", local_port)).is_ok() {
                return Ok(Tunnel { local_port, child });
            }

            // If ssh already exited (e.g. auth failure), stop polling early.
            if let Ok(Some(status)) = child.try_wait() {
                let tail = read_stderr_tail(&mut child);
                return Err(format!(
                    "ssh exited early ({status}) before the tunnel came up: {tail}"
                ));
            }

            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                let tail = read_stderr_tail(&mut child);
                return Err(format!(
                    "ssh tunnel did not come up within {CONNECT_TIMEOUT:?}: {tail}"
                ));
            }

            std::thread::sleep(POLL_STEP);
        }
    }
}

impl Drop for Tunnel {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Binds an ephemeral local port, reads it back, then drops the listener so
/// `ssh` can bind it moments later.
#[allow(dead_code)]
fn pick_free_port() -> Result<u16, String> {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0))
        .map_err(|e| format!("failed to pick a free local port: {e}"))?;
    let port = listener
        .local_addr()
        .map_err(|e| format!("failed to read local port: {e}"))?
        .port();
    drop(listener);
    Ok(port)
}

/// Spawns `program` with `args`, stdin closed and stderr piped so callers can
/// read an error tail on failure. Any error message contains "ssh" so
/// callers/tests can identify ssh-related spawn failures.
#[allow(dead_code)]
fn spawn_ssh(program: &str, args: &[String]) -> Result<Child, String> {
    let mut cmd = Command::new(program);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }

    cmd.spawn()
        .map_err(|e| format!("failed to spawn ssh ('{program}'): {e}"))
}

/// Locates the `ssh` binary once (via `where`/`which`) purely for a friendlier
/// "not found" error; the actual spawn still resolves via PATH.
#[allow(dead_code)]
fn ssh_binary() -> Result<&'static str, String> {
    static FOUND: OnceLock<bool> = OnceLock::new();
    let found = *FOUND.get_or_init(|| {
        #[cfg(windows)]
        let probe = Command::new("where").arg("ssh").output();
        #[cfg(not(windows))]
        let probe = Command::new("which").arg("ssh").output();

        matches!(probe, Ok(o) if o.status.success())
    });
    if found {
        Ok("ssh")
    } else {
        Err("ssh binary not found on PATH (system OpenSSH client is required for SSH tunnels)"
            .to_string())
    }
}

/// Best-effort read of whatever stderr is already buffered, without blocking
/// indefinitely (the child has already been killed/waited by the caller).
#[allow(dead_code)]
fn read_stderr_tail(child: &mut Child) -> String {
    let mut buf = String::new();
    if let Some(stderr) = child.stderr.as_mut() {
        let _ = stderr.read_to_string(&mut buf);
    }
    let trimmed = buf.trim();
    if trimmed.is_empty() {
        "(no stderr output)".to_string()
    } else {
        trimmed.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn free_port_is_free() {
        let p = pick_free_port().unwrap();
        assert!(std::net::TcpListener::bind(("127.0.0.1", p)).is_ok());
    }

    #[test]
    fn missing_binary_is_a_value_error() {
        let e = spawn_ssh("definitely-not-ssh-binary-xyz", &["-V".into()]).unwrap_err();
        assert!(e.contains("ssh"));
    }
}
