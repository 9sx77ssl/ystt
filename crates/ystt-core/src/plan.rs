//! Deterministic HTTP range planning for video downloads.

/// Inclusive byte range.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ByteRange {
    /// First byte, inclusive.
    pub start: u64,
    /// Last byte, inclusive.
    pub end: u64,
}

impl ByteRange {
    /// Number of bytes in the range.
    pub fn len(self) -> u64 {
        self.end.saturating_sub(self.start).saturating_add(1)
    }

    /// Whether the range contains no bytes.
    pub fn is_empty(self) -> bool {
        self.end < self.start
    }
}

/// Planned set of ranges covering `[0, total)` with no gaps and no overlaps.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RangePlan {
    /// Total file size in bytes.
    pub total: u64,
    /// Ranges in ascending order.
    pub ranges: Vec<ByteRange>,
}

impl RangePlan {
    /// First range, which is requested before the rest are scheduled.
    pub fn first_range(&self) -> Option<ByteRange> {
        self.ranges.first().copied()
    }

    /// All ranges except the first.
    pub fn rest_ranges(&self) -> &[ByteRange] {
        self.ranges.get(1..).unwrap_or_default()
    }

    /// Whether more than one connection is planned.
    pub fn is_parallel(&self) -> bool {
        self.ranges.len() > 1
    }
}

/// Range planner tuned for "range-first" downloads.
///
/// The first request always asks for `first_chunk` bytes. If the file is larger,
/// the remaining bytes are split into at most `max_connections - 1` contiguous
/// ranges, each at least `min_chunk` bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RangePlanner {
    /// Size of the initial range request.
    pub first_chunk: u64,
    /// Minimum size of a follow-up range.
    pub min_chunk: u64,
    /// Maximum total number of connections, including the first.
    pub max_connections: usize,
}

impl Default for RangePlanner {
    fn default() -> Self {
        Self::new(6, 8 * 1024 * 1024, 2 * 1024 * 1024)
    }
}

impl RangePlanner {
    /// Create a planner. Values are clamped to at least 1.
    pub fn new(max_connections: usize, first_chunk: u64, min_chunk: u64) -> Self {
        Self {
            first_chunk: first_chunk.max(1),
            min_chunk: min_chunk.max(1),
            max_connections: max_connections.max(1),
        }
    }

    /// Plan a transfer with the configured first chunk.
    pub fn plan(&self, total: u64) -> RangePlan {
        let first = self.first_chunk.min(total);
        self.plan_with_first(total, first)
    }

    /// Plan a transfer when the first response already covered `first_len` bytes.
    ///
    /// `first_len` is clamped to `[1, total]`. A single range is returned when
    /// the file is small or only one connection is allowed.
    pub fn plan_with_first(&self, total: u64, first_len: u64) -> RangePlan {
        if total == 0 {
            return RangePlan {
                total: 0,
                ranges: Vec::new(),
            };
        }

        let first_len = first_len.clamp(1, total);
        if self.max_connections <= 1 || first_len >= total {
            return RangePlan {
                total,
                ranges: vec![ByteRange {
                    start: 0,
                    end: total - 1,
                }],
            };
        }

        let mut ranges = vec![ByteRange {
            start: 0,
            end: first_len - 1,
        }];

        let remaining = total - first_len;
        let available_workers = self.max_connections - 1;
        let needed_workers = ceil_div(remaining, self.min_chunk) as usize;
        let workers = available_workers.min(needed_workers).max(1);
        let chunk = ceil_div(remaining, workers as u64);

        let mut start = first_len;
        while start < total {
            let end = start.saturating_add(chunk).saturating_sub(1).min(total - 1);
            ranges.push(ByteRange { start, end });
            start = end + 1;
        }

        RangePlan { total, ranges }
    }
}

fn ceil_div(value: u64, divisor: u64) -> u64 {
    value.div_ceil(divisor)
}

/// Adaptive range-concurrency controller.
///
/// Starts at `start`, increases the limit when measured throughput improves by
/// 5% over the moving average, and backs off when it drops by 30%. The limit is
/// always clamped to `1..=max`.
#[derive(Debug, Clone)]
pub struct AdaptiveRangeController {
    max: usize,
    limit: usize,
    average_throughput: f64,
}

impl AdaptiveRangeController {
    /// Create a controller.
    pub fn new(max: usize, start: usize) -> Self {
        let max = max.max(1);
        Self {
            max,
            limit: start.max(1).min(max),
            average_throughput: 0.0,
        }
    }

