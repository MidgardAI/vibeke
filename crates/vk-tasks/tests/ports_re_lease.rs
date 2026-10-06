//! `PortLeases::re_lease` (05 §6 `task ports --re-lease`).

use vk_tasks::{LeaseRequest, PortLeases, PortPool};

#[test]
fn re_lease_moves_to_another_block_of_the_same_size() {
    let tmp = tempfile::tempdir().unwrap();
    let pool = PortPool::parse("30000-30059", 10).unwrap();
    let pl = PortLeases::new(tmp.path(), pool).with_probe(false);
    let a = pl.lease_sized(&LeaseRequest::new("a", "s"), 20).unwrap();
    let b = pl.lease(&LeaseRequest::new("b", "s")).unwrap();
    let (old, new) = pl.re_lease(&LeaseRequest::new("a", "s"), 10).unwrap();
    assert_eq!(old.as_ref(), Some(&a));
    assert_eq!(new.count(), a.count(), "same size");
    assert!(new.end < a.start || new.start > a.end, "not the old block");
    assert!(
        new.end < b.start || new.start > b.end,
        "not another task's block"
    );
    let leases = pl.list().unwrap();
    assert_eq!(leases.iter().filter(|l| l.task_id == "a").count(), 1);
    // Without an old lease it is a plain lease.
    let (none, fresh) = pl.re_lease(&LeaseRequest::new("c", "s"), 10).unwrap();
    assert!(none.is_none());
    assert_eq!(fresh.count(), 10);
    // Fill the pool: re-leasing is exhausted, and the old lease is kept.
    pl.lease(&LeaseRequest::new("d", "s")).unwrap();
    pl.lease(&LeaseRequest::new("e", "s")).unwrap();
    assert!(pl.re_lease(&LeaseRequest::new("b", "s"), 10).is_err());
    assert_eq!(pl.lease_for("b").unwrap().as_ref(), Some(&b));
}
