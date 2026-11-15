// TAG [SPARK-114]

use super::*;
use crate::facts::Point;
use crate::provenance::channels::{allocation, Capture};
use spark_mir::BlockId;
use spark_types::IntWidth;

#[test]
fn widened_channel_captures_keep_shared_targets_and_wake_metadata_joins() {
    let mut table = TypeTable::new();
    let int = table.intern(Ty::Int(IntWidth::I64));
    let channel = table.intern(Ty::Channel { inner: int });
    let callable = table.intern(Ty::Function {
        params: Vec::new(),
        ret: int,
        is_async: false,
    });
    let first = Capture {
        owner: DefId::new(9900),
        index: 0,
        ty: channel,
    };
    let second = Capture { index: 1, ..first };
    let base = allocation(DefId::new(9901), Point::new(BlockId::ENTRY, 0), channel);
    let widen = |mut value: Value| {
        for _ in 0..32 {
            value = value.prefixed(
                Slot::Capture(DefId::new(9902), 0, callable),
                callable,
                &table,
            );
        }
        value
    };
    let mut joined = widen(base.with_channel_capture(first));
    assert!(joined.join(&widen(base.with_channel_capture(second))));
    assert!(!joined.join(&joined.clone()));
    assert!(!joined.is_empty());
    assert!(joined.queues.keys().all(|path| path.len() <= 2));
    let first_value = joined.select_channel_capture(first);
    let second_value = joined.select_channel_capture(second);
    let left = first_value
        .root_queue_handles()
        .next()
        .expect("first handle");
    let right = second_value
        .root_queue_handles()
        .next()
        .expect("second handle");
    assert_eq!(left.target, right.target);
    assert_eq!(first_value.root_queue_handles().count(), 1);
    assert_eq!(second_value.root_queue_handles().count(), 1);
    let unknown = widen(allocation(
        DefId::new(9901),
        Point::new(BlockId::ENTRY, 1),
        channel,
    ));
    assert!(joined.join(&unknown));
    let selected = joined.select_channel_capture(first);
    assert_eq!(selected.root_queue_handles().count(), 2);
    assert!(selected
        .root_queue_handles()
        .all(|handle| handle.capture.is_none_or(|capture| capture == first)));
}
