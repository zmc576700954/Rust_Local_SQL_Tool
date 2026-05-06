use core_lib::timeout_policy::TimeoutPolicy;

#[test]
fn timeout_policy_defaults_are_positive() {
    let p = TimeoutPolicy::default();
    assert!(p.db_connect.as_millis() > 0);
    assert!(p.db_query.as_millis() > 0);
    assert!(p.external_http_default.as_millis() > 0);
    assert!(p.job_poll_request.as_millis() > 0);
}

