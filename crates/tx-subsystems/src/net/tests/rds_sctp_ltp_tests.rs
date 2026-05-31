use super::*;
use crate::net::SocketRecvBytesOutcome;

fn rds_socket() -> tx_substrate::zone::Cap<SocketIdentity> {
    registry::create_socket_for_test_or_bootstrap(
        SocketKind::RdsSeqPacket,
        SocketOptionSet::for_kind(SocketKind::RdsSeqPacket),
    )
    .expect("rds socket")
}

fn sctp_socket() -> tx_substrate::zone::Cap<SocketIdentity> {
    registry::create_socket_for_test_or_bootstrap(
        SocketKind::Sctp,
        SocketOptionSet::for_kind(SocketKind::Sctp),
    )
    .expect("sctp socket")
}

#[test]
fn rds_and_sctp_socket_type_validation_matches_ltp_tuples() {
    let rds = ValidSocketType::validate(21, 5, 0).expect("rds seqpacket");
    assert_eq!(rds.domain, AddressFamily::Rds);
    assert_eq!(
        SocketKind::from_valid_socket_type(rds),
        Ok(SocketKind::RdsSeqPacket)
    );

    let sctp4 = ValidSocketType::validate(2, 1, 132).expect("ipv4 sctp");
    assert_eq!(
        SocketKind::from_valid_socket_type(sctp4),
        Ok(SocketKind::Sctp)
    );

    let sctp6 = ValidSocketType::validate(10, 1, 132).expect("ipv6 sctp");
    assert_eq!(
        SocketKind::from_valid_socket_type(sctp6),
        Ok(SocketKind::Sctp)
    );
}

#[test]
fn rds_loopback_seqpacket_delivers_source_endpoint() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    crate::net::reset_initial_net_namespace_for_test();
    let guard = tx_substrate::epoch::guard();

    let server = rds_socket();
    let client = rds_socket();
    assert_eq!(
        step_bind(&server, inet(4000), &guard),
        StepOutcome::Done(())
    );
    assert_eq!(
        step_bind(&client, inet(4001), &guard),
        StepOutcome::Done(())
    );

    let payload = b"hello world\0";
    assert_eq!(
        step_send_to_kernel_bytes(
            &client,
            Some(endpoint(4000)),
            payload,
            SendRecvFlags::empty(),
            &guard
        ),
        StepOutcome::Done(payload.len())
    );

    let mut out = [0u8; 128];
    let outcome = step_recv_kernel_bytes(&server, &mut out, SendRecvFlags::empty(), &guard);
    assert_eq!(
        outcome,
        StepOutcome::Done(SocketRecvBytesOutcome {
            bytes: payload.len(),
            source: Some(endpoint(4001)),
            unix_source: None,
            packet_source: None,
            destination: Some(endpoint(4000)),
            truncated: false,
            became_empty: true,
        })
    );
    assert_eq!(&out[..payload.len()], payload);
}

#[test]
fn sctp_loopback_stream_accepts_and_moves_bytes() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    crate::net::reset_initial_net_namespace_for_test();
    let guard = tx_substrate::epoch::guard();

    let listener = sctp_socket();
    assert_eq!(
        step_bind(&listener, inet(4100), &guard),
        StepOutcome::Done(())
    );
    assert_eq!(step_listen(&listener, 1, &guard), StepOutcome::Done(()));

    let client = sctp_socket();
    assert_eq!(
        step_bind(&client, inet(4101), &guard),
        StepOutcome::Done(())
    );
    assert_eq!(
        step_connect(&client, inet(4100), &guard),
        StepOutcome::Done(())
    );

    let accepted = match step_accept(&listener, &guard) {
        StepOutcome::Done(accepted) => accepted.child,
        _ => panic!("unexpected accept outcome"),
    };

    let request = 7u32.to_ne_bytes();
    assert_eq!(
        step_send_kernel_bytes(&accepted, &request, SendRecvFlags::empty(), &guard),
        StepOutcome::Done(request.len())
    );
    let mut request_out = [0u8; 4];
    assert_eq!(
        step_recv_kernel_bytes(&client, &mut request_out, SendRecvFlags::empty(), &guard),
        StepOutcome::Done(SocketRecvBytesOutcome {
            bytes: request.len(),
            source: None,
            unix_source: None,
            packet_source: None,
            destination: None,
            truncated: false,
            became_empty: true,
        })
    );
    assert_eq!(request_out, request);

    let response = b"IPv4 loop SCTP\0";
    assert_eq!(
        step_send_kernel_bytes(&client, response, SendRecvFlags::empty(), &guard),
        StepOutcome::Done(response.len())
    );
    let mut response_out = [0u8; 64];
    assert_eq!(
        step_recv_kernel_bytes(&accepted, &mut response_out, SendRecvFlags::empty(), &guard),
        StepOutcome::Done(SocketRecvBytesOutcome {
            bytes: response.len(),
            source: None,
            unix_source: None,
            packet_source: None,
            destination: None,
            truncated: false,
            became_empty: true,
        })
    );
    assert_eq!(&response_out[..response.len()], response);
}

#[test]
fn sctp_ipv6_wildcard_listener_accepts_loopback_connect() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    crate::net::reset_initial_net_namespace_for_test();
    let guard = tx_substrate::epoch::guard();

    let listener_valid = ValidSocketType::validate(10, 1, 132).expect("sctp6 listener");
    let listener = match step_socket_create(listener_valid, &guard) {
        StepOutcome::Done(socket) => socket,
        _ => panic!("unexpected listener create outcome"),
    };
    assert_eq!(
        step_bind(&listener, any_inet6(4200), &guard),
        StepOutcome::Done(())
    );
    assert_eq!(step_listen(&listener, 1, &guard), StepOutcome::Done(()));

    let client_valid = ValidSocketType::validate(10, 1, 132).expect("sctp6 client");
    let client = match step_socket_create(client_valid, &guard) {
        StepOutcome::Done(socket) => socket,
        _ => panic!("unexpected client create outcome"),
    };
    assert_eq!(
        step_bind(
            &client,
            KernelSockAddr::V6(SockAddrIn6::new(4201, Ipv6Address::LOOPBACK)),
            &guard
        ),
        StepOutcome::Done(())
    );
    assert_eq!(
        step_connect(
            &client,
            KernelSockAddr::V6(SockAddrIn6::new(4200, Ipv6Address::LOOPBACK)),
            &guard
        ),
        StepOutcome::Done(())
    );
    assert!(matches!(
        step_accept(&listener, &guard),
        StepOutcome::Done(_)
    ));
}

#[test]
fn sctp_and_tcp_bind_same_port_independently() {
    init_zones();
    let _lock = crate::test_support::EPOCH_TEST_LOCK
        .lock()
        .expect("net epoch test lock");
    crate::net::reset_initial_net_namespace_for_test();
    let guard = tx_substrate::epoch::guard();

    let tcp = registry::create_socket_for_test_or_bootstrap(
        SocketKind::Tcp,
        SocketOptionSet::for_kind(SocketKind::Tcp),
    )
    .expect("tcp socket");
    let sctp = sctp_socket();

    assert_eq!(step_bind(&tcp, inet(4300), &guard), StepOutcome::Done(()));
    assert_eq!(step_bind(&sctp, inet(4300), &guard), StepOutcome::Done(()));
}
