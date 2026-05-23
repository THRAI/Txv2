use super::ProcessIdentity;
use crate::process::adapter::step_engine::{RestrictionStackHandle, SubjectIdentity};

#[test]
fn process_identity_implements_subject_identity_with_expected_associated_types() {
    fn assert_credential<I: SubjectIdentity<Credential = crate::cred::Cred>>() {}
    fn assert_restrictions<I: SubjectIdentity<Restrictions = RestrictionStackHandle>>() {}
    fn assert_thread<I: SubjectIdentity<ThreadIdentity = crate::thread_runtime::ThreadIdentity>>() {
    }
    assert_credential::<ProcessIdentity>();
    assert_restrictions::<ProcessIdentity>();
    assert_thread::<ProcessIdentity>();
}
