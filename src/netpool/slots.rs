//! The pool's bookkeeping: which namespace is free and which is lent, and to which lease.
//!
//! Pure state. No process, socket or namespace is touched here, so the rules can be tested
//! exhaustively: a namespace has at most one lease, it is lent only when it is ready, and
//! every namespace is in exactly one state at any time, whatever order the events come in.

use std::collections::VecDeque;

pub type SlotId = u32;
pub type LeaseId = u64;

/// Where one namespace is in its life.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// Being built: the first build, or the replacement of a destroyed namespace.
    Building,
    /// Built or reset, verified, and free.
    Ready,
    /// Lent. The lease is one connection, and this is its number.
    Leased(LeaseId),
    /// Its lease ended; being reset and verified.
    Resetting,
    /// Being destroyed: its verification failed, or one of its processes died.
    Destroying,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Counts {
    pub building: usize,
    pub ready: usize,
    pub leased: usize,
    pub resetting: usize,
    pub destroying: usize,
}

impl Counts {
    pub fn total(&self) -> usize {
        self.building + self.ready + self.leased + self.resetting + self.destroying
    }
}

/// A transition the table refused: the event does not fit the namespace's state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refused {
    pub slot: SlotId,
    pub event: &'static str,
    pub state: State,
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "namespace {} cannot be {} while it is {:?}",
            self.slot, self.event, self.state
        )
    }
}

impl std::error::Error for Refused {}

struct Slot {
    state: State,
    /// Leases this namespace has had since it was built.
    generation: u64,
}

/// What a claim got.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Claimed {
    pub slot: SlotId,
    pub lease: LeaseId,
    /// The namespace's generation with this lease counted: 1 is its first lease.
    pub generation: u64,
}

pub struct Table {
    slots: Vec<Slot>,
    /// Ready namespaces, the one ready longest first.
    ready: VecDeque<SlotId>,
    next_lease: LeaseId,
}

impl Table {
    /// `slots` namespaces, all being built.
    pub fn new(slots: usize) -> Self {
        Self {
            slots: (0..slots)
                .map(|_| Slot {
                    state: State::Building,
                    generation: 0,
                })
                .collect(),
            ready: VecDeque::new(),
            next_lease: 1,
        }
    }

    fn step(
        &mut self,
        slot: SlotId,
        event: &'static str,
        from: impl Fn(State) -> bool,
        to: State,
    ) -> Result<(), Refused> {
        let state = self.slots[slot as usize].state;
        if !from(state) {
            return Err(Refused { slot, event, state });
        }
        self.slots[slot as usize].state = to;
        Ok(())
    }

    /// The build of `slot` finished and its fingerprint is taken.
    pub fn built(&mut self, slot: SlotId) -> Result<(), Refused> {
        self.step(slot, "built", |s| s == State::Building, State::Ready)?;
        self.slots[slot as usize].generation = 0;
        self.ready.push_back(slot);
        Ok(())
    }

    /// Lend the namespace that has been ready longest, if one is.
    pub fn claim(&mut self) -> Option<Claimed> {
        let slot = self.ready.pop_front()?;
        let lease = self.next_lease;
        self.next_lease += 1;
        let entry = &mut self.slots[slot as usize];
        debug_assert_eq!(entry.state, State::Ready);
        entry.state = State::Leased(lease);
        entry.generation += 1;
        Some(Claimed {
            slot,
            lease,
            generation: entry.generation,
        })
    }

    /// `lease` on `slot` ended. Refused for any other lease: an old lease's end must not
    /// take the namespace from the lease that has it now.
    pub fn released(&mut self, slot: SlotId, lease: LeaseId) -> Result<(), Refused> {
        self.step(
            slot,
            "released",
            |s| s == State::Leased(lease),
            State::Resetting,
        )
    }

    /// The reset of `slot` finished and its fingerprint matched.
    pub fn reset_done(&mut self, slot: SlotId) -> Result<(), Refused> {
        self.step(slot, "reset", |s| s == State::Resetting, State::Ready)?;
        self.ready.push_back(slot);
        Ok(())
    }

    /// `slot` must not be lent again: its verification failed or one of its processes died.
    /// From any state but `Destroying`; a lease it had is over.
    pub fn condemn(&mut self, slot: SlotId) -> Result<(), Refused> {
        self.step(
            slot,
            "condemned",
            |s| s != State::Destroying,
            State::Destroying,
        )?;
        self.ready.retain(|ready| *ready != slot);
        Ok(())
    }

    /// The processes of `slot` are gone; its replacement is being built.
    pub fn destroyed(&mut self, slot: SlotId) -> Result<(), Refused> {
        self.step(
            slot,
            "destroyed",
            |s| s == State::Destroying,
            State::Building,
        )
    }

    pub fn state(&self, slot: SlotId) -> State {
        self.slots[slot as usize].state
    }

