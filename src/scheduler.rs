use std::time::{SystemTime, UNIX_EPOCH};

pub struct Scheduler {
    start: (u8, u8), // (hour, minute) 24h
    end: (u8, u8),
}

impl Scheduler {
    pub fn new(start: &str, end: &str) -> anyhow::Result<Self> {
        Ok(Self {
            start: parse_hhmm(start)?,
            end: parse_hhmm(end)?,
        })
    }

    /// Returns true if caching is currently allowed (uses real local time).
    pub fn is_caching_allowed(&self) -> bool {
        let (h, m) = local_hm();
        self.is_allowed_at(h, m)
    }

    /// Testable version: check a specific (hour, minute).
    pub fn is_allowed_at(&self, h: u8, m: u8) -> bool {
        let now = h as u32 * 60 + m as u32;
        let start = self.start.0 as u32 * 60 + self.start.1 as u32;
        let end = self.end.0 as u32 * 60 + self.end.1 as u32;
        if start <= end {
            // Normal window, e.g. 08:00–22:00
            now >= start && now < end
        } else {
            // Wraps midnight, e.g. 22:00–02:00
            now >= start || now < end
        }
    }
}

fn parse_hhmm(s: &str) -> anyhow::Result<(u8, u8)> {
    let mut parts = s.splitn(2, ':');
    let h: u8 = parts.next().ok_or_else(|| anyhow::anyhow!("bad time '{}'", s))?.parse()?;
    let m: u8 = parts.next().ok_or_else(|| anyhow::anyhow!("bad time '{}'", s))?.parse()?;
    anyhow::ensure!(h < 24 && m < 60, "time out of range: {}", s);
    Ok((h, m))
}

fn local_hm() -> (u8, u8) {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::localtime_r(&ts, &mut tm) };
    (tm.tm_hour as u8, tm.tm_min as u8)
}
