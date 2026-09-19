//! How the differential exec test settles what the reference says for one case.
//!
//! The reference is host podman, and host podman can fail on its own. In issue
//! #944 `podman exec -u nobody` answered `Error: unable to find user nobody`
//! twice in a row, inside one reference container, for an image whose passwd
//! file has that user. The next run's fresh container answered correctly.
//!
//! The decision lives here, apart from the test that drives real containers, so
//! `tests/test_parity_reference.rs` can exercise it with no podman and no VM.

/// What one case came to.
pub struct Settled {
    /// Mismatches against the reference answer that stood in the end.
    pub differences: Vec<String>,
    /// Set when a fresh reference container repeated the reference's own
    /// failure and the target still disagrees with it. Holds the evidence
    /// collected from the first container before it was replaced.
    pub reference_failure: Option<String>,
}

/// Ask the reference for one case and compare the target with its answer.
///
/// `own_failure` describes an answer that is the reference tool's own error, as
/// opposed to the command's, and returns `None` for every other answer.
///
/// Such an answer is asked for again, because one that was transient is gone on
/// the second ask. `collect_evidence` runs before that second ask: a failure
/// that does not repeat leaves nothing to look at later, and when it is gone the
/// evidence is what `note` gets. One that repeats is still the right answer when the target
/// agrees with it. A command that does not exist fails the same way under both
/// tools, `compare` finding nothing says so, and that evidence is dropped.
///
/// One that repeats while the target disagrees is the fault of the reference
/// container or of the reference tool. The evidence is collected again and goes
/// to `note` before anything else can fail, while the container that failed
/// still exists. `restart` then replaces the container and the case is asked
/// once more. Whatever the fresh container says is compared as it stands. If it
/// is the same failure, the evidence travels with the differences.
pub fn settle<O, E>(
    mut ask: impl FnMut() -> Result<O, E>,
    own_failure: impl Fn(&O) -> Option<String>,
    mut compare: impl FnMut(&O) -> Result<Vec<String>, E>,
    mut collect_evidence: impl FnMut() -> String,
    mut restart: impl FnMut() -> Result<(), E>,
    mut note: impl FnMut(String),
) -> Result<Settled, E> {
    let first = ask()?;
    let Some(failure) = own_failure(&first) else {
        return Ok(Settled {
            differences: compare(&first)?,
            reference_failure: None,
        });
    };
    let after_first = collect_evidence();
    note(format!("podman reported {failure}; asking again"));
    let second = ask()?;
    let Some(failure) = own_failure(&second) else {
        note(format!(
            "the failure did not repeat. The reference container right after it:\n{after_first}"
        ));
        return Ok(Settled {
            differences: compare(&second)?,
            reference_failure: None,
        });
    };
    let differences = compare(&second)?;
    if differences.is_empty() {
        return Ok(Settled {
            differences,
            reference_failure: None,
        });
    }

    let evidence = format!(
        "after the first failure:\n{after_first}after the second:\n{}",
        collect_evidence()
    );
    note(format!(
        "podman reported {failure} again and the target disagrees. The reference container:\n{evidence}"
    ));
    note("replacing the reference container and asking once more".to_string());
    restart()?;
    let fresh = ask()?;
    let differences = compare(&fresh)?;
    let reference_failure = match own_failure(&fresh) {
        Some(failure) => {
            note(format!(
                "a fresh reference container reported {failure} too: the reference tool fails here, not one container"
            ));
            (!differences.is_empty()).then_some(evidence)
        }
        None => {
            note("the fresh reference container answered: the failure belonged to the container that was replaced".to_string());
            None
        }
    };
    Ok(Settled {
        differences,
        reference_failure,
    })
}
