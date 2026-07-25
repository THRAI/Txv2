use super::*;
use crate::cred::{CapabilitySet, Cred, Gid, Uid};
use crate::execution::Errno;
use crate::ipc::sysv_shm::execution::{IPC_CREAT, IPC_EXCL};
use crate::process::adapter::step_engine::{NoProgress, StepOutcome, WaitSourceId, YieldShape};
use crate::process::adapter::wait_routing::{MailboxEvent, TaskMailbox};
use crate::test_support::EPOCH_TEST_LOCK;
use crate::zones;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicUsize, Ordering};
use tx_substrate::step::InterestMask;

static SYSV_MSG_REF_POST_COUNT: AtomicUsize = AtomicUsize::new(0);

fn counting_msg_ref_post(mailbox: &TaskMailbox, event: MailboxEvent) -> bool {
    SYSV_MSG_REF_POST_COUNT.fetch_add(1, Ordering::AcqRel);
    mailbox.post(event)
}

fn direct_msg_ref_post(mailbox: &TaskMailbox, event: MailboxEvent) -> bool {
    mailbox.post(event)
}

fn setup() -> std::sync::MutexGuard<'static, ()> {
    let guard = EPOCH_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    tx_test_support::init_host();
    let _ = zones::register_all();
    tx_test_support::drain_to_quiescence();
    guard
}

fn cred(uid: u32, gid: u32) -> crate::process::adapter::step_engine::Cap<Cred> {
    crate::cred::sign_cred(Cred {
        uid: Uid(uid),
        euid: Uid(uid),
        suid: Uid(uid),
        gid: Gid(gid),
        egid: Gid(gid),
        sgid: Gid(gid),
        effective_caps: CapabilitySet::EMPTY,
        permitted_caps: CapabilitySet::EMPTY,
    })
    .expect("cred cap")
}

#[test]
fn msg_payload_endpoints_match_wait_source_ids() {
    let _g = setup();

    let owner = cred(1000, 1000);
    let ns = crate::process::nsproxy::sign_init_nsproxy().expect("nsproxy cap");
    let msqid = execution::step_msgget(IPC_EXCL, IPC_CREAT | 0o600, &owner, &ns).expect("msgget");
    let queue = structure::lookup_msg(msqid).expect("queue registered");
    let payload = queue
        .payload
        .lock()
        .as_ref()
        .cloned()
        .expect("live msg payload");

    assert_eq!(
        tx_substrate::wake::WaitEndpoint::source_id(payload.send_endpoint()).raw(),
        payload.send_source_id
    );
    assert_eq!(
        tx_substrate::wake::WaitEndpoint::source_id(payload.recv_endpoint()).raw(),
        payload.recv_source_id
    );

    execution::step_msgctl_in_ns_with_post(
        msqid,
        execution::IPC_RMID,
        None,
        &owner,
        &ns,
        direct_msg_ref_post,
    )
    .expect("cleanup");
}

#[test]
fn msgsnd_with_post_uses_injected_mailbox_ref_post_for_receiver_wake() {
    let _g = setup();
    SYSV_MSG_REF_POST_COUNT.store(0, Ordering::Release);

    let owner = cred(1000, 1000);
    let ns = crate::process::nsproxy::sign_init_nsproxy().expect("nsproxy cap");
    let msqid = execution::step_msgget(IPC_EXCL, IPC_CREAT | 0o600, &owner, &ns).expect("msgget");

    let wait_source_id =
        match execution::step_msgrcv_v3_with_post(msqid, 8, 0, 0, &owner, direct_msg_ref_post) {
            StepOutcome::Yield {
                progress,
                shape: YieldShape::OnWaitSource { source, interests },
            } => {
                assert_eq!(progress, NoProgress);
                assert_eq!(interests.raw(), 1);
                source.raw()
            }
            other => panic!("expected blocking msgrcv to yield, got {other:?}"),
        };
    let source = tx_substrate::wake::lookup_source(WaitSourceId::new(wait_source_id))
        .expect("msg recv wait source should be registered");
    let mailbox = Arc::new(TaskMailbox::new());
    let generation = mailbox.next_generation();
    let subscriber = source.register(Arc::downgrade(&mailbox), generation, InterestMask::new(1));

    let sent = execution::step_msgsnd_with_post(
        msqid,
        1,
        alloc::vec![b'w'; 4],
        0,
        &owner,
        counting_msg_ref_post,
    )
    .expect("msgsnd");

    assert_eq!(sent, 4);
    assert_eq!(SYSV_MSG_REF_POST_COUNT.load(Ordering::Acquire), 1);
    assert!(
        matches!(
            mailbox.poll(),
            Some(MailboxEvent::SourceFired {
                generation: fired,
                ..
            }) if fired == generation
        ),
        "msgsnd should wake recv waiters through injected post"
    );
    source.unregister(subscriber);
    execution::step_msgctl_in_ns_with_post(
        msqid,
        execution::IPC_RMID,
        None,
        &owner,
        &ns,
        direct_msg_ref_post,
    )
    .expect("cleanup");
}

