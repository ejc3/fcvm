#!/bin/sh
# cargo/nextest target runner for the privileged (root) test and bench suites.
#
# `make test-root` runs cargo-nextest AS THE INVOKING USER and elevates only the test
# binary, via CARGO_TARGET_<TRIPLE>_RUNNER='sudo -E env PATH=... <this script>'. That
# sudo hop is a one-way door for process teardown, and the whole VM tree hangs off it:
#
#   cargo-nextest (uid 1000) -> sudo (ruid 1000) -> test binary (uid 0)
#                                                     -> fcvm -> firecracker
#
#   * The kernel refuses a signal from uid 1000 to a uid-0 process, so nextest's
#     slow-timeout kill cannot touch the elevated test binary. It kills the test's
#     process group, and kill(2) still returns success — it signalled `sudo`, which is
#     enough for the call to report 0 — so the failure is completely silent.
#   * SIGKILL cannot be caught, so `sudo` never gets a chance to forward it.
#
#   Result: the test binary is orphaned to init and keeps running. Because it never
#   dies, the PR_SET_PDEATHSIG chain BELOW it (test binary -> fcvm -> firecracker) never
#   fires either, so every microVM the test had booted survives until the box reboots.
#   Measured: `killpg(SIGKILL)` from uid 1000 left 2/2 root processes alive, reparented
#   to init. That is how a self-hosted runner accumulated ~490 live `firecracker`
#   processes and wedged for 21 hours.
#
# `setpriv --pdeathsig KILL` re-establishes exactly the link the privilege boundary
# broke: the kernel SIGKILLs this process the moment `sudo` dies. It is kernel-enforced,
# so it holds in the case that leaked here — a SIGKILL where no userspace cleanup,
# Drop impl or signal handler gets to run.

# Host test recipes provide a unique absolute XDG_CONFIG_HOME containing the
# current worktree's embedded config. sudo also sets SUDO_USER, and fcvm normally
# gives that user's ~/.config precedence over XDG; remove only that lookup hint
# for hermetic test runs so another worktree cannot replace the config between
# setup and VM launch. Normal invocations without XDG_CONFIG_HOME are unchanged.
if [ -n "${XDG_CONFIG_HOME:-}" ]; then
	unset SUDO_USER
fi

exec setpriv --pdeathsig KILL "$@"
