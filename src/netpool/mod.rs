//! A pool of rootless network namespaces that are recycled between VMs.
//!
//! Building a rootless VM's namespace (its holder, a TAP, a bridge, pasta) and destroying
//! it holds the kernel's `rtnl_mutex` for milliseconds, and the mutex is one for the whole
//! host. `fcvm netpool serve` builds a number of namespaces ahead of time and lends each to
//! one VM at a time, so a clone's start takes a namespace that exists and its stop gives it
//! back.
//!
//! # Ownership
//!
//! One pool process per data directory and user ([`proto::dir`]): it holds `pool.lock` for
//! its life and listens on `pool.sock`. Every holder and every pasta is the pool's child,
//! armed with `PR_SET_PDEATHSIG`, so nothing of a pool outlives it. A clone never starts a
//! pool: that would be a process with no parent to die with.
//!
//! # The lease
//!
//! A claim is one connection to `pool.sock` ([`client::claim`]). The pool answers it with a
//! free namespace: descriptors of its user and network namespaces, a channel to its holder
//! for sockets in it, and the TAP's name. The connection is the lease. Nothing is written
//! down, so nothing can be left behind: the kernel closes the connection when the last
//! process that holds it exits.
//!
//! The clone alone holding the lease would not be enough. A killed clone's descriptors
//! close before its VMM has been told to die, and the pool would lend the namespace while
//! that VMM still had its TAP. So the VMM inherits the lease across its exec
//! ([`crate::utils::NamespaceParams::keep_fds`]), and the connection reads end-of-file only
//! when the clone and its VMM are both gone.
//!
//! # Reset
//!
//! At the end of a lease the pool has the namespace's pasta forget its guest (a reset over
//! pasta's control socket, `crate::network::pesto`), has the holder remove
//! what a VM's use leaves ([`crate::network::netns_agent::reset`]) and compares the
//! namespace's [fingerprint](crate::network::fingerprint) with the one taken when it was
//! built. A namespace that differs is destroyed and built again. Its generation counts the
//! leases it has had; a namespace of generation 2 or more has been recycled.
//!
//! # Destroying
//!
//! The kernel destroys a namespace in a worker after its last process has exited, a batch
//! of dead namespaces under one `rtnl_mutex` hold, and on kernels before Linux
//! 25ae123db10b each bridge in the batch costs an `rcu_barrier()` under it: 14 ms a
//! namespace on a 192-CPU host, measured (`bench/start-rate/pieces/RESULTS.md`). A thousand
//! at once stop every network change on the host for 15 s. The pool therefore destroys one
//! namespace at a time, [`server::DESTROY_SPACING`] apart.
//!
//! # What is not pooled
//!
//! A clone with a published port, an isolated namespace, a cold boot, and any clone when
//! no pool runs or no namespace is free take the path they always took, through the same
//! builder. Published ports wait for pasta to report whether it bound a forward added
//! through its control socket; until then a pooled pasta has no forwards.

pub mod client;
pub mod proto;
pub mod server;
pub mod slots;
