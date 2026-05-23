use super::*;
use crate::cred::{CapabilitySet, Cred, Gid, Uid};
use crate::execution::Errno;
use crate::test_support::EPOCH_TEST_LOCK;
use crate::zones;

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
fn mq_open_same_name_isolated_by_ipc_namespace() {
    let _g = setup();

    let owner = cred(1000, 1000);
    let first_ns = crate::process::nsproxy::sign_init_nsproxy().expect("first nsproxy cap");
    let second_ns = crate::process::nsproxy::sign_init_nsproxy().expect("second nsproxy cap");
    let name = b"tx-mq-ns";
    let attr = execution::MqCreateAttr {
        maxmsg: 2,
        msgsize: 16,
    };

    let first = execution::step_mq_open(
        name,
        execution::MQ_O_CREAT | execution::MQ_O_EXCL,
        0o600,
        Some(attr),
        &owner,
        &first_ns,
    )
    .expect("first namespace creates queue");

    let second = execution::step_mq_open(
        name,
        execution::MQ_O_CREAT | execution::MQ_O_EXCL,
        0o600,
        Some(attr),
        &owner,
        &second_ns,
    )
    .expect("second namespace creates independent same-name queue");

    assert_ne!(first.msqid(), second.msqid());

    execution::step_mq_unlink(name, &first_ns).expect("unlink first namespace queue");
    execution::step_mq_unlink(name, &second_ns).expect("unlink second namespace queue");
}

#[test]
fn mq_namespace_entry_is_identity_cap_authority() {
    let _g = setup();

    let owner = cred(1000, 1000);
    let ns = crate::process::nsproxy::sign_init_nsproxy().expect("nsproxy cap");
    let name = b"tx-mq-cap-authority";
    let opened = execution::step_mq_open(
        name,
        execution::MQ_O_CREAT | execution::MQ_O_EXCL,
        0o600,
        Some(execution::MqCreateAttr {
            maxmsg: 2,
            msgsize: 16,
        }),
        &owner,
        &ns,
    )
    .expect("create queue");

    let mq_name = crate::process::nsproxy::PosixMqName::new(name);
    let namespace_identity = ns
        .ipc_ns
        .posix_mq
        .lock()
        .get(&mq_name)
        .expect("namespace entry")
        .clone();

    assert_eq!(
        namespace_identity.key().raw(),
        opened.identity.key().raw(),
        "IpcNamespace.posix_mq must be the authority for the queue identity cap"
    );

    execution::step_mq_unlink(name, &ns).expect("cleanup");
}

#[test]
fn mq_unlink_only_withdraws_from_calling_ipc_namespace() {
    let _g = setup();

    let owner = cred(1000, 1000);
    let first_ns = crate::process::nsproxy::sign_init_nsproxy().expect("first nsproxy cap");
    let second_ns = crate::process::nsproxy::sign_init_nsproxy().expect("second nsproxy cap");
    let name = b"tx-mq-ns-unlink";
    let attr = execution::MqCreateAttr {
        maxmsg: 2,
        msgsize: 16,
    };

    let first = execution::step_mq_open(
        name,
        execution::MQ_O_CREAT,
        0o600,
        Some(attr),
        &owner,
        &first_ns,
    )
    .expect("first namespace creates queue");
    let second = execution::step_mq_open(
        name,
        execution::MQ_O_CREAT,
        0o600,
        Some(attr),
        &owner,
        &second_ns,
    )
    .expect("second namespace creates queue");

    execution::step_mq_unlink(name, &first_ns).expect("unlink only first namespace");
    assert_eq!(
        execution::step_mq_open(name, 0, 0o600, None, &owner, &first_ns)
            .expect_err("first namespace name was withdrawn"),
        Errno::ENOENT
    );

    let reopened_second = execution::step_mq_open(name, 0, 0o600, None, &owner, &second_ns)
        .expect("second namespace name remains reachable");
    assert_eq!(reopened_second.msqid(), second.msqid());
    assert_ne!(first.msqid(), reopened_second.msqid());

    execution::step_mq_unlink(name, &second_ns).expect("cleanup second namespace queue");
}
