//! Running a Docker daemon inside a sandbox, shared by every sandbox backend.
//!
//! Backends decide how a sandbox is isolated (see `docker_in_sandbox` in the config); once a
//! sandbox may run Docker, the backend runs [`start_dockerd_command`] in it as root whenever the
//! sandbox is created or reconnected to. The command is idempotent and needs nothing from the
//! image beyond `dockerd` (Debian's `docker.io`), `pgrep` and `nohup`, so it works the same in a
//! Docker container (via `docker exec -u root`) and in an LXC container (via `lxc-attach`).

/// Where dockerd's output goes inside the sandbox.
pub const DOCKERD_LOG: &str = "/var/log/dockerd.log";

/// Command that starts dockerd inside the sandbox unless it is already running. Run it as root,
/// detached; it returns as soon as dockerd has been launched.
///
/// The socket is owned by `socket_gid` (the sandbox user's group) so `docker` works without sudo.
/// A restarted sandbox keeps its old pid file, which dockerd refuses to start over, so it is
/// removed first.
pub fn start_dockerd_command(socket_gid: u32) -> Vec<String> {
    vec![
        "sh".to_string(),
        "-c".to_string(),
        format!(
            "pgrep -x dockerd >/dev/null 2>&1 && exit 0; \
             rm -f /var/run/docker.pid; \
             nohup dockerd --group {socket_gid} >{DOCKERD_LOG} 2>&1 &"
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
        assert!(cmd[2].starts_with("pgrep -x dockerd >/dev/null 2>&1 && exit 0;"));
        assert!(cmd[2].contains("dockerd --group 1000 >/var/log/dockerd.log"));
    }
}
