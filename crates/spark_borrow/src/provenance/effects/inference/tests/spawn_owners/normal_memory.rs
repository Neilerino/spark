// TAG [SPARK-114]

use super::*;
use crate::provenance::{
    address::OpaqueSite,
    demand::{Atom, Coordinate, Demand, Storage},
    observations::{FrameScope, Memory, Sample},
    tasks::memory::Lane,
    Slot,
};
use std::sync::Arc;

const INSTALLER: &str = r#"
struct Text { value: Ref[String] }
struct Bucket { values: List[Future[Int]]! }
struct Target { bucket: Ref[Bucket]! }
async fn wait(text: Text, input: Channel[Int]): Int { return input.recv() }
async fn hold(child: Future[Int], release: Channel[Int], target: Target): Int {
    values: List[Future[Int]] = [child]
    bucket: Ref[Bucket]! = target.bucket
    bucket.values = values
    return release.recv()
}
fn main(): Int {
    input: Channel[Int] = Channel[Int]()
    release: Channel[Int] = Channel[Int]()
    text: String = " borrowed ".trim()
    borrowed: Ref[String] = text
    shared: Text = Text { value: borrowed }
    child: Future[Int] = spawn { wait(shared, input) }
    empty: List[Future[Int]] = []
    bucket: Bucket! = Bucket { values: empty }
    bucketRef: Ref[Bucket]! = bucket
    target: Target = Target { bucket: bucketRef }
    keeper: Future[Int] = spawn { hold(child, release, target) }
    result: Int = await child
    return result
}
"#;

#[test]
fn newly_bound_normal_memory_imports_backing_before_settlement_and_keeps_its_rhs() {
    with_keeper(INSTALLER, |flow, mut state, parent, child, _, _, _| {
        let parent_value = state.value(parent);
        let handle = parent_value.futures[&Vec::new()].iter().next().unwrap();
        let site = handle.target.instantiation_site().unwrap();
        let Terminator::Call { target: block, .. } =
            flow.function.blocks[site.point.block.index()].terminator
        else {
            panic!("spawn successor");
        };
        let point = Point::new(block, 0);
        flow.transfers.remove(&point);
        flow.ensure_transfer(point, &state);
        let named = |name: &str| {
            flow.function
                .locals
                .iter()
                .find(|local| local.name.is_some_and(|symbol| symbol.as_str() == name))
                .unwrap()
                .local
        };
        let bucket = named("bucket");
        let list_ty = flow.function.locals[named("empty").index()].ty;
        let rhs = state.value(child).sourced(&Storage::Logical(child));
        let (rhs_path, origin, capture) = rhs
            .leaves
            .keys()
            .find(|(_, origin, _)| matches!(origin, crate::provenance::Origin::Loan(_)))
            .cloned()
            .unwrap();
        let rhs = flow
            .freeze_sources(point, rhs)
            .prefixed(Slot::Element, list_ty, flow.table);
        let witness = state.future_copies.witness(parent);
        let mut prepared = flow.prepare_task_memory(vec![witness], &[], point, &mut state);
        let normal = prepared
            .normal_values_mut()
            .find(|value| value.futures.contains_key(&vec![Slot::Element]))
            .expect("actual normal installer projection contains its child Future");
        let (actor, view) = normal
            .task_views
            .get_mut(&vec![Slot::Element])
            .unwrap()
            .iter_mut()
            .next()
            .unwrap();
        let lane = Arc::new(Lane {
            execution: crate::provenance::effects::Execution::Current,
            event: OpaqueSite {
                function: flow.function.def_id,
                point,
            },
            actor: actor.clone(),
            location: Sample {
                memory: Memory::Frame {
                    scope: FrameScope::Current,
                    function: flow.function.def_id,
                    local: bucket,
                    ty: flow.function.locals[bucket.index()].ty,
                },
                path: vec![Slot::Field(spark_intern::sym("values"))],
                ty: list_ty,
            },
        });
        assert!(!state.pending_memory.contains_key(&lane));
        state.invalidated_copies.clear();
        view.memory.import(lane.clone(), rhs);
        state.consume(child);
        flow.record_consumed(point, child);
        let settled = flow.commit_task_memory(prepared, point, &mut state);
        assert_eq!(settled.len(), 1);
        assert!(settled[0].contains_local(flow, point, parent));
        assert!(
            state.pending_memory.contains_key(&lane),
            "new final payload must import its deferred memory backing"
        );
        assert!(state.pending_memory[&lane].origins().contains(&origin));
        assert!(state.invalidated_copies.contains(&bucket));
        let mut path = vec![Slot::Element];
        path.extend(rhs_path.clone());
        let before = flow.transfers[&point].before(&Demand::from_iter([Atom {
            coordinate: Coordinate {
                storage: Storage::PendingWrite(lane.clone()),
                path,
            },
            origin: origin.clone(),
            capture,
        }]));
        assert!(before.contains(&Atom {
            coordinate: Coordinate {
                storage: Storage::Logical(child),
                path: rhs_path
            },
            origin,
            capture,
        }));
        assert!(!before
            .iter()
            .any(|atom| atom.coordinate.storage == Storage::PendingWrite(lane.clone())));
    });
}

