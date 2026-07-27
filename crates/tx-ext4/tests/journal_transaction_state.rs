use tx_ext4::journal::{JournalTransactionState, JournalTransactionStateError};

#[test]
fn transaction_state_rejects_checkpoint_before_durable_commit() {
    let mut state = JournalTransactionState::<()>::new();
    state.begin(()).unwrap();
    assert_eq!(
        state.take_checkpoint_ready(),
        Err(JournalTransactionStateError::NotCommitted)
    );
    state.mark_data_durable().unwrap();
    state.mark_commit_submitted().unwrap();
    state.mark_commit_durable().unwrap();
    assert_eq!(state.take_checkpoint_ready().unwrap(), Some(()));
}

#[test]
fn transaction_state_retains_transaction_until_checkpoint_completion() {
    let mut state = JournalTransactionState::new();
    state.begin(7u64).unwrap();
    state.mark_data_durable().unwrap();
    state.mark_commit_submitted().unwrap();
    state.mark_commit_durable().unwrap();

    assert_eq!(state.checkpoint_ready().unwrap(), Some(&7));
    assert_eq!(state.active(), Some(&7));
    assert_eq!(state.complete_checkpoint().unwrap(), Some(7));
    assert_eq!(state.active(), None);
}

#[test]
fn transaction_state_requires_data_durable_before_commit_submission() {
    let mut state = JournalTransactionState::new();
    state.begin(()).unwrap();
    assert_eq!(
        state.mark_commit_submitted(),
        Err(JournalTransactionStateError::DataNotDurable)
    );
    state.mark_data_durable().unwrap();
    state.mark_commit_submitted().unwrap();
    state.mark_commit_durable().unwrap();
}
