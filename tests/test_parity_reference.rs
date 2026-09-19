//! The differential exec test's handling of a reference that fails on its own
//! (issue #944), driven by a scripted reference: no podman and no VM.

mod parity_reference;

use parity_reference::{settle, Settled};
use std::cell::RefCell;

/// What host podman said in #944, twice in a row, about an image that has the user.
const LOOKUP_FAILURE: &str =
    "Error: unable to find user nobody: no matching entries in passwd file\n";
/// What the same case prints when the lookup works, under podman and under fcvm.
const RIGHT_ANSWER: &str = "65534\n65534\n65534\n";
/// A failure that is the case's answer: both tools fail this way on a missing command.
const NOT_FOUND: &str = "Error: crun: executable file `no-such-command` not found in $PATH\n";
const EVIDENCE: &str = "passwd: 28 lines, nobody present. status: running";

/// A reference that gives scripted answers in order and records what was done to it.
struct Scripted {
    answers: RefCell<Vec<&'static str>>,
    restart_fails: bool,
    events: RefCell<Vec<&'static str>>,
    notes: RefCell<Vec<String>>,
}

impl Scripted {
    fn answering(answers: &[&'static str]) -> Self {
        Scripted {
            answers: RefCell::new(answers.to_vec()),
            restart_fails: false,
            events: RefCell::new(Vec::new()),
            notes: RefCell::new(Vec::new()),
        }
    }

    /// Settle one case whose target answered `target`.
    fn settle_against(&self, target: &'static str) -> Result<Settled, String> {
        settle(
            || {
                self.events.borrow_mut().push("ask");
                let mut answers = self.answers.borrow_mut();
                assert!(
                    !answers.is_empty(),
                    "asked more often than the script allows"
                );
                Ok(answers.remove(0))
            },
            |answer: &&'static str| answer.starts_with("Error:").then(|| format!("{answer:?}")),
            |answer: &&'static str| {
                self.events.borrow_mut().push("compare");
                Ok(if *answer == target {
                    Vec::new()
                } else {
                    vec![format!("stdout: podman {answer:?} fcvm {target:?}")]
                })
            },
            || {
                self.events.borrow_mut().push("evidence");
                EVIDENCE.to_string()
            },
            || {
                self.events.borrow_mut().push("restart");
                if self.restart_fails {
                    Err("no fresh container".to_string())
                } else {
                    Ok(())
                }
            },
            |line| self.notes.borrow_mut().push(line),
        )
    }

    fn events(&self) -> Vec<&'static str> {
        self.events.borrow().clone()
    }

    fn notes(&self) -> String {
        self.notes.borrow().join("\n")
    }
}

/// Issue #944 itself: the lookup fails twice in one container and a fresh one answers.
#[test]
fn a_failure_a_fresh_reference_does_not_repeat_is_not_a_difference() {
    let reference = Scripted::answering(&[LOOKUP_FAILURE, LOOKUP_FAILURE, RIGHT_ANSWER]);
    let settled = reference.settle_against(RIGHT_ANSWER).unwrap();
    assert_eq!(settled.differences, Vec::<String>::new());
    assert_eq!(settled.reference_failure, None);
    assert_eq!(
        reference.events(),
        ["ask", "evidence", "ask", "compare", "evidence", "restart", "ask", "compare"],
        "the evidence has to come from the container that failed, before it is replaced"
    );
    assert!(
        reference.notes().contains(EVIDENCE),
        "a case that recovers still has to show what the failing container looked like:\n{}",
        reference.notes()
    );
}

#[test]
fn a_failure_a_fresh_reference_repeats_fails_and_carries_the_evidence() {
    let reference = Scripted::answering(&[LOOKUP_FAILURE, LOOKUP_FAILURE, LOOKUP_FAILURE]);
    let settled = reference.settle_against(RIGHT_ANSWER).unwrap();
    assert_eq!(settled.differences.len(), 1, "{:?}", settled.differences);
    let carried = settled
        .reference_failure
        .expect("a failure two containers share has to carry its evidence");
    assert_eq!(
        carried.matches(EVIDENCE).count(),
        2,
        "what the container looked like after the first failure and after the second:\n{carried}"
    );
    assert_eq!(
        reference.events(),
        ["ask", "evidence", "ask", "compare", "evidence", "restart", "ask", "compare"]
    );
}

