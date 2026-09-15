//! Spike B (M0): subprocess supervision on `tokio::process`.
//!
//! **Disposable spike** — the production supervisor is rewritten in M2
//! (`crates/runtime`): bounded stdout/stderr ring buffers, exit
//! classification, health deadline, `FailureClass`. This file only proves
//! the mechanisms the M2 supervisor must have:
//!
//! 1. **Continuous stdout/stderr drain** (`AsyncReadExt::read` loops) keeps
//!    the OS pipe buffers from filling: a chatty child that writes >64KB
//!    (the typical pipe capacity) on stdout *or* stderr keeps running,
//!    because a non-draining supervisor would deadlock the child once a
//!    buffer fills.
//! 2. **`terminate(grace)` escalation**: graceful signal → grace (300ms in
//!    tests, 5s in production — ADR-0002) → force kill → stdout/stderr
//!    EOF, while keeping both pipes drained (a large log flush on the
//!    signal must not wedge the child in `write()`). On unix the child is
//!    placed in its own process group (`process_group(0)` before exec) and
//!    signals are delivered to the whole group (`kill(-pgid, sig)`), so
//!    descendants of a wrapper shell (e.g. a backgrounded `sleep`) are
//!    reaped and cannot hold the pipes open forever (no EOF ⇒ the
//!    supervisor could never detect exit).
//!
//!    **Ownership rule (PGID-reuse safety, ADR-0002)**: a group signal
//!    is only ever sent for a PGID whose ownership is verifiable —
//!    while the child (group leader) is alive, or because a group
//!    member recorded in the `group_members` snapshot (pid + starttime,
//!    scanned while the child was alive) still exists, which keeps the
//!    PGID number from having been freed and reused. Once the leader is
//!    gone and no snapshot member can be found, the PGID is never
//!    signaled again. Accepted residual gap: a descendant spawned in the
//!    last refresh interval before the leader's exit may survive
//!    (killing a stranger is strictly worse than a survivor); M2's
//!    dedicated cgroup closes even that gap.
//! 3. Windows has no process-group semantics: `terminate()` force-kills a
//!    single process (`TerminateProcess` via `Child::kill`); any child
//!    descendants become orphans (acceptable — `llama-server` /
//!    `ninfer-serve` are single processes; see ADR-0002). M2 collects
//!    both streams into bounded ring buffers with classification.
//!
//! The fake children are per-OS one-liners chosen at runtime via
//! `std::env::consts::OS` so the same test binary exercises the unix
//! branch on WSL and the Windows branch on the dev machine.

use std::io::{Error as IoError, ErrorKind};
use std::time::Duration;

use tokio::io::AsyncReadExt;
use tokio::process::{Child, ChildStderr, ChildStdout, Command};
use tokio::time::{Instant, sleep};

/// Tail of stdout kept for later failure diagnostics (M2 replaces this
/// with a proper bounded ring buffer).
const STDOUT_TAIL_BYTES: usize = 4 * 1024;

/// Poll interval for `try_wait` in `terminate`.
const POLL: Duration = Duration::from_millis(20);

/// Extra hard deadline after the force kill, in case a group member
/// refuses to die (should not happen with KILL/`TerminateProcess`).
const POST_KILL_HARD_TIMEOUT: Duration = Duration::from_secs(10);

/// (unix) Brief settle time after the direct child is reaped, before
/// probing whether group descendants are still alive.
#[cfg(unix)]
const GROUP_SETTLE: Duration = Duration::from_millis(100);

/// Which fake child to spawn. `IgnoresTerm` / `Grandchild` are unix-only
/// (Windows has no SIGTERM to trap; `TerminateProcess` cannot be trapped).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FakeChild {
    /// Forever-chatty producer: ~200–400 bytes per tick on BOTH stdout
    /// and stderr, forever.
    Verbose,
    /// (unix) `trap "" TERM` bash loop — survives SIGTERM, proves the
    /// SIGKILL escalation path.
    IgnoresTerm,
    /// (unix) `sleep 100 &` grandchild, then `exec` into a forever loop —
    /// proves group-kill reaps the child's own descendants.
    Grandchild,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminateOutcome {
    /// Died during the grace window after the graceful signal.
    Graceful,
    /// Survived the grace window; force kill was required.
    Escalated,
    /// Already dead before `terminate` was called.
    AlreadyExited,
}

/// Result of a bounded `drain_stdout` budget.
#[derive(Debug, Clone, Copy)]
pub struct DrainResult {
    /// Bytes read during this call.
    pub new_bytes: u64,
    /// Total bytes read since spawn.
    pub total_bytes: u64,
    /// Set when the read returned 0 (all writers of the pipe are dead).
    pub eof: bool,
}

/// Minimal child supervisor. See module docs for what it proves.
pub struct ChildSupervisor {
    child: Child,
    stdout: Option<ChildStdout>,
    stderr: Option<ChildStderr>,
    pid: u32,
    stdout_total: u64,
    stdout_tail: Vec<u8>,
    stderr_total: u64,
    stderr_tail: Vec<u8>,
    exited: bool,
    /// Unix: `(pid, starttime)` of every process seen in our group
    /// WHILE THE CHILD WAS ALIVE (see `refresh_ownership` /
    /// `group_is_ours`). Only members from this snapshot may justify
    /// signaling the group after the leader has exited — a PGID whose
    /// snapshot members are all gone may already have been freed and
    /// reused, and is never signaled.
    #[cfg(unix)]
    group_members: Vec<(u32, u64)>,
    #[cfg(unix)]
    pgid: u32,
}