    pub fn counts(&self) -> Counts {
        let mut counts = Counts::default();
        for slot in &self.slots {
            match slot.state {
                State::Building => counts.building += 1,
                State::Ready => counts.ready += 1,
                State::Leased(_) => counts.leased += 1,
                State::Resetting => counts.resetting += 1,
                State::Destroying => counts.destroying += 1,
            }
        }
        counts
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ready_table(slots: usize) -> Table {
        let mut table = Table::new(slots);
        for slot in 0..slots as SlotId {
            table.built(slot).unwrap();
        }
        table
    }

    #[test]
    fn a_namespace_is_lent_to_one_lease_at_a_time() {
        let mut table = ready_table(2);
        let first = table.claim().unwrap();
        let second = table.claim().unwrap();
        assert_ne!(first.slot, second.slot);
        assert_ne!(first.lease, second.lease);
        assert_eq!(table.claim(), None, "both namespaces are lent");
        assert_eq!(table.state(first.slot), State::Leased(first.lease));
    }

    #[test]
    fn a_namespace_is_lent_again_only_after_its_reset() {
        let mut table = ready_table(1);
        let first = table.claim().unwrap();
        assert_eq!(first.generation, 1);
        table.released(first.slot, first.lease).unwrap();
        assert_eq!(table.claim(), None, "a namespace being reset is not free");
        table.reset_done(first.slot).unwrap();
        let second = table.claim().unwrap();
        assert_eq!((second.slot, second.generation), (first.slot, 2));
    }

    /// The end of a lease the namespace no longer has changes nothing: its connection can be
    /// seen closing after the namespace was condemned, destroyed, rebuilt and lent again.
    #[test]
    fn the_end_of_an_old_lease_does_not_touch_the_current_one() {
        let mut table = ready_table(1);
        let old = table.claim().unwrap();
        table.condemn(old.slot).unwrap();
        table.destroyed(old.slot).unwrap();
        table.built(old.slot).unwrap();
        let current = table.claim().unwrap();
        assert_eq!(current.generation, 1, "a rebuilt namespace starts over");
        let refused = table.released(old.slot, old.lease).unwrap_err();
        assert_eq!(refused.state, State::Leased(current.lease));
        assert_eq!(table.state(current.slot), State::Leased(current.lease));
    }

    #[test]
    fn a_condemned_namespace_is_never_lent() {
        let mut table = ready_table(2);
        table.condemn(0).unwrap();
        assert_eq!(table.claim().unwrap().slot, 1);
        assert_eq!(table.claim(), None);
        assert!(table.condemn(0).is_err(), "it is already being destroyed");
        assert!(table.reset_done(0).is_err());
        assert!(table.built(0).is_err());
    }

    /// Every sequence of events, valid or refused, leaves each namespace in exactly one
    /// state, no namespace both free and lent, and no lease on two namespaces.
    #[test]
    fn counts_are_conserved_under_any_order_of_events() {
        const SLOTS: usize = 5;
        let mut table = Table::new(SLOTS);
        let mut leases: Vec<Option<LeaseId>> = vec![None; SLOTS];
        // A fixed-seed linear congruential generator: the run is the same every time.
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        let mut next = |bound: u64| {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (seed >> 33) % bound
        };
        for _ in 0..20_000 {
            let slot = next(SLOTS as u64) as SlotId;
            match next(6) {
                0 => {
                    let _ = table.built(slot);
                }
                1 => {
                    if let Some(claimed) = table.claim() {
                        assert_eq!(leases[claimed.slot as usize], None, "lent twice");
                        leases[claimed.slot as usize] = Some(claimed.lease);
                    }
                }
                2 => {
                    // The end of this slot's lease, or of a lease it never had.
                    let lease = leases[slot as usize].unwrap_or(u64::MAX);
                    if table.released(slot, lease).is_ok() {
                        leases[slot as usize] = None;
                    }
                }
                3 => {
                    let _ = table.reset_done(slot);
                }
                4 => {
                    if table.condemn(slot).is_ok() {
                        leases[slot as usize] = None;
                    }
                }
                _ => {
                    let _ = table.destroyed(slot);
                }
            }
            let counts = table.counts();
            assert_eq!(counts.total(), SLOTS);
            assert_eq!(counts.ready, table.ready.len());
            assert_eq!(counts.leased, leases.iter().flatten().count());
            for (slot, lease) in leases.iter().enumerate() {
                match (table.state(slot as SlotId), lease) {
                    (State::Leased(have), Some(want)) => assert_eq!(have, *want),
                    (State::Leased(_), None) | (_, Some(_)) => panic!("lease out of step"),
                    _ => {}
                }
            }
            let mut ready: Vec<SlotId> = table.ready.iter().copied().collect();
            ready.sort_unstable();
            ready.dedup();
            assert_eq!(ready.len(), counts.ready, "a namespace is queued twice");
            for slot in ready {
                assert_eq!(table.state(slot), State::Ready);
            }
        }
    }
}