/// The failure the second ask was written for: there once, gone when asked again.
#[test]
fn a_failure_that_is_gone_on_the_second_ask_shows_its_evidence_and_needs_no_restart() {
    let reference = Scripted::answering(&[LOOKUP_FAILURE, RIGHT_ANSWER]);
    let settled = reference.settle_against(RIGHT_ANSWER).unwrap();
    assert_eq!(settled.differences, Vec::<String>::new());
    assert_eq!(settled.reference_failure, None);
    assert_eq!(
        reference.events(),
        ["ask", "evidence", "ask", "compare"],
        "the evidence is collected before the second ask, while there may still be something to see"
    );
    assert!(
        reference.notes().contains(EVIDENCE),
        "a failure that does not repeat has to leave its evidence behind:\n{}",
        reference.notes()
    );
}

/// Four cases fail this way on every run: a missing command, a file that is not
/// executable, a missing working directory. fcvm fails the same way, so the
/// failure is the answer, the reference container is left alone, and the
/// evidence is dropped.
#[test]
fn a_failure_the_target_shares_is_the_answer_and_needs_no_restart() {
    let reference = Scripted::answering(&[NOT_FOUND, NOT_FOUND]);
    let settled = reference.settle_against(NOT_FOUND).unwrap();
    assert_eq!(settled.differences, Vec::<String>::new());
    assert_eq!(settled.reference_failure, None);
    assert_eq!(reference.events(), ["ask", "evidence", "ask", "compare"]);
    assert!(
        !reference.notes().contains(EVIDENCE),
        "an answer both tools give is no anomaly, and its evidence would be noise:\n{}",
        reference.notes()
    );
}

#[test]
fn an_answer_that_is_not_a_failure_is_asked_for_once() {
    let reference = Scripted::answering(&[RIGHT_ANSWER]);
    let settled = reference.settle_against(RIGHT_ANSWER).unwrap();
    assert_eq!(settled.differences, Vec::<String>::new());
    assert_eq!(reference.events(), ["ask", "compare"]);
    assert_eq!(reference.notes(), "");
}

/// The reference recovering does not excuse the target: its own difference stands.
#[test]
fn a_target_difference_survives_a_fresh_reference() {
    let reference = Scripted::answering(&[LOOKUP_FAILURE, LOOKUP_FAILURE, RIGHT_ANSWER]);
    let settled = reference.settle_against("0\n0\n0\n").unwrap();
    assert_eq!(settled.differences.len(), 1, "{:?}", settled.differences);
    assert_eq!(
        settled.reference_failure, None,
        "the fresh reference answered, so this difference is the target's"
    );
    assert_eq!(
        reference.events(),
        ["ask", "evidence", "ask", "compare", "evidence", "restart", "ask", "compare"]
    );
}

/// Comparing with the answer of a container that could not be replaced would
/// report the reference's failure as fcvm's. The case stops, and the evidence
/// is already out.
#[test]
fn a_restart_that_fails_stops_the_case_after_the_evidence_is_out() {
    let mut reference = Scripted::answering(&[LOOKUP_FAILURE, LOOKUP_FAILURE]);
    reference.restart_fails = true;
    let error = reference
        .settle_against(RIGHT_ANSWER)
        .err()
        .expect("a reference that cannot be replaced has no answer to compare with");
    assert_eq!(error, "no fresh container");
    assert_eq!(
        reference.events(),
        ["ask", "evidence", "ask", "compare", "evidence", "restart"]
    );
    assert!(
        reference.notes().contains(EVIDENCE),
        "{}",
        reference.notes()
    );
}
