use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll, Waker};

use tx_reactor::userspace::{SyscallRequest, UserspaceTrapInfo};
use tx_shims::linux_syscall::{
    AF_INET, NR_ACCEPT4, NR_BIND, NR_CLOSE, NR_CONNECT, NR_EXIT_GROUP, NR_LISTEN, NR_READ,
    NR_SOCKET, NR_WRITE, O_NONBLOCK,
};
use tx_subsystems::net::delegate::{net_delegate_clear, DelegateWireSet, NetDelegateTaskConfig};
use tx_subsystems::net::protocol::loopback_iface;
use tx_subsystems::process::ExitStatus;

use super::{drive_boot_wiring, setup, CoreInit, TestPlatform, LAST_USERSPACE_CTX};

const SOCK_STREAM: u64 = 1;
const SOCKADDR_IN_BYTES: u64 = 16;
const SERVER_PORT: u16 = 54_039;
const CLIENT_PORT: u16 = 54_139;
const EINPROGRESS_A0: usize = (-115isize) as usize;

fn sockaddr_in_loopback(port: u16) -> [u8; SOCKADDR_IN_BYTES as usize] {
    let mut bytes = [0u8; SOCKADDR_IN_BYTES as usize];
    bytes[0..2].copy_from_slice(&AF_INET.to_le_bytes());
    bytes[2..4].copy_from_slice(&port.to_be_bytes());
    bytes[4..8].copy_from_slice(&[127, 0, 0, 1]);
    bytes
}

fn expect_pending<F: Future<Output = ()>>(
    pinned: &mut Pin<&mut F>,
    cx: &mut Context<'_>,
    label: &str,
) {
    match pinned.as_mut().poll(cx) {
        Poll::Pending => {}
        Poll::Ready(()) => panic!("{label}: run_thread returned Ready unexpectedly"),
    }
}

fn submit_reactor_delegate_for_smoke(ready_steps: usize) {
    CoreInit::<TestPlatform>::init_boot_reactor_for_test();
    CoreInit::<TestPlatform>::submit_net_delegate_task_for_test(NetDelegateTaskConfig::run_steps(
        ready_steps,
    ))
    .expect("boot reactor must accept net delegate task");

    let parked = CoreInit::<TestPlatform>::step_boot_reactor_once_for_test()
        .expect("boot reactor must step after delegate submit");
    assert_eq!(
        parked.stats.polled, 1,
        "first reactor step should poll delegate into wait_on_token"
    );
    assert_eq!(
        parked.stats.completed, 0,
        "bounded delegate should park before any POLL/TICK"
    );
}

fn drive_reactor_delegate_poll(label: &str) -> tx_reactor::hart_loop::HartLoopStep {
    for _ in 0..4 {
        let step = CoreInit::<TestPlatform>::step_boot_reactor_once_for_test()
            .expect("boot reactor must step for net delegate");
        if step.stats.polled > 0 {
            return step;
        }
    }
    panic!("{label}: net delegate task did not run after POLL");
}

