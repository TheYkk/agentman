//! Running a Docker daemon inside a sandbox, shared by every sandbox backend.
//!
//! Backends decide how a sandbox is isolated (see `docker_in_sandbox` in the config); once a
//! sandbox may run Docker, the backend runs [`start_dockerd_command`] in it as root whenever the
//! sandbox is created or reconnected to. The command is idempotent and needs nothing from the
//! image beyond `dockerd` (Debian's `docker.io`), `pgrep` and `nohup`, so it works the same in a
//! Docker container (via `docker exec -u root`) and in an LXC container (via `lxc-attach`).

/// Where dockerd's output goes inside the sandbox.
pub const DOCKERD_LOG: &str = "/var/log/dockerd.log";

/// How long [`start_dockerd_command`] waits for the daemon to answer, in seconds.
pub const DOCKERD_READY_TIMEOUT_SECS: u32 = 20;

/// Command that starts dockerd inside the sandbox unless it is already running, then waits (up
/// to [`DOCKERD_READY_TIMEOUT_SECS`]) until it answers on its socket, so the session's first
/// `docker` command does not race daemon startup. Run it as root and wait for it to exit; dockerd
/// keeps running in the background. Exits 1 if the daemon never became ready.
///
/// The socket is owned by `socket_gid` (the sandbox user's group) so `docker` works without sudo.
/// A restarted sandbox keeps its old pid file, which dockerd refuses to start over, so it is
/// removed first.
pub fn start_dockerd_command(socket_gid: u32) -> Vec<String> {
    let tries = DOCKERD_READY_TIMEOUT_SECS * 4;
    vec![
        "sh".to_string(),
        "-c".to_string(),
        format!(
            "if ! pgrep -x dockerd >/dev/null 2>&1; then \
               rm -f /var/run/docker.pid; \
               nohup dockerd --group {socket_gid} >{DOCKERD_LOG} 2>&1 </dev/null & \
             fi; \
             i=0; \
             while [ $i -lt {tries} ]; do \
               if command -v docker >/dev/null 2>&1; then \
                 docker version >/dev/null 2>&1 && exit 0; \
               elif [ -S /var/run/docker.sock ]; then \
                 exit 0; \
               fi; \
               sleep 0.25; i=$((i+1)); \
             done; \
             echo \"dockerd did not become ready; see {DOCKERD_LOG}\" >&2; exit 1"
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn start_command_is_idempotent_and_sets_socket_group() {
        let cmd = start_dockerd_command(1000);
        assert_eq!(&cmd[..2], ["sh", "-c"]);
        assert!(cmd[2].starts_with("if ! pgrep -x dockerd >/dev/null 2>&1; then"));
        assert!(cmd[2].contains("dockerd --group 1000 >/var/log/dockerd.log"));
        assert!(cmd[2].contains("docker version"));
    }
}