#[test]
fn msgrcv_with_post_uses_injected_mailbox_ref_post_for_sender_wake() {
    let _g = setup();
    SYSV_MSG_REF_POST_COUNT.store(0, Ordering::Release);

    let owner = cred(1000, 1000);
    let ns = crate::process::nsproxy::sign_init_nsproxy().expect("nsproxy cap");
    let msqid = execution::step_msgget(IPC_EXCL, IPC_CREAT | 0o600, &owner, &ns).expect("msgget");
    let qbytes = match execution::step_msgctl_with_post(
        msqid,
        execution::IPC_STAT,
        None,
        &owner,
        direct_msg_ref_post,
    )
    .expect("IPC_STAT")
    {
        execution::MsgCtlResult::Stat(info) => info.qbytes,
        other => panic!("expected Stat, got {other:?}"),
    };
    let max_msg_size = match execution::step_msgctl_with_post(
        msqid,
        execution::MSG_INFO,
        None,
        &owner,
        direct_msg_ref_post,
    )
    .expect("MSG_INFO")
    {
        execution::MsgCtlResult::Info { msgmax, .. } => msgmax as usize,
        other => panic!("expected Info, got {other:?}"),
    };

    let mut remaining = qbytes;
    while remaining > 0 {
        let chunk = remaining.min(max_msg_size);
        execution::step_msgsnd_with_post(
            msqid,
            1,
            alloc::vec![b'x'; chunk],
            0,
            &owner,
            direct_msg_ref_post,
        )
        .expect("fill queue");
        remaining -= chunk;
    }

    let wait_source_id = match execution::step_msgsnd_v3_with_post(
        msqid,
        1,
        alloc::vec![b'y'],
        0,
        &owner,
        direct_msg_ref_post,
    ) {
        StepOutcome::Yield {
            progress,
            shape: YieldShape::OnWaitSource { source, interests },
        } => {
            assert_eq!(progress.bytes(), 0);
            assert_eq!(interests.raw(), 1);
            source.raw()
        }
        other => panic!("expected blocking msgsnd to yield, got {other:?}"),
    };
    let source = tx_substrate::wake::lookup_source(WaitSourceId::new(wait_source_id))
        .expect("msg send wait source should be registered");
    let mailbox = Arc::new(TaskMailbox::new());
    let generation = mailbox.next_generation();
    let subscriber = source.register(Arc::downgrade(&mailbox), generation, InterestMask::new(1));

    let (mtype, mtext) =
        execution::step_msgrcv_with_post(msqid, max_msg_size, 0, 0, &owner, counting_msg_ref_post)
            .expect("msgrcv");

    assert_eq!(mtype, 1);
    assert_eq!(mtext.len(), max_msg_size);
    assert_eq!(SYSV_MSG_REF_POST_COUNT.load(Ordering::Acquire), 1);
    assert!(
        matches!(
            mailbox.poll(),
            Some(MailboxEvent::SourceFired {
                generation: fired,
                ..
            }) if fired == generation
        ),
        "msgrcv should wake send waiters through injected post"
    );
    source.unregister(subscriber);
    execution::step_msgctl_in_ns_with_post(
        msqid,
        execution::IPC_RMID,
        None,
        &owner,
        &ns,
        direct_msg_ref_post,
    )
    .expect("cleanup");
}