#[test]
fn boot_smoke_userspace_tcp_loopback_uses_reactor_owned_delegate() {
    let _serial = setup();
    drive_boot_wiring();

    loopback_iface().clear_for_test_or_bootstrap();
    net_delegate_clear(DelegateWireSet::POLL | DelegateWireSet::TICK);

    let init = tx_subsystems::process::execution::init_process()
        .expect("INIT_PROCESS must be populated post-bootstrap");
    let leader = init
        .nth_thread(0)
        .expect("init has a leader thread post-bootstrap");
    let payload = leader
        .payload_cap()
        .expect("leader payload must be alive before userspace smoke");
    payload.store_saved_user_context(Some(tx_hal::UserTrapContext {
        regs: [0; 32],
        pc: 0,
        status: 0,
        fp: tx_hal::UserFpContext::empty(),
    }));

    let waker = Waker::noop().clone();
    let mut cx = Context::from_waker(&waker);
    let future = crate::thread_future::run_thread::<TestPlatform>(leader.clone(), payload.clone());
    let wrapped =
        crate::thread_future::PerHartSlotted::<TestPlatform, _>::new(payload.clone(), future);
    let mut boxed = std::boxed::Box::new(wrapped);
    // SAFETY: `boxed` is owned by this test and is not moved after pinning.
    let mut pinned = unsafe { Pin::new_unchecked(&mut *boxed) };

    expect_pending(&mut pinned, &mut cx, "initial userspace entry");
    assert_eq!(
        LAST_USERSPACE_CTX
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .expect("first userspace entry recorded")
            .regs[10],
        0
    );

    macro_rules! syscall {
        ($nr:expr, $args:expr, $label:literal) => {{
            *LAST_USERSPACE_CTX.lock().unwrap_or_else(|e| e.into_inner()) = None;
            let active = payload
                .active_userspace_request()
                .expect(concat!($label, ": active userspace request"));
            payload
                .userspace_slot()
                .complete_interesting_trap(
                    active,
                    UserspaceTrapInfo::Syscall(SyscallRequest::new($nr, $args)),
                )
                .expect(concat!($label, ": complete userspace trap"));
            let mut returned = None;
            for _ in 0..4 {
                expect_pending(&mut pinned, &mut cx, $label);
                returned = *LAST_USERSPACE_CTX.lock().unwrap_or_else(|e| e.into_inner());
                if returned.is_some() {
                    break;
                }
            }
            returned
                .expect(concat!($label, ": syscall returned to userspace"))
                .regs[10]
        }};
    }

    let listener_addr = sockaddr_in_loopback(SERVER_PORT);
    let client_addr = sockaddr_in_loopback(CLIENT_PORT);
    submit_reactor_delegate_for_smoke(2);

    let listener_fd = syscall!(
        NR_SOCKET,
        [AF_INET as u64, SOCK_STREAM | O_NONBLOCK as u64, 0, 0, 0, 0],
        "socket(listener)"
    );
    assert_eq!(listener_fd, 3);
    assert_eq!(
        syscall!(
            NR_BIND,
            [
                listener_fd as u64,
                listener_addr.as_ptr() as u64,
                SOCKADDR_IN_BYTES,
                0,
                0,
                0
            ],
            "bind(listener)"
        ),
        0
    );
    assert_eq!(
        syscall!(NR_LISTEN, [listener_fd as u64, 8, 0, 0, 0, 0], "listen"),
        0
    );

    let client_fd = syscall!(
        NR_SOCKET,
        [AF_INET as u64, SOCK_STREAM | O_NONBLOCK as u64, 0, 0, 0, 0],
        "socket(client)"
    );
    assert_eq!(client_fd, 4);
    assert_eq!(
        syscall!(
            NR_BIND,
            [
                client_fd as u64,
                client_addr.as_ptr() as u64,
                SOCKADDR_IN_BYTES,
                0,
                0,
                0
            ],
            "bind(client)"
        ),
        0
    );
    assert_eq!(
        syscall!(
            NR_CONNECT,
            [
                client_fd as u64,
                listener_addr.as_ptr() as u64,
                SOCKADDR_IN_BYTES,
                0,
                0,
                0
            ],
            "connect(client)"
        ),
        EINPROGRESS_A0
    );

    let connected = drive_reactor_delegate_poll("connect(client)");
    assert!(
        connected.stats.polled > 0,
        "connect must wake reactor-owned net delegate"
    );

    let accepted_fd = syscall!(
        NR_ACCEPT4,
        [listener_fd as u64, 0, 0, O_NONBLOCK as u64, 0, 0],
        "accept4(listener)"
    );
    assert_eq!(accepted_fd, 5);

    let message = b"hello";
    assert_eq!(
        syscall!(
            NR_WRITE,
            [
                client_fd as u64,
                message.as_ptr() as u64,
                message.len() as u64,
                0,
                0,
                0
            ],
            "write(client)"
        ),
        message.len()
    );
    let transferred = drive_reactor_delegate_poll("write(client)");
    assert!(
        transferred.stats.polled > 0,
        "write must wake reactor-owned net delegate"
    );
    assert_eq!(
        transferred.stats.completed, 1,
        "bounded delegate should finish after the second ready wake"
    );

    let mut recv_buf = [0u8; 5];
    assert_eq!(
        syscall!(
            NR_READ,
            [
                accepted_fd as u64,
                recv_buf.as_mut_ptr() as u64,
                recv_buf.len() as u64,
                0,
                0,
                0
            ],
            "read(accepted)"
        ),
        message.len()
    );
    assert_eq!(&recv_buf, message);

    assert_eq!(
        syscall!(
            NR_CLOSE,
            [accepted_fd as u64, 0, 0, 0, 0, 0],
            "close(accepted)"
        ),
        0
    );
    assert_eq!(
        syscall!(NR_CLOSE, [client_fd as u64, 0, 0, 0, 0, 0], "close(client)"),
        0
    );
    assert_eq!(
        syscall!(
            NR_CLOSE,
            [listener_fd as u64, 0, 0, 0, 0, 0],
            "close(listener)"
        ),
        0
    );

    let active = payload
        .active_userspace_request()
        .expect("exit_group: active userspace request");
    payload
        .userspace_slot()
        .complete_interesting_trap(
            active,
            UserspaceTrapInfo::Syscall(SyscallRequest::new(NR_EXIT_GROUP, [0, 0, 0, 0, 0, 0])),
        )
        .expect("exit_group trap completion");
    match pinned.as_mut().poll(&mut cx) {
        Poll::Ready(()) => {}
        Poll::Pending => panic!("exit_group poll returned Pending; expected Ready"),
    }

    assert!(init.is_zombie(), "exit_group must zombify init");
    assert_eq!(init.exit_status(), Some(ExitStatus::Exited(0)));

    drop(boxed);
    let _ = tx_subsystems::thread_runtime::clear_current_thread_payload(0);
    loopback_iface().clear_for_test_or_bootstrap();
    net_delegate_clear(DelegateWireSet::POLL | DelegateWireSet::TICK);
}
