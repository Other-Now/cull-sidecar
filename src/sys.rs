//! Process self-metrics. Linux reads /proc/self/status; elsewhere returns None.

/// (current RSS kB, peak RSS kB)
pub fn rss_kb() -> (Option<u64>, Option<u64>) {
    let Ok(s) = std::fs::read_to_string("/proc/self/status") else {
        return (None, None);
    };
    let field = |name: &str| {
        s.lines()
            .find(|l| l.starts_with(name))
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|v| v.parse().ok())
    };
    (field("VmRSS:"), field("VmHWM:"))
}
