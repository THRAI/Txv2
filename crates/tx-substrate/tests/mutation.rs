use tx_substrate::epoch;
use tx_substrate::index::Index;
use tx_substrate::mutation::{self, MutationError};

#[derive(Debug, Eq, PartialEq)]
struct NonCloneValue {
    number: u32,
}

#[test]
fn install_if_absent_commits_only_when_key_is_free() {
    let index = Index::<u32, NonCloneValue, 2>::new();

    mutation::install_if_absent(&index, 1, NonCloneValue { number: 10 }).expect("initial install");

    let err = mutation::install_if_absent(&index, 1, NonCloneValue { number: 20 })
        .expect_err("duplicate install fails");
    assert_eq!(err, MutationError::AlreadyPresent);

    let guard = epoch::guard();
    let observed = index.lookup(&1, &guard).expect("original value remains");
    assert_eq!(observed.value().number, 10);
}

#[test]
fn withdraw_and_swap_move_values_without_clone_bounds() {
    let index = Index::<u32, NonCloneValue, 2>::new();
    mutation::install_if_absent(&index, 2, NonCloneValue { number: 30 }).expect("install");

    let old = mutation::swap(&index, &2, NonCloneValue { number: 31 }).expect("swap");
    assert_eq!(old, NonCloneValue { number: 30 });

    let removed = mutation::withdraw(&index, &2).expect("withdraw");
    assert_eq!(removed, NonCloneValue { number: 31 });

    let guard = epoch::guard();
    assert!(index.lookup(&2, &guard).is_none());
    assert_eq!(
        mutation::withdraw::<_, _, 2>(&index, &2),
        Err(MutationError::Missing)
    );
}
