use tx_ext4::journal::{JournalTransactionState, JournalTransactionStateError};

#[test]
fn transaction_state_rejects_checkpoint_before_durable_commit() {
    let mut state = JournalTransactionState::<()>::new();
    state.begin(()).unwrap();
    assert_eq!(
        state.take_checkpoint_ready(),
        Err(JournalTransactionStateError::NotCommitted)
    );
    state.mark_commit_durable().unwrap();
    assert_eq!(state.take_checkpoint_ready().unwrap(), Some(()));
}