#[test]
fn settled_completion_keeps_its_original_projection_after_a_late_pointer_write() {
    let source = INSTALLER
        .replace(
            "async fn wait",
            "struct Holder { target: Ref[Target]! }\nasync fn wait",
        )
        .replace("target: Target): Int", "holder: Holder): Int")
        .replace(
            "    bucket: Ref[Bucket]! = target.bucket",
            "    target: Ref[Target]! = holder.target\n    bucket: Ref[Bucket]! = target.bucket",
        )
        .replace(
            "target: Target = Target { bucket: bucketRef }",
            r#"
    target: Target! = Target { bucket: bucketRef }
    targetRef: Ref[Target]! = target
    holder: Holder = Holder { target: targetRef }
    otherBucket: Bucket! = Bucket { values: empty }
    otherRef: Ref[Bucket]! = otherBucket
"#,
        )
        .replace(
            "hold(child, release, target)",
            "hold(child, release, holder)",
        );
    with_keeper(&source, |flow, mut state, parent, _, _, _, borrowed| {
        let named = |name: &str| {
            flow.function
                .locals
                .iter()
                .find(|local| local.name.is_some_and(|symbol| symbol.as_str() == name))
                .unwrap()
                .local
        };
        let bucket = named("bucket");
        let other = named("otherBucket");
        let target = named("targetRef");
        let other_ref = named("otherRef");
        let site = state.value(parent).futures[&Vec::new()]
            .iter()
            .next()
            .unwrap()
            .target
            .instantiation_site()
            .unwrap();
        let Terminator::Call { target: block, .. } =
            flow.function.blocks[site.point.block.index()].terminator
        else {
            panic!("spawn successor")
        };
        let point = Point::new(block, 0);
        flow.transfers.remove(&point);
        flow.ensure_transfer(point, &state);
        let witness = state.future_copies.witness(parent);
        let prepared = flow.prepare_task_memory(vec![witness], &[], point, &mut state);
        let mut settled = flow.commit_task_memory(prepared, point, &mut state);
        assert_eq!(settled.len(), 1);
        let at = |location: &Sample, local: Local| {
            matches!(location.memory,
            Memory::Frame { scope: FrameScope::Current, local: actual, .. } if actual == local)
        };
        assert!(state.settled_memory.iter().any(
            |(location, value)| at(location, bucket) && !value.origins().is_disjoint(&borrowed)
        ));
        assert!(!state
            .settled_memory
            .keys()
            .any(|location| at(location, other)));
        let target_value = flow.read(&Place::from_local(target), &state);
        let target_ty = crate::provenance::value::child_type(
            flow.function.locals[target.index()].ty,
            &Slot::Deref,
            flow.table,
        )
        .unwrap();
        let metadata = flow.project_reference(
            &target_value,
            target_ty,
            &vec![Slot::Field(spark_intern::sym("bucket"))],
        );
        let replacement = state.value(other_ref).sourced(&Storage::Logical(other_ref));
        flow.store_references(
            &metadata,
            &replacement,
            flow.function.locals[other_ref.index()].ty,
            point,
            &mut state,
        );
        let pointer_holder = named("target");
        let current = state.value(parent);
        let handle = current.futures[&Vec::new()].iter().next().unwrap();
        let view = crate::provenance::tasks::views::contribution(&current, &[], handle);
        assert!(
            view.memory.interference.iter().any(|sample| {
                at(sample, pointer_holder)
                    && sample.path == [Slot::Field(spark_intern::sym("bucket"))]
            }),
            "the pending Future must retain the actual write to its destination pointer"
        );
        settled.pop().unwrap().finish(flow, None, point, &mut state);
        assert!(
            !state
                .settled_memory
                .keys()
                .any(|location| at(location, other)),
            "finishing must not rebind the completed worker to the new destination"
        );
        assert!(
            state
                .settled_memory
                .iter()
                .any(|(location, value)| at(location, bucket)
                    && !value.origins().is_disjoint(&borrowed)),
            "the original weak settlement and its borrowed payload must survive"
        );
    });
}