    /// Current connection limit.
    pub fn limit(&self) -> usize {
        self.limit
    }

    /// Record a completed range.
    ///
    /// `bytes` and `elapsed_secs` describe one completed transfer. Returns the
    /// updated limit.
    pub fn observe(&mut self, bytes: u64, elapsed_secs: f64) -> usize {
        if bytes == 0 || elapsed_secs <= 0.0 {
            return self.limit;
        }
        let throughput = bytes as f64 / elapsed_secs;
        if self.average_throughput == 0.0 {
            self.average_throughput = throughput;
            return self.limit;
        }

        if throughput > self.average_throughput * 1.05 {
            self.limit = (self.limit + 1).min(self.max);
        } else if throughput < self.average_throughput * 0.70 {
            self.limit = self.limit.saturating_sub(1).max(1);
        }
        self.average_throughput = self.average_throughput * 0.7 + throughput * 0.3;
        self.limit
    }

    /// Back off after a range failure.
    pub fn on_error(&mut self) -> usize {
        self.limit = self.limit.saturating_sub(1).max(1);
        self.limit
    }

    /// Reset the moving average.
    pub fn reset(&mut self) {
        self.average_throughput = 0.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: u64 = 1024 * 1024;

    fn assert_complete(plan: &RangePlan) {
        assert_eq!(
            plan.ranges.iter().map(|range| range.len()).sum::<u64>(),
            plan.total
        );
        if let Some(first) = plan.ranges.first() {
            assert_eq!(first.start, 0);
        }
        for pair in plan.ranges.windows(2) {
            assert_eq!(pair[0].end + 1, pair[1].start, "gap or overlap");
        }
        if let Some(last) = plan.ranges.last() {
            assert_eq!(last.end + 1, plan.total);
        }
    }

    #[test]
    fn small_file_is_one_range() {
        let plan = RangePlanner::new(6, 8 * MIB, 2 * MIB).plan(2 * MIB);
        assert_eq!(plan.ranges.len(), 1);
        assert_eq!(
            plan.ranges[0],
            ByteRange {
                start: 0,
                end: 2 * MIB - 1
            }
        );
        assert_complete(&plan);
    }

    #[test]
    fn large_file_splits_into_bounded_ranges() {
        let planner = RangePlanner::new(6, 8 * MIB, 2 * MIB);
        let plan = planner.plan(100 * MIB);
        assert!(plan.is_parallel());
        assert!(plan.ranges.len() <= 6);
        assert_eq!(plan.ranges.len(), 6);
        assert_complete(&plan);
    }

    #[test]
    fn first_response_length_is_respected() {
        let plan = RangePlanner::new(4, 8 * MIB, 2 * MIB).plan_with_first(20 * MIB, 8 * MIB);
        assert_eq!(
            plan.ranges[0],
            ByteRange {
                start: 0,
                end: 8 * MIB - 1
            }
        );
        assert_eq!(plan.ranges.len(), 4);
        assert_complete(&plan);
    }

    #[test]
    fn one_connection_always_gets_one_range() {
        let plan = RangePlanner::new(1, 8 * MIB, 2 * MIB).plan(100 * MIB);
        assert_eq!(
            plan.ranges,
            vec![ByteRange {
                start: 0,
                end: 100 * MIB - 1
            }]
        );
        assert_complete(&plan);
    }

    #[test]
    fn empty_file_has_no_ranges() {
        let plan = RangePlanner::new(6, 8 * MIB, 2 * MIB).plan(0);
        assert_eq!(plan.total, 0);
        assert!(plan.ranges.is_empty());
    }

    #[test]
    fn min_chunk_limits_connection_count() {
        let plan = RangePlanner::new(8, 8 * MIB, 4 * MIB).plan(12 * MIB);
        assert_eq!(plan.ranges.len(), 2);
        assert_complete(&plan);
    }

    #[test]
    fn adaptive_controller_ramps_and_backs_off() {
        let mut controller = AdaptiveRangeController::new(8, 2);
        assert_eq!(controller.limit(), 2);
        controller.observe(1_000_000, 1.0);
        assert_eq!(controller.limit(), 2);
        controller.observe(2_000_000, 1.0);
        assert_eq!(controller.limit(), 3);
        controller.on_error();
        assert_eq!(controller.limit(), 2);
    }
}