#[test]
fn msg_namespace_entry_is_identity_cap_authority() {
    let _g = setup();

    let owner = cred(1000, 1000);
    let ns = crate::process::nsproxy::sign_init_nsproxy().expect("nsproxy cap");
    let key = 0x4d534701;
    let msqid = execution::step_msgget(key, IPC_CREAT | IPC_EXCL | 0o600, &owner, &ns)
        .expect("msgget keyed");

    let namespace_queue = ns
        .ipc_ns
        .sysv_msg
        .lock()
        .get(&crate::process::nsproxy::SysvKey::new(key as u32))
        .expect("namespace entry")
        .clone();

    assert_eq!(namespace_queue.msqid, msqid);
    assert_eq!(
        namespace_queue.key().raw(),
        structure::lookup_msg(msqid)
            .expect("global compatibility registry")
            .key()
            .raw(),
        "IpcNamespace.sysv_msg must be the authority for the msg identity cap"
    );

    execution::step_msgctl_in_ns_with_post(
        msqid,
        execution::IPC_RMID,
        None,
        &owner,
        &ns,
        direct_msg_ref_post,
    )
    .expect("cleanup");
}

#[test]
fn msg_queue_rmid_releases_legacy_and_wake_sources() {
    let _g = setup();

    let legacy_before = crate::wait_source::registry_summary().total;
    let wake_before = tx_substrate::wake::registry_summary().live;

    {
        let owner = cred(1000, 1000);
        let ns = crate::process::nsproxy::sign_init_nsproxy().expect("nsproxy cap");
        let key = 0x4d534702;
        let msqid = execution::step_msgget(key, IPC_CREAT | IPC_EXCL | 0o600, &owner, &ns)
            .expect("msgget keyed");

        assert_eq!(
            crate::wait_source::registry_summary().total,
            legacy_before + 2,
            "msg queue creation registers send and recv compatibility wait sources"
        );
        assert_eq!(
            tx_substrate::wake::registry_summary().live,
            wake_before + 2,
            "msg queue creation registers send and recv wake sources"
        );

        execution::step_msgctl_in_ns_with_post(
            msqid,
            execution::IPC_RMID,
            None,
            &owner,
            &ns,
            direct_msg_ref_post,
        )
        .expect("IPC_RMID");
    }

    tx_test_support::drain_to_quiescence();

    assert_eq!(
        crate::wait_source::registry_summary().total,
        legacy_before,
        "IPC_RMID must release msg queue compatibility wait sources"
    );
    assert_eq!(
        tx_substrate::wake::registry_summary().live,
        wake_before,
        "IPC_RMID must unregister msg queue wake sources"
    );
}

#[test]
fn msgrcv_blocking_wait_yields_and_rmid_wakes_receiver() {
    let _g = setup();

    let owner = cred(1000, 1000);
    let ns = crate::process::nsproxy::sign_init_nsproxy().expect("nsproxy cap");
    let msqid = execution::step_msgget(IPC_EXCL, IPC_CREAT | 0o600, &owner, &ns).expect("msgget");

    let wait_source_id =
        match execution::step_msgrcv_v3_with_post(msqid, 8, 0, 0, &owner, direct_msg_ref_post) {
            StepOutcome::Yield {
                progress,
                shape: YieldShape::OnWaitSource { source, interests },
            } => {
                assert_eq!(progress, NoProgress);
                assert_eq!(interests.raw(), 1);
                source.raw()
            }
            other => panic!("expected blocking msgrcv to yield, got {other:?}"),
        };

    let source = tx_substrate::wake::lookup_source(WaitSourceId::new(wait_source_id))
        .expect("msg recv wait source should be registered");
    let mailbox = Arc::new(TaskMailbox::new());
    let generation = mailbox.next_generation();
    let subscriber = source.register(Arc::downgrade(&mailbox), generation, InterestMask::new(1));
    assert!(mailbox.poll().is_none(), "wait should park before IPC_RMID");

    execution::step_msgctl_in_ns_with_post(
        msqid,
        execution::IPC_RMID,
        None,
        &owner,
        &ns,
        direct_msg_ref_post,
    )
    .expect("IPC_RMID");

    assert!(
        matches!(
            mailbox.poll(),
            Some(MailboxEvent::SourceFired {
                generation: fired,
                ..
            }) if fired == generation
        ),
        "IPC_RMID must wake recv waiters"
    );
    source.unregister(subscriber);
    assert_eq!(
        execution::step_msgrcv_with_post(msqid, 8, 0, 0, &owner, direct_msg_ref_post),
        Err(Errno::EIDRM),
        "retry after namespace withdrawal must report the removed id"
    );
}

