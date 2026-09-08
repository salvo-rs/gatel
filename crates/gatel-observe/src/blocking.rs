//! Transport-independent scrape data. No registry or traffic-derived labels.
use std::fmt::Write as _;

#[derive(Clone, Copy, Debug, Default)]
pub struct BlockingSample {
    pub jobs: u64,
    pub running: u64,
    pub memory_bytes: u64,
    pub jobs_limit: u64,
    pub memory_limit_bytes: u64,
    pub closed: bool,
    pub admitted: u64,
    pub rejected_jobs: u64,
    pub rejected_memory: u64,
    pub rejected_closed: u64,
    pub completed: u64,
    pub panicked: u64,
    pub abandoned: u64,
}

/// Append one coherent budget snapshot to a normal Prometheus scrape.
/// Values are integers, preserving u64 limits without signed casts.
pub fn append_blocking(out: &mut String, sample: BlockingSample) {
    for (name, help, value) in [
        (
            "gatel_blocking_jobs",
            "Admitted blocking jobs, queued or running.",
            sample.jobs,
        ),
        (
            "gatel_blocking_running_jobs",
            "Blocking closures currently running.",
            sample.running,
        ),
        (
            "gatel_blocking_memory_bytes",
            "Reserved declared working memory, not process RSS.",
            sample.memory_bytes,
        ),
        (
            "gatel_blocking_jobs_limit",
            "Configured blocking job admission limit.",
            sample.jobs_limit,
        ),
        (
            "gatel_blocking_memory_limit_bytes",
            "Configured declared working memory admission limit.",
            sample.memory_limit_bytes,
        ),
        (
            "gatel_blocking_closed",
            "Whether the budget refuses all new admission.",
            u64::from(sample.closed),
        ),
    ] {
        let _ = writeln!(out, "# HELP {name} {help}\n# TYPE {name} gauge\n{name} {value}");
    }
    let _ = writeln!(
        out,
        "# HELP gatel_blocking_admissions_total Accepted blocking work reservations.\n# TYPE gatel_blocking_admissions_total counter\ngatel_blocking_admissions_total {}",
        sample.admitted
    );
    let _ = writeln!(
        out,
        "# HELP gatel_blocking_rejections_total Admission refusals; exclusive precedence closed, jobs, memory.\n# TYPE gatel_blocking_rejections_total counter"
    );
    for (reason, value) in [
        ("jobs", sample.rejected_jobs),
        ("memory", sample.rejected_memory),
        ("closed", sample.rejected_closed),
    ] {
        let _ = writeln!(
            out,
            "gatel_blocking_rejections_total{{reason=\"{reason}\"}} {value}"
        );
    }
    let _ = writeln!(
        out,
        "# HELP gatel_blocking_completions_total Finished reservations; completed means closure returned, not authentication success.\n# TYPE gatel_blocking_completions_total counter"
    );
    for (outcome, value) in [
        ("completed", sample.completed),
        ("panicked", sample.panicked),
        ("abandoned", sample.abandoned),
    ] {
        let _ = writeln!(
            out,
            "gatel_blocking_completions_total{{outcome=\"{outcome}\"}} {value}"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn samples_have_fixed_cardinality_and_unsigned_values() {
        let sample = BlockingSample {
            memory_limit_bytes: u64::MAX,
            admitted: u64::MAX,
            ..Default::default()
        };
        let mut first = String::new();
        append_blocking(&mut first, sample);
        let mut second = String::new();
        append_blocking(&mut second, sample);
        assert_eq!(first, second);
        assert_eq!(first.lines().filter(|line| !line.starts_with('#')).count(), 13);
        assert!(first.contains("gatel_blocking_memory_limit_bytes 18446744073709551615\n"));
        for name in [
            "gatel_blocking_admissions_total",
            "gatel_blocking_rejections_total",
            "gatel_blocking_completions_total",
        ] {
            assert_eq!(first.matches(&format!("# TYPE {name} counter")).count(), 1);
        }
    }
}