impl std::fmt::Debug for ChildSupervisor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChildSupervisor")
            .field("pid", &self.child.id())
            .field("stdout_total", &self.stdout_total)
            .field("stderr_total", &self.stderr_total)
            .field("exited", &self.exited)
            .finish()
    }
}

impl ChildSupervisor {
    /// Spawn the fake child for this OS.
    ///
    /// On unix the child is created in its own process group
    /// (`process_group(0)` ⇒ child pid == pgid == group leader) so
    /// `kill(-pgid, sig)` reaches it and every descendant.
    pub async fn spawn(kind: FakeChild) -> std::io::Result<Self> {
        let mut cmd = fake_child_command(kind)?;
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());
        #[cfg(unix)]
        {
            // `tokio::process::Command::process_group` (safe API): place the
            // child in a new process group (pid == pgid == group leader) so
            // `kill(-pgid, sig)` reaches descendants.
            cmd.process_group(0);
        }
        let mut child = cmd.spawn()?;
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let pid = child.id().unwrap_or(0);
        Ok(Self {
            child,
            stdout,
            stderr,
            pid,
            stdout_total: 0,
            stdout_tail: Vec::new(),
            stderr_total: 0,
            stderr_tail: Vec::new(),
            exited: false,
            #[cfg(unix)]
            group_members: Vec::new(),
            #[cfg(unix)]
            pgid: pid,
        })
    }

    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// The child is the group leader, so on unix `pgid == pid`.
    #[cfg(unix)]
    pub fn pgid(&self) -> u32 {
        self.pgid
    }

    pub fn stdout_total(&self) -> u64 {
        self.stdout_total
    }

    pub fn stdout_tail(&self) -> &[u8] {
        &self.stdout_tail
    }

    pub fn stderr_total(&self) -> u64 {
        self.stderr_total
    }

    pub fn stderr_tail(&self) -> &[u8] {
        &self.stderr_tail
    }

    /// True if the child has not exited yet (reaps it via `try_wait` if so).
    pub fn is_alive(&mut self) -> std::io::Result<bool> {
        if self.exited {
            return Ok(false);
        }
        // While the child is alive the PGID is provably ours: keep the
        // ownership snapshot fresh for post-exit cleanup.
        #[cfg(unix)]
        self.refresh_ownership();
        match self.child.try_wait()? {
            Some(_) => {
                self.exited = true;
                Ok(false)
            }
            None => Ok(true),
        }
    }

    /// (unix) Refresh `group_members` with a fresh /proc scan. Only
    /// meaningful while the child is still alive — after that the PGID
    /// may already be free and reused, and scanning would record
    /// unrelated processes as "ours", so this is a no-op once `exited`.
    ///
    /// The refresh interval bounds a liveness gap: a descendant spawned
    /// in the window between the last refresh and the leader's exit is
    /// not in the snapshot and may survive post-exit cleanup. That is
    /// the accepted spike-level trade — killing a stranger (a reused
    /// PGID) is strictly worse than a surviving descendant — and M2's
    /// dedicated cgroup (systemd scope) closes even that gap because
    /// cgroup membership is ownership-based (see ADR-0002).
    #[cfg(unix)]
    fn refresh_ownership(&mut self) {
        if self.exited {
            return;
        }
        self.group_members = scan_group_members(self.pgid);
    }

    /// (unix) True when at least one member from the `group_members`
    /// snapshot (all verified as group members while the child was
    /// alive) still exists with the same starttime. Such a member holds
    /// the PGID number, so the number cannot have been freed and reused
    /// — signaling `-pgid` can only reach our own (or their) group.
    /// An empty snapshot (child never observed alive, or `/proc`
    /// unavailable) always returns false: no ownership evidence, no
    /// signal.
    #[cfg(unix)]
    fn group_is_ours(&self) -> bool {
        self.group_members
            .iter()
            .any(|(pid, st)| member_still_there(*pid, *st))
    }

    /// Read stdout for up to `budget`, continuously. This is the whole
    /// point of the spike: without it the OS pipe buffer (≈64KB) fills and
    /// the child blocks in `write()` forever.
    pub async fn drain_stdout(&mut self, budget: Duration) -> std::io::Result<DrainResult> {
        let before = self.stdout_total;
        let deadline = Instant::now() + budget;
        let mut eof = false;
        let mut buf = [0u8; 8192];
        while let Some(stdout) = self.stdout.as_mut() {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            tokio::select! {
                r = stdout.read(&mut buf) => match r {
                    Ok(0) => {
                        eof = true;
                        break;
                    }
                    Ok(n) => {
                        self.stdout_total += n as u64;
                        self.push_tail(&buf[..n]);
                    }
                    Err(e) => return Err(e),
                },
                _ = sleep(remaining) => break,
            }
        }
        Ok(DrainResult {
            new_bytes: self.stdout_total - before,
            total_bytes: self.stdout_total,
            eof,
        })
    }

    /// Drain stdout until EOF (read → 0). Only valid once every process
    /// holding the write end of the pipe is dead — i.e. after
    /// `terminate` killed the whole process group.
    pub async fn drain_stdout_to_eof(&mut self) -> std::io::Result<u64> {
        let mut buf = [0u8; 8192];
        while let Some(stdout) = self.stdout.as_mut() {
            match stdout.read(&mut buf).await {
                Ok(0) => break,
                Ok(n) => {
                    self.stdout_total += n as u64;
                    self.push_tail(&buf[..n]);
                }
                Err(e) => return Err(e),
            }
        }
        Ok(self.stdout_total)
    }

    /// Poll BOTH pipes in parallel and apply one ready chunk from
    /// whichever stream is ready (the other stays pending in the OS pipe
    /// buffer until the next poll — that is exactly the continuous-drain
    /// contract: `terminate` awaits with both pipes pumped, so a child
    /// that flushes a large log on the signal cannot wedge in `write()`
    /// and be incorrectly escalated to a force kill).
    ///
    /// `select!` over two `self.pump_*_once()` calls would not compile
    /// (two simultaneous `&mut self` borrows), so each stream is polled
    /// through its own disjoint field borrow (`self.stdout` / `self.stderr`).
    async fn pump_streams_once(&mut self) -> std::io::Result<()> {
        let mut obuf = [0u8; 8192];
        let mut ebuf = [0u8; 8192];
        let out_pipe = self.stdout.as_mut();
        let err_pipe = self.stderr.as_mut();
        let mut ores: Option<std::io::Result<usize>> = None;
        let mut eres: Option<std::io::Result<usize>> = None;

        let o = async {
            ores = Some(match out_pipe {
                Some(p) => p.read(&mut obuf).await,
                // This pipe already reached EOF: wait forever so `select!`
                // never busy-spins on a permanently-ready read.
                None => std::future::pending().await,
            });
        };
        let e = async {
            eres = Some(match err_pipe {
                Some(p) => p.read(&mut ebuf).await,
                None => std::future::pending().await,
            });
        };

        tokio::select! {
            _ = o => {}
            _ = e => {}
        }

        // Disable any stream that just hit EOF so future polls take the
        // pending branch (see above).
        if matches!(ores, Some(Ok(0))) {
            self.stdout = None;
        }
        if matches!(eres, Some(Ok(0))) {
            self.stderr = None;
        }
        if let Some(r) = ores {
            match r {
                Ok(0) => {}
                Ok(n) => {
                    self.stdout_total += n as u64;
                    self.push_tail(&obuf[..n]);
                }
                Err(e) => return Err(e),
            }
        }
        if let Some(r) = eres {
            match r {
                Ok(0) => {}
                Ok(n) => {
                    self.stderr_total += n as u64;
                    self.push_stderr_tail(&ebuf[..n]);
                }
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    /// Keep the most recent `STDOUT_TAIL_BYTES` of stdout for failure
    /// diagnostics (M2 replaces this with a bounded ring buffer).
    fn push_tail(&mut self, chunk: &[u8]) {
        self.stdout_tail.extend_from_slice(chunk);
        if self.stdout_tail.len() > STDOUT_TAIL_BYTES {
            let drop_n = self.stdout_tail.len() - STDOUT_TAIL_BYTES;
            self.stdout_tail.drain(..drop_n);
        }
    }

    /// Keep the most recent `STDOUT_TAIL_BYTES` of stderr for failure
    /// diagnostics (M2 replaces this with a bounded ring buffer).
    fn push_stderr_tail(&mut self, chunk: &[u8]) {
        self.stderr_tail.extend_from_slice(chunk);
        if self.stderr_tail.len() > STDOUT_TAIL_BYTES {
            let drop_n = self.stderr_tail.len() - STDOUT_TAIL_BYTES;
            self.stderr_tail.drain(..drop_n);
        }
    }

    /// Read stderr for up to `budget`, continuously — the stderr twin of
    /// `drain_stdout`. A child that fills the stderr pipe (≈64KB) would
    /// block in `write(2)` and hang on error output alone.
    pub async fn drain_stderr(&mut self, budget: Duration) -> std::io::Result<DrainResult> {
        let before = self.stderr_total;
        let deadline = Instant::now() + budget;
        let mut eof = false;
        let mut buf = [0u8; 8192];
        while let Some(stderr) = self.stderr.as_mut() {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            tokio::select! {
                r = stderr.read(&mut buf) => match r {
                    Ok(0) => {
                        eof = true;
                        break;
                    }
                    Ok(n) => {
                        self.stderr_total += n as u64;
                        self.push_stderr_tail(&buf[..n]);
                    }
                    Err(e) => return Err(e),
                },
                _ = sleep(remaining) => break,
            }
        }
        Ok(DrainResult {
            new_bytes: self.stderr_total - before,
            total_bytes: self.stderr_total,
            eof,
        })
    }

    /// Drain stderr until EOF (read → 0).
    pub async fn drain_stderr_to_eof(&mut self) -> std::io::Result<u64> {
        let mut buf = [0u8; 8192];
        while let Some(stderr) = self.stderr.as_mut() {
            match stderr.read(&mut buf).await {
                Ok(0) => break,
                Ok(n) => {
                    self.stderr_total += n as u64;
                    self.push_stderr_tail(&buf[..n]);
                }
                Err(e) => return Err(e),
            }
        }
        Ok(self.stderr_total)
    }

    /// Two-phase shutdown: graceful signal, wait `grace`, then force.
    ///
    /// - unix: `kill -s TERM -- -pgid` → grace → `kill -s KILL -- -pgid`.
    ///   Before returning `Graceful` or `Escalated` the whole group is
    ///   probed (signal 0): a surviving descendant (a TERM-ignoring child
    ///   or a grandchild) would keep the stdout pipe open and hang the
    ///   EOF drain, so survivors force the KILL phase, and survivors
    ///   after the KILL hit the hard deadline (`kill_timeout`). Both
    ///   shutdown loops keep draining both pipes (see `pump_streams_once`) so
    ///   a large log flush on the signal cannot wedge the child in
    ///   `write()` and force an unnecessary escalation.
    /// - Windows: `Child::kill()` (`TerminateProcess` — immediate, there is
    ///   no SIGTERM) on the first phase; the force phase is a no-op retry.
    ///
    /// Grace in tests is 300ms; production value is 5s (ADR-0002).
    pub async fn terminate(&mut self, grace: Duration) -> std::io::Result<TerminateOutcome> {
        // Reap before choosing a phase: a child that already exited on its
        // own must classify as `AlreadyExited` regardless of whether the
        // caller happened to poll it first.
        self.reap()?;
        if self.exited {
            // The direct child is reaped, but on unix group descendants
            // (e.g. a backgrounded `sleep` that outlived its leader) may
            // still be alive and holding the stdout pipe.
            #[cfg(unix)]
            {
                // Kill the group ONLY while its ownership is still
                // verifiable: at least one member recorded in
                // `group_members` (scanned while the child was alive)
                // must still exist with the same starttime — it holds
                // the PGID number, so the number cannot have been freed
                // and reused, and `kill(-pgid)` can only reach our own
                // group. If the snapshot is empty (the child was never
                // observed alive, or `/proc` was unavailable), NO signal
                // is sent: without an ownership record the PGID must be
                // treated as unownable, and any descendants are accepted
                // as survivors (spike-level limitation; M2's dedicated
                // cgroup makes this moot — see ADR-0002).
                if !self.group_members.is_empty() && self.group_is_ours() {
                    let pgid = self.pgid;
                    let _ = signal_group(pgid, "KILL");
                }
                let deadline = Instant::now() + POST_KILL_HARD_TIMEOUT;
                while self.group_is_ours() && Instant::now() < deadline {
                    sleep(POLL).await;
                }
                if self.group_is_ours() {
                    return Err(IoError::new(
                        ErrorKind::TimedOut,
                        "child group still alive after force kill",
                    ));
                }
            }
            return Ok(TerminateOutcome::AlreadyExited);
        }
        // Phase 1 — graceful.
        #[cfg(unix)]
        {
            // Signals the group, not just the child: descendants (wrapper
            // shell's `sleep` jobs, etc.) must die or they keep the stdout
            // pipe open and EOF never arrives.
            let _ = signal_group(self.pgid, "TERM");
        }
        #[cfg(not(unix))]
        {
            let _ = self.child.kill().await;
        }
        let deadline = Instant::now() + grace;
        loop {
            // While the child is alive the PGID is provably ours: keep
            // the ownership snapshot fresh so the post-exit decisions
            // below know which pids are verifiably ours.
            #[cfg(unix)]
            self.refresh_ownership();
            if self.reap()? {
                #[cfg(unix)]
                {
                    // Settle window with continued draining: a descendant
                    // may still be flushing both pipes, and stopping the
                    // drain would wedge it in `write()` and misclassify it
                    // as a survivor (spurious SIGKILL escalation).
                    let settle = Instant::now() + GROUP_SETTLE;
                    loop {
                        let remaining = settle.saturating_duration_since(Instant::now());
                        if remaining.is_zero() {
                            break;
                        }
                        tokio::select! {
                            r = self.pump_streams_once() => {
                                r?;
                            }
                            _ = sleep(remaining) => break,
                        }
                    }
                    // Ownership check (see `group_is_ours`): some member
                    // recorded while the child was alive still exists ⇒
                    // the PGID number is still held by OUR group ⇒ safe
                    // to escalate. If none is left, any process currently
                    // using the number is NOT verifiably ours and is
                    // never signaled — `Graceful` is correct: nothing we
                    // own holds the pipes.
                    if !self.group_is_ours() {
                        return Ok(TerminateOutcome::Graceful);
                    }
                    // Verified-own group members survived TERM ⇒ escalate.
                    break;
                }
                #[cfg(not(unix))]
                {
                    // No group semantics: direct child reaped ⇒ done.
                    return Ok(TerminateOutcome::Graceful);
                }
            }
            if Instant::now() >= deadline {
                break;
            }
            // Keep draining BOTH pipes while waiting: a child that flushes
            // a large log on TERM must not wedge in `write()` (see
            // `pump_streams_once`).
            tokio::select! {
                r = self.pump_streams_once() => {
                    r?;
                }
                _ = sleep(POLL) => {}
            }
        }
        // Phase 2 — force.
        #[cfg(unix)]
        {
            // Signal only while ownership is verifiable. If the child is
            // still alive here the PGID is provably ours; if it died
            // during grace, the snapshot (refreshed at the top of every
            // grace-loop iteration while it was alive) decides.
            let owned = !self.exited || self.group_is_ours();
            if owned {
                let _ = signal_group(self.pgid, "KILL");
            }
        }
        #[cfg(not(unix))]
        {
            let _ = self.child.kill().await;
        }
        let hard = Instant::now() + POST_KILL_HARD_TIMEOUT;
        loop {
            #[cfg(unix)]
            self.refresh_ownership();
            if self.reap()? {
                #[cfg(unix)]
                let survivors = self.group_is_ours();
                #[cfg(not(unix))]
                let survivors = false;
                if !survivors {
                    return Ok(TerminateOutcome::Escalated);
                }
                // Descendants survived even the group KILL (should be
                // impossible) — let the hard deadline below classify the
                // run as `kill_timeout` instead of returning early.
            }
            if Instant::now() >= hard {
                #[cfg(unix)]
                {
                    return Err(IoError::new(
                        ErrorKind::TimedOut,
                        "child group still alive after force kill",
                    ));
                }
                // Do NOT synthesize a successful outcome: if the child is
                // still not confirmed dead after the force kill, that is a
                // `kill_timeout` on every platform (on Windows
                // `TerminateProcess` normally succeeds and this branch is
                // rarely taken).
                #[cfg(not(unix))]
                {
                    return Err(IoError::new(
                        ErrorKind::TimedOut,
                        "child still alive after force kill",
                    ));
                }
            }
            tokio::select! {
                r = self.pump_streams_once() => {
                    r?;
                }
                _ = sleep(POLL) => {}
            }
        }
    }

    /// Non-blocking reap; true when the child has exited.
    fn reap(&mut self) -> std::io::Result<bool> {
        if self.exited {
            return Ok(true);
        }
        match self.child.try_wait()? {
            Some(_) => {
                // Observation only: NO signal is sent here. `terminate`
                // still has to probe the group for descendants and may
                // have to report `Escalated`; `Drop` signals the group
                // only while the child is provably alive and unconditionally
                // reaps the zombie via `waitpid` (clearing the PGID
                // number), so a PGID that was freed and reused is never
                // hit.
                self.exited = true;
                Ok(true)
            }
            None => Ok(false),
        }
    }
}

impl Drop for ChildSupervisor {
    /// Best-effort safety net so a test panic never leaks a fake child.
    /// Kills the group, then reaps the direct child explicitly via
    /// `waitpid(2)` — tokio only reaps a dropped `Child` on a best-effort
    /// background basis, which can leave a zombie holding a process slot
    /// AND its PGID number (until reaped, the process-group number cannot
    /// be reused; the reaper therefore also closes the PGID-reuse window
    /// left by an exit that was never observed by `reap`/`is_alive`).
    fn drop(&mut self) {
        // Observe any unobserved exit FIRST: a child that exited without
        // being polled may already have had its PGID freed and reused —
        // only a provably-alive child's PGID may be group-signaled. An
        // error here means the child cannot be verified alive either, so
        // it is treated as exited (no signal).
        match self.child.try_wait() {
            Ok(Some(_)) => self.exited = true,
            Ok(None) => {}
            Err(_) => self.exited = true,
        }
        #[cfg(unix)]
        {
            if !self.exited {
                // Child still alive ⇒ the PGID (== child pid, the group
                // leader) is provably ours: signal the WHOLE group
                // (coreutils `kill`; the M2 production supervisor uses
                // `libc::kill(-pgid, KILL)` instead of shelling out).
                let pgid = self.pgid;
                let _ = std::process::Command::new("kill")
                    .args(["-s", "KILL", "--", &format!("-{}", pgid)])
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status();
            }
            // else: leader exit already observed (`reap`/`is_alive`):
            // the PGID number may already be free and reused, so `Drop`
            // must NEVER group-signal it. `terminate` (which keeps an
            // ownership snapshot) is the only post-exit cleanup path;
            // group descendants of a leader that exited without
            // `terminate` are intentionally left alone (spike-level
            // limitation, recorded in ADR-0002; M2 uses a dedicated
            // cgroup whose membership is ownership-based and immune to
            // pid/PGID reuse).

            // Unconditional explicit reap of the direct child via
            // non-blocking `waitpid(2, WNOHANG)` polling: an exit that was
            // never observed (supervisor dropped without a poll) leaves a
            // zombie that still holds the PGID number; a blocking wait
            // would make the deadline below unreachable and could hang the
            // runtime thread forever if the group kill above failed.
            // ECHILD means tokio already reaped it (harmless); if the
            // child is still alive after the deadline, the safety net
            // gives up (best-effort by design — `terminate()` is the real
            // path).
            let mut status = 0i32;
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
            while std::time::Instant::now() < deadline {
                let r = unsafe { libc::waitpid(self.pid as i32, &mut status, libc::WNOHANG) };
                if r == self.pid as i32 || r < 0 {
                    break; // reaped here, or already reaped by tokio
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        }
        #[cfg(not(unix))]
        {
            if !self.exited {
                let _ = self.child.start_kill();
            }
            // Windows: the OS reaps the process object; nothing to do.
        }
    }
}

/// Build the per-OS fake child command for `kind`.
fn fake_child_command(kind: FakeChild) -> std::io::Result<Command> {
    match std::env::consts::OS {
        "windows" => {
            if kind != FakeChild::Verbose {
                return Err(IoError::new(
                    ErrorKind::Unsupported,
                    format!("{kind:?} is unix-only"),
                ));
            }
            let mut cmd = Command::new("powershell");
            cmd.args([
                "-NoProfile",
                "-Command",
                // ~200 bytes every ~5ms ≈ 40KB/s → >64KB in ~2s.
                "while($true){'x'*200 | Out-String | Write-Output; 'y'*200 | Out-String | Write-Error; Start-Sleep -Milliseconds 5}",
            ]);
            Ok(cmd)
        }
        _ => {
            let script = match kind {
                FakeChild::Verbose => {
                    // ~401 bytes every ~20ms ≈ 20KB/s → >64KB in ~3.5s.
                    "while :; do printf \"%0.s x\" $(seq 1 200); echo; printf \"%0.s y\" $(seq 1 200) >&2; echo >&2; sleep 0.02; done"
                }
                FakeChild::IgnoresTerm => {
                    // `trap "" TERM` = SIG_IGN, which bash keeps while
                    // looping (its foreground `sleep` dies on the group
                    // TERM, bash wakes, sees TERM ignored, continues).
                    "trap \"\" TERM; while :; do echo x; sleep 0.05; done"
                }
                FakeChild::Grandchild => {
                    // Background `sleep 100` stays in the child's process
                    // group (and inherits its stdout pipe); `exec` then
                    // turns the same pid into the loop leader.
                    "sleep 100 & exec bash -c \"while :; do echo x; done\""
                }
            };
            let mut cmd = Command::new("bash");
            cmd.arg("-c").arg(script);
            Ok(cmd)
        }
    }
}

/// (unix, spike shortcut) Send a named signal to a whole process group by
/// shelling out to coreutils `kill` (`kill -s <name> -- -<pgid>`), because
/// `libc`/`nix` are not in the spike's dependency list. The M2 production
/// supervisor calls `libc::kill(-pgid, sig)` directly — no fork, no
/// coreutils dependency. Nonzero `kill` exit (e.g. group already gone) is
/// reported as an error; callers that probe group liveness use
/// `group_has_live_member`.
///
/// SAFETY (see ADR-0002): callers must only signal a PGID whose
/// ownership is verifiable — while the child (group leader) is alive, or
/// when `group_is_ours` confirms a snapshot member (pid + starttime,
/// recorded while the child was alive) still exists, which keeps the
/// PGID number from having been freed and reused. A PGID with no
/// verifiable ownership is NEVER signaled.
#[cfg(unix)]
fn signal_group(pgid: u32, name: &str) -> std::io::Result<()> {
    let target = format!("-{pgid}");
    let status = std::process::Command::new("kill")
        .args(["-s", name, "--", &target])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(IoError::other(format!(
            "kill -s {name} -- {target} exited with {status}"
        )))
    }
}

/// (unix) `(pid, starttime)` of every process in process group `pgid`
/// (zombies included — a zombie still holds the PGID number, which is
/// exactly what makes it a valid ownership anchor). Linux `/proc` scan;
/// returns empty when `/proc` is unavailable, which callers treat as
/// "no ownership evidence" (never a signal).
#[cfg(unix)]
fn scan_group_members(pgid: u32) -> Vec<(u32, u64)> {
    let Ok(dir) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    let target = pgid.to_string();
    let mut out = Vec::new();
    for entry in dir.flatten() {
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        let Ok(pid) = name.parse::<u32>() else {
            continue;
        };
        // comm is parenthesized and may contain spaces/parens; the
        // numeric fields start after the LAST `)`: state, ppid, pgrp,
        // ..., starttime (index 19).
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
            continue;
        };
        let Some(paren_end) = stat.rfind(')') else {
            continue;
        };
        let fields: Vec<&str> = stat[paren_end + 1..].split_whitespace().collect();
        let (Some(pgrp), Some(starttime)) = (fields.get(2), fields.get(19)) else {
            continue;
        };
        if *pgrp == target.as_str() && let Ok(st) = starttime.parse::<u64>() {
            out.push((pid, st));
        }
    }
    out
}

/// (unix) True when `pid` still exists and its `/proc` starttime equals
/// `starttime` — a reused pid has a different starttime, so this is a
/// valid "same process" check.
#[cfg(unix)]
fn member_still_there(pid: u32, starttime: u64) -> bool {
    let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return false;
    };
    let Some(paren_end) = stat.rfind(')') else {
        return false;
    };
    let fields: Vec<&str> = stat[paren_end + 1..].split_whitespace().collect();
    fields.get(19).and_then(|s| s.parse::<u64>().ok()) == Some(starttime)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// (a) Continuous drain: child survives after emitting >64KB on stdout
    /// (while the stderr pipe it also writes to stays drained).
    #[tokio::test]
    async fn stdout_drain_64kb_child_stays_alive() {
        let mut sup = ChildSupervisor::spawn(FakeChild::Verbose)
            .await
            .expect("spawn verbose");
        let started = Instant::now();
        while sup.stdout_total() <= 64 * 1024 && started.elapsed() < Duration::from_secs(15) {
            // `Verbose` writes to BOTH pipes: keep the stderr pipe drained
            // as well, or the child stalls once the stderr buffer fills and
            // this test could never observe >64KB on stdout.
            let _ = sup
                .drain_stderr(Duration::from_millis(250))
                .await
                .expect("drain");
            let _ = sup
                .drain_stdout(Duration::from_millis(250))
                .await
                .expect("drain");
        }
        assert!(
            sup.stdout_total() > 64 * 1024,
            "expected >64KB drained, got {} bytes (child: {})",
            sup.stdout_total(),
            String::from_utf8_lossy(sup.stdout_tail())
        );
        assert!(
            sup.is_alive().expect("is_alive"),
            "child must still be alive after >64KB stdout (drain prevented pipe deadlock)"
        );
        let _ = sup.terminate(Duration::from_millis(300)).await;
    }

    /// (a2) >64KB on STDERR: same pipe-deadlock scenario as (a) but on the
    /// stderr pipe. A supervisor that only drained stdout (or discarded
    /// stderr without draining a pipe) would deadlock this child once the
    /// stderr buffer filled.
    #[tokio::test]
    async fn stderr_drain_64kb_child_stays_alive() {
        let mut sup = ChildSupervisor::spawn(FakeChild::Verbose)
            .await
            .expect("spawn verbose");
        let started = Instant::now();
        while sup.stderr_total() <= 64 * 1024 && started.elapsed() < Duration::from_secs(15) {
            let _ = sup
                .drain_stderr(Duration::from_millis(250))
                .await
                .expect("drain");
            let _ = sup
                .drain_stdout(Duration::from_millis(250))
                .await
                .expect("drain");
        }
        assert!(
            sup.stderr_total() > 64 * 1024,
            "expected >64KB stderr drained, got {} bytes (tail: {})",
            sup.stderr_total(),
            String::from_utf8_lossy(sup.stderr_tail())
        );
        assert!(
            sup.is_alive().expect("is_alive"),
            "child must still be alive after >64KB stderr (drain prevented pipe deadlock)"
        );
        let _ = sup.terminate(Duration::from_millis(300)).await;
    }

    /// (b) TERM→KILL escalation + stdout EOF. Unix: child traps TERM and
    /// only dies on the group KILL ⇒ `Escalated`. Windows: no SIGTERM,
    /// first phase is already a force kill ⇒ `Graceful`.
    #[tokio::test]
    async fn terminate_escalates_to_kill_and_reaches_stdout_eof() {
        #[cfg(unix)]
        let kind = FakeChild::IgnoresTerm;
        #[cfg(not(unix))]
        let kind = FakeChild::Verbose;
        let mut sup = ChildSupervisor::spawn(kind).await.expect("spawn");

        // Wait for at least one flushed line so the EOF assertion is
        // meaningful (Windows powershell takes ~0.5s to start).
        let started = Instant::now();
        while sup.stdout_total() == 0 && started.elapsed() < Duration::from_secs(10) {
            let _ = sup
                .drain_stdout(Duration::from_millis(100))
                .await
                .expect("drain");
        }

        let outcome = sup
            .terminate(Duration::from_millis(300))
            .await
            .expect("terminate");
        #[cfg(unix)]
        {
            assert_eq!(
                outcome,
                TerminateOutcome::Escalated,
                "TERM-ignoring child must require the SIGKILL escalation"
            );
        }
        #[cfg(not(unix))]
        {
            assert_eq!(
                outcome,
                TerminateOutcome::Graceful,
                "TerminateProcess is immediate on Windows"
            );
        }

        // stdout EOF: every member of the group is dead ⇒ the read end
        // sees 0. (Guards against a surviving grandchild holding the pipe.)
        let total = tokio::time::timeout(Duration::from_secs(10), sup.drain_stdout_to_eof())
            .await
            .expect("stdout must reach EOF after group kill")
            .expect("drain to eof");
        assert!(total > 0, "expected at least one line on stdout before EOF");

        // stderr must reach EOF just as well — every write end in the
        // group is dead.
        tokio::time::timeout(Duration::from_secs(10), sup.drain_stderr_to_eof())
            .await
            .expect("stderr must reach EOF after group kill")
            .expect("stderr drain to eof");
    }

    /// (c) unix only: the child is a process-group leader; killing the
    /// group reaps its `sleep 100` grandchild — verified via /proc.
    #[cfg(unix)]
    #[tokio::test]
    async fn process_group_kill_reaps_grandchild() {
        let mut sup = ChildSupervisor::spawn(FakeChild::Grandchild)
            .await
            .expect("spawn");
        let pid = sup.pid();

        // process_group(0) ⇒ the child is its own group leader.
        assert_eq!(
            proc_pgrp(pid as i32),
            Some(pid as i32),
            "child must be the leader of a new process group"
        );
        // The backgrounded `sleep 100` appears in the same group.
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut group = live_pids_in_group(pid);
        while group.len() < 2 && Instant::now() < deadline {
            sleep(Duration::from_millis(50)).await;
            group = live_pids_in_group(pid);
        }
        assert!(
            group.len() >= 2,
            "expected leader + `sleep 100` grandchild in group {pid}, got {group:?}"
        );

        let _outcome = sup
            .terminate(Duration::from_millis(300))
            .await
            .expect("terminate");

        // /proc: no LIVE (non-zombie) process may remain in the group.
        // (The supervisor cannot reap the grandchild; if WSL/container
        // PID-1 reaps the orphaned zombie slowly, the group still counts
        // as clean for every purpose the supervisor has — same rule as
        // `group_has_live_member`.)
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut remaining = live_pids_in_group(pid);
        while !remaining.is_empty() && Instant::now() < deadline {
            sleep(Duration::from_millis(50)).await;
            remaining = live_pids_in_group(pid);
        }
        assert!(
            remaining.is_empty(),
            "process group {pid} still has live members after group kill: {remaining:?}"
        );

        // And stdout reaches EOF (all pipe writers in the group are dead).
        tokio::time::timeout(Duration::from_secs(10), sup.drain_stdout_to_eof())
            .await
            .expect("stdout must reach EOF after group kill")
            .expect("drain to eof");
        tokio::time::timeout(Duration::from_secs(10), sup.drain_stderr_to_eof())
            .await
            .expect("stderr must reach EOF after group kill")
            .expect("stderr drain to eof");
    }

    /// (d) `Drop` as panic safety net must leak nothing: dropping the
    /// supervisor of a still-alive child must KILL the group and reap the
    /// direct child via `waitpid` — afterwards the pid is invisible to
    /// the OS (not live and not a zombie; a zombie would still hold the
    /// PGID number and a process slot). Windows: `Drop` uses
    /// `start_kill` and the OS reaps process objects, so the same
    /// "pid gone from tasklist" check applies.
    #[tokio::test]
    async fn drop_reaps_child_and_leaks_nothing() {
        let sup = ChildSupervisor::spawn(FakeChild::Verbose)
            .await
            .expect("spawn verbose");
        let pid = sup.pid();
        // Wait until the child is actually up (tasklist/proc visible),
        // so the post-drop absence check is meaningful.
        let visible = wait_until_visible(pid, Instant::now() + Duration::from_secs(10)).await;
        assert!(visible, "child pid {pid} never became visible before drop");

        drop(sup);
        let gone = wait_until_gone(pid, Instant::now() + Duration::from_secs(5))
            .await
            .unwrap_or_else(|e| {
                eprintln!("test skipped: {e}");
                true
            });
        assert!(
            gone,
            "pid {pid} still visible (live or zombie) after Drop — the waitpid reaper leaked"
        );
    }

    /// True when `pid` is still visible to the OS: a live process OR a
    /// zombie (unix, un-reaped exit). Both count as a leak for the test
    /// above. Windows: `tasklist` only lists live processes; the OS
    /// auto-reaps process objects, so this is the right check there.
    ///
    /// `Err` = the probe itself could not run (e.g. `tasklist` missing
    /// or access-denied on a locked-down host): callers must treat this
    /// as "cannot verify" (skip), never as "process absent".
    fn os_pid_alive(pid: u32) -> Result<bool, String> {
        #[cfg(unix)]
        {
            // `/proc/<pid>/stat` exists for live AND zombie processes
            // (a zombie persists until reaped — exactly what `Drop`
            // must do).
            Ok(std::path::Path::new(&format!("/proc/{pid}/stat")).exists())
        }
        #[cfg(not(unix))]
        {
            let pid_str = pid.to_string();
            let out = std::process::Command::new("tasklist")
                .args(["/FI", &format!("PID eq {pid}")])
                .output()
                .map_err(|e| format!("tasklist failed to run: {e}"))?;
            if !out.status.success() {
                return Err(format!(
                    "tasklist exited with {}: {}",
                    out.status,
                    String::from_utf8_lossy(&out.stderr).trim()
                ));
            }
            // A match appears as a whitespace-separated column equal to
            // the pid string (byte-substring matching would confuse pid
            // 5 with 105 etc.).
            Ok(String::from_utf8_lossy(&out.stdout)
                .lines()
                .any(|l| l.split_whitespace().any(|w| w == pid_str.as_str())))
        }
    }

    /// Wait up to `deadline` for `pid` to become visible to the OS.
    /// Probe failures abort the test with a skip note instead of being
    /// misread as "child never appeared". Returns true when visibility
    /// was verified.
    async fn wait_until_visible(pid: u32, deadline: Instant) -> bool {
        loop {
            match os_pid_alive(pid) {
                Ok(true) => return true,
                Ok(false) => {}
                Err(e) => {
                    eprintln!("test skipped: cannot enumerate processes: {e}");
                    return false;
                }
            }
            if Instant::now() >= deadline {
                return false;
            }
            sleep(Duration::from_millis(50)).await;
        }
    }

    /// Wait up to `deadline` for `pid` to disappear from the OS (no live
    /// process and no zombie). Returns `Ok(true)` when disappearance was
    /// verified, `Ok(false)` when the deadline passed with the pid still
    /// visible; `Err` = probe failure (skip, do not assert).
    async fn wait_until_gone(pid: u32, deadline: Instant) -> Result<bool, String> {
        loop {
            match os_pid_alive(pid) {
                Ok(false) => return Ok(true),
                Ok(true) => {}
                Err(e) => return Err(format!("cannot verify process absence: {e}")),
            }
            if Instant::now() >= deadline {
                return Ok(false);
            }
            sleep(Duration::from_millis(50)).await;
        }
    }

    /// pgrp of `pid` from /proc/<pid>/stat (field 5; `comm` may contain
    /// spaces, so skip past the last `)`).
    #[cfg(unix)]
    fn proc_pgrp(pid: i32) -> Option<i32> {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        let after = stat.rfind(')')?;
        stat[after + 2..].split_whitespace().nth(2)?.parse().ok()
    }

    /// All visible LIVE (non-zombie) pids whose pgrp equals `pgid`.
    #[cfg(unix)]
    fn live_pids_in_group(pgid: u32) -> Vec<u32> {
        let mut out = Vec::new();
        let Ok(dir) = std::fs::read_dir("/proc") else {
            return out;
        };
        for entry in dir.flatten() {
            let Ok(name) = entry.file_name().into_string() else {
                continue;
            };
            let Ok(pid) = name.parse::<u32>() else {
                continue;
            };
            let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
                continue;
            };
            let Some(after) = stat.rfind(')') else {
                continue;
            };
            let fields: Vec<&str> = stat[after + 2..].split_whitespace().collect();
            // state(0), ppid(1), pgrp(2), ...
            let (Some(state), Some(pgrp)) = (fields.first(), fields.get(2)) else {
                continue;
            };
            if *pgrp == pgid.to_string().as_str() && *state != "Z" {
                out.push(pid);
            }
        }
        out
    }
}