#[test]
fn msgsnd_blocking_wait_yields_and_rmid_wakes_sender() {
    let _g = setup();

    let owner = cred(1000, 1000);
    let ns = crate::process::nsproxy::sign_init_nsproxy().expect("nsproxy cap");
    let msqid = execution::step_msgget(IPC_EXCL, IPC_CREAT | 0o600, &owner, &ns).expect("msgget");
    let qbytes = match execution::step_msgctl_with_post(
        msqid,
        execution::IPC_STAT,
        None,
        &owner,
        direct_msg_ref_post,
    )
    .expect("IPC_STAT")
    {
        execution::MsgCtlResult::Stat(info) => info.qbytes,
        other => panic!("expected Stat, got {other:?}"),
    };
    let max_msg_size = match execution::step_msgctl_with_post(
        msqid,
        execution::MSG_INFO,
        None,
        &owner,
        direct_msg_ref_post,
    )
    .expect("MSG_INFO")
    {
        execution::MsgCtlResult::Info { msgmax, .. } => msgmax as usize,
        other => panic!("expected Info, got {other:?}"),
    };

    let mut remaining = qbytes;
    while remaining > 0 {
        let chunk = remaining.min(max_msg_size);
        execution::step_msgsnd_with_post(
            msqid,
            1,
            alloc::vec![b'x'; chunk],
            0,
            &owner,
            direct_msg_ref_post,
        )
        .expect("fill queue");
        remaining -= chunk;
    }

    let wait_source_id = match execution::step_msgsnd_v3_with_post(
        msqid,
        1,
        alloc::vec![b'y'],
        0,
        &owner,
        direct_msg_ref_post,
    ) {
        StepOutcome::Yield {
            progress,
            shape: YieldShape::OnWaitSource { source, interests },
        } => {
            assert_eq!(progress.bytes(), 0);
            assert_eq!(interests.raw(), 1);
            source.raw()
        }
        other => panic!("expected blocking msgsnd to yield, got {other:?}"),
    };

    let source = tx_substrate::wake::lookup_source(WaitSourceId::new(wait_source_id))
        .expect("msg send wait source should be registered");
    let mailbox = Arc::new(TaskMailbox::new());
    let generation = mailbox.next_generation();
    let subscriber = source.register(Arc::downgrade(&mailbox), generation, InterestMask::new(1));
    assert!(mailbox.poll().is_none(), "wait should park before IPC_RMID");

    execution::step_msgctl_in_ns_with_post(
        msqid,
        execution::IPC_RMID,
        None,
        &owner,
        &ns,
        direct_msg_ref_post,
    )
    .expect("IPC_RMID");

    assert!(
        matches!(
            mailbox.poll(),
            Some(MailboxEvent::SourceFired {
                generation: fired,
                ..
            }) if fired == generation
        ),
        "IPC_RMID must wake send waiters"
    );
    source.unregister(subscriber);
    assert_eq!(
        execution::step_msgsnd_with_post(
            msqid,
            1,
            alloc::vec![b'y'],
            0,
            &owner,
            direct_msg_ref_post,
        ),
        Err(Errno::EIDRM),
        "retry after namespace withdrawal must report the removed id"
    );
}
