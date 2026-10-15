//! The tether: a server runs in its own process group under a small `sh`
//! watcher that takes the group down when hearth's pipe to it closes, so a
//! server never outlives the supervisor, even after SIGKILL.

use std::process::{Command, ExitStatus};
use std::time::Duration;

/// The tether. `$1` is the stop grace in seconds, the rest is the server.
///
/// * The server's stdin is `/dev/null`; the supervisor's pipe is fd 3 of a
///   watcher subshell. When the pipe closes, the watcher sends SIGTERM to
///   the process group (`kill 0`), waits out the grace, then sends SIGKILL to
///   it. The watcher outlives the tether if it has to, and while it lives the
///   group id cannot be reused, so the signals only ever reach this group.
/// * The tether waits for the server and exits with its status. A SIGTERM to
///   the tether marks a stop: once the server is gone, the tether kills what
///   is left of the group (a wrapper's children) and itself.
/// * `wait` returning 127 means the pid is no longer this shell's child.
#[cfg(unix)]
const TETHER: &str = r#"g=$1; shift
trap 'stopping=1' TERM
exec 3<&0
"$@" </dev/null &
c=$!
( trap : TERM; read _ <&3; kill -TERM 0; sleep "$g"; kill -KILL 0 ) </dev/null >/dev/null 2>&1 &
while :; do
  wait $c
  s=$?
  [ $s -eq 127 ] && break
  kill -0 $c 2>/dev/null || break
done
[ -n "$stopping" ] && kill -KILL 0
exit $s"#;

/// Runs `c` under the tether, in a new process group: same program,
/// arguments, environment and directory. Its stdin is the supervisor's end
/// of the pipe; dropping it stops the server.
#[cfg(unix)]
#[must_use]
pub fn tethered(c: &Command, stop_grace: Duration) -> Command {
    use std::os::unix::process::CommandExt as _;
    let mut t = Command::new("sh");
    t.arg("-c")
        .arg(TETHER)
        .arg("sh")
        .arg(format!("{:.3}", stop_grace.as_secs_f64()))
        .arg(c.get_program())
        .args(c.get_args())
        .stdin(std::process::Stdio::piped())
        .process_group(0);
    for (k, v) in c.get_envs() {
        match v {
            Some(v) => t.env(k, v),
            None => t.env_remove(k),
        };
    }
    if let Some(d) = c.get_current_dir() {
        t.current_dir(d);
    }
    t
}

#[cfg(not(unix))]
#[must_use]
pub fn tethered(c: &Command, _stop_grace: Duration) -> Command {
    let mut t = Command::new(c.get_program());
    t.args(c.get_args());
    t
}

/// The shell reports a child killed by signal `n` as exit status `128 + n`;
/// a convention, not a policy value.
#[cfg(unix)]
const SHELL_SIGNAL_BASE: i32 = 128;

/// `(exit code, signal)`. Under the tether a server killed by signal `n`
/// is reported the way the shell reports it, as exit code `128 + n`.
pub(super) fn ended(status: ExitStatus) -> (Option<i32>, Option<i32>) {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt as _;
        if let Some(signal) = status.signal() {
            return (None, Some(signal));
        }
        if let Some(code) = status.code().filter(|&c| c > SHELL_SIGNAL_BASE) {
            return (None, Some(code - SHELL_SIGNAL_BASE));
        }
    }
    (status.code(), None)
}
