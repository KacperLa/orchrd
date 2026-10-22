//! The single-instance guard.

use std::time::Duration;

use apple_tv_remote::lock;

/// Two daemons must not share one uinput device, and the guard must hand the
/// lock back when the holder exits.
#[test]
fn a_second_instance_is_rejected_until_the_first_releases() {
    let directory = tempfile::tempdir().expect("temp dir");
    let first = lock::acquire_in(directory.path(), "instance-test").expect("first lock");

    let second = lock::acquire_in(directory.path(), "instance-test");
    assert!(second.is_err(), "a second daemon took the same lock");

    drop(first);
    // A failed attempt does not wedge the lock for the next start.
    std::thread::sleep(Duration::from_millis(10));
    assert!(lock::acquire_in(directory.path(), "instance-test").is_ok());
}

/// Different names never collide, so unrelated daemons coexist.
#[test]
fn differently_named_daemons_do_not_compete() {
    let directory = tempfile::tempdir().expect("temp dir");
    let _first = lock::acquire_in(directory.path(), "one").expect("first lock");
    assert!(lock::acquire_in(directory.path(), "two").is_ok());
}
