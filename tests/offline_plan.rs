//! Offline range-planner tests.

use ystt::{ByteRange, RangePlanner};

const MIB: u64 = 1024 * 1024;

fn assert_complete(total: u64, ranges: &[ByteRange]) {
    assert_eq!(ranges.iter().map(|range| range.len()).sum::<u64>(), total);
    assert_eq!(ranges.first().unwrap().start, 0);
    for pair in ranges.windows(2) {
        assert_eq!(pair[0].end + 1, pair[1].start);
    }
    assert_eq!(ranges.last().unwrap().end + 1, total);
}

#[test]
fn small_files_use_one_range() {
    let plan = RangePlanner::new(6, 8 * MIB, 2 * MIB).plan(3 * MIB);
    assert_eq!(plan.ranges.len(), 1);
    assert_complete(plan.total, &plan.ranges);
}

#[test]
fn large_files_are_bounded_and_contiguous() {
    let plan = RangePlanner::new(6, 8 * MIB, 2 * MIB).plan(100 * MIB);
    assert!(plan.ranges.len() <= 6);
    assert!(plan.ranges.len() > 1);
    assert_complete(plan.total, &plan.ranges);
}

#[test]
fn first_response_length_shapes_the_rest() {
    let plan = RangePlanner::new(4, 8 * MIB, 2 * MIB).plan_with_first(20 * MIB, 8 * MIB);
    assert_eq!(
        plan.ranges[0],
        ByteRange {
            start: 0,
            end: 8 * MIB - 1
        }
    );
    assert_complete(plan.total, &plan.ranges);
}

#[test]
fn single_connection_never_splits() {
    let plan = RangePlanner::new(1, 8 * MIB, 2 * MIB).plan(100 * MIB);
    assert_eq!(plan.ranges.len(), 1);
    assert_complete(plan.total, &plan.ranges);
}

#[test]
fn min_chunk_limit_is_respected() {
    let plan = RangePlanner::new(8, 8 * MIB, 4 * MIB).plan(12 * MIB);
    assert_eq!(plan.ranges.len(), 2);
    assert_complete(plan.total, &plan.ranges);
}

#[test]
fn empty_file_has_no_ranges() {
    let plan = RangePlanner::new(6, 8 * MIB, 2 * MIB).plan(0);
    assert!(plan.ranges.is_empty());
}