const OVERLAP: &str = r#"
struct Text { value: Ref[String] }
struct Bucket { values: List[Ref[String]]! }
struct Target { bucket: Ref[Bucket]! }
struct Holder { target: Ref[Target]! }
async fn wait(text: Text, input: Channel[Int], holder: Holder): Int {
    target: Ref[Target]! = holder.target
    bucket: Ref[Bucket]! = target.bucket
    values: List[Ref[String]] = [text.value]
    bucket.values = values
    return input.recv()
}
async fn hold(child: Future[Int], release: Channel[Int]): Int { return release.recv() }
fn main(): Int {
    input: Channel[Int] = Channel[Int]()
    release: Channel[Int] = Channel[Int]()
    text: String = " borrowed ".trim()
    borrowed: Ref[String] = text
    shared: Text = Text { value: borrowed }
    empty: List[Ref[String]] = []
    bucket: Bucket! = Bucket { values: empty }
    bucketRef: Ref[Bucket]! = bucket
    target: Target! = Target { bucket: bucketRef }
    targetRef: Ref[Target]! = target
    holder: Holder = Holder { target: targetRef }
    otherBucket: Bucket! = Bucket { values: empty }
    otherRef: Ref[Bucket]! = otherBucket
    child: Future[Int] = spawn { wait(shared, input, holder) }
    keeper: Future[Int] = spawn { hold(child, release) }
    result: Int = await child
    return result
}
"#;

#[test]
fn a_preselected_child_does_not_requery_during_parent_cleanup_in_either_order() {
    for parent_first in [false, true] {
        with_keeper(
            OVERLAP,
            |flow, mut state, parent, child, captured, _, borrowed| {
                let named = |name: &str| {
                    flow.function
                        .locals
                        .iter()
                        .find(|local| local.name.is_some_and(|symbol| symbol.as_str() == name))
                        .unwrap()
                        .local
                };
                let site = state.value(parent).futures[&Vec::new()]
                    .iter()
                    .next()
                    .unwrap()
                    .target
                    .instantiation_site()
                    .unwrap();
                let Terminator::Call { target: block, .. } =
                    flow.function.blocks[site.point.block.index()].terminator
                else {
                    panic!("spawn successor")
                };
                let point = Point::new(block, 0);
                flow.transfers.remove(&point);
                flow.ensure_transfer(point, &state);
                assert!(flow.transfer_drop(&Place::from_local(child), point, &mut state));
                let child_witness = state.future_copies.role_witness(&captured);
                assert_eq!(
                    child_witness.peers,
                    FxHashSet::from_iter([captured.clone()])
                );
                assert!(state.future_owners.known_role(&captured));
                let parent_witness = state.future_copies.witness(parent);
                let requests = if parent_first {
                    vec![parent_witness, child_witness]
                } else {
                    vec![child_witness, parent_witness]
                };
                let prepared = flow.prepare_task_memory(requests, &[], point, &mut state);
                let settled = flow.commit_task_memory(prepared, point, &mut state);
                assert_eq!(settled.len(), 2);
                let target = named("targetRef");
                let other_ref = named("otherRef");
                let target_value = flow.read(&Place::from_local(target), &state);
                let target_ty = crate::provenance::value::child_type(
                    flow.function.locals[target.index()].ty,
                    &Slot::Deref,
                    flow.table,
                )
                .unwrap();
                let metadata = flow.project_reference(
                    &target_value,
                    target_ty,
                    &vec![Slot::Field(spark_intern::sym("bucket"))],
                );
                let replacement = state.value(other_ref).sourced(&Storage::Logical(other_ref));
                flow.store_references(
                    &metadata,
                    &replacement,
                    flow.function.locals[other_ref.index()].ty,
                    point,
                    &mut state,
                );
                for completion in settled {
                    completion.finish(flow, None, point, &mut state);
                }
                let at = |memory: &Memory, local| {
                    matches!(memory,
                Memory::Frame { scope: FrameScope::Current, local: actual, .. } if *actual == local)
                };
                assert!(state.settled_memory.iter().any(|(location, value)| at(
                    &location.memory,
                    named("bucket")
                ) && !value
                    .origins()
                    .is_disjoint(&borrowed)));
                assert!(!state.settled_memory.keys().any(|location| at(&location.memory, named("otherBucket"))),
                "parent-first={parent_first}: cleanup must reuse the already-settled actual child");
            },
        );
    }
}

#[test]
fn a_settled_request_cannot_complete_a_refilled_slot_with_the_same_labels() {
    with_keeper(OVERLAP, |flow, mut state, _, child, _, _, _| {
        let point = Point::new(BlockId::ENTRY, 0);
        flow.transfers.remove(&point);
        flow.ensure_transfer(point, &state);
        let original = state.value(child);
        let witness = state.future_copies.witness(child);
        let prepared = flow.prepare_task_memory(vec![witness.clone()], &[], point, &mut state);
        let mut settled = flow.commit_task_memory(prepared, point, &mut state);
        assert!(settled[0].contains_local(flow, point, child));
        // Reuse the original labels deliberately, modeling a new spawn at the same site.
        flow.store(
            &Place::from_local(child),
            original.clone(),
            point,
            &mut state,
        );
        state.future_copies.assign(
            child,
            witness,
            &crate::provenance::tasks::Live::new(&state.live, &state.queue_current),
        );
        assert_eq!(state.value(child).futures, original.futures);
        settled.pop().unwrap().finish(flow, None, point, &mut state);
        assert_eq!(
            state.value(child).futures,
            original.futures,
            "the old request must not complete the replacement"
        );
    });
}
