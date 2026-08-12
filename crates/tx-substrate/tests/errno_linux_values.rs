use tx_substrate::step::Errno;

#[test]
fn elibbad_uses_linux_errno_80() {
    assert_eq!(Errno::ELIBBAD.linux_i32(), 80);
}
