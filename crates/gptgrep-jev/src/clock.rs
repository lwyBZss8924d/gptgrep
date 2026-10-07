//! Immutable same-boot deadline anchor for durable, same-host run accounting.
use anyhow::{Result, anyhow, ensure};
use serde::{Deserialize, Serialize};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Persist the original anchor and adopt it on joins; never recapture its lifetime.
/// Private boot identity is deliberately excluded from Debug output.
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RunClockAnchor {
    schema: String,
    deadline_unix_ms: u64,
    clock_kind: String,
    boot_identity: String,
    clock_namespace: Option<String>,
    monotonic_started_ns: u64,
    monotonic_deadline_ns: u64,
}

struct Sample {
    clock_kind: &'static str,
    boot_identity: String,
    clock_namespace: Option<String>,
    monotonic_ns: u64,
}

impl RunClockAnchor {
    /// Capture the full run lifetime, without a per-request timeout cap.
    pub fn capture(absolute_deadline: SystemTime) -> Result<Self> {
        // Sample the monotonic clock first: time spent reading UTC only shortens
        // this anchor, instead of extending it beyond the requested lifetime.
        let sample = sample()?;
        Self::capture_at(absolute_deadline, SystemTime::now(), sample)
    }

    fn capture_at(deadline: SystemTime, wall: SystemTime, sample: Sample) -> Result<Self> {
        let deadline_unix_ms = deadline_millis(deadline)?;
        let deadline = UNIX_EPOCH + Duration::from_millis(deadline_unix_ms);
        let lifetime = deadline
            .duration_since(wall)
            .map_err(|_| anyhow!("Run absolute deadline has expired"))?;
        ensure!(!lifetime.is_zero(), "Run absolute deadline has expired");
        let lifetime_ns = u64::try_from(lifetime.as_nanos())
            .map_err(|_| anyhow!("Invalid run clock lifetime"))?;
        Ok(Self {
            schema: "gptgrep.run-clock-anchor.v1".into(),
            deadline_unix_ms,
            clock_kind: sample.clock_kind.into(),
            boot_identity: sample.boot_identity,
            clock_namespace: sample.clock_namespace,
            monotonic_started_ns: sample.monotonic_ns,
            monotonic_deadline_ns: sample
                .monotonic_ns
                .checked_add(lifetime_ns)
                .ok_or_else(|| anyhow!("Invalid run clock lifetime"))?,
        })
    }

    pub fn deadline_unix_ms(&self) -> u64 {
        self.deadline_unix_ms
    }

    /// Validate the explicit UTC binding and current boot/clock identity.
    /// Expiration is checked by remaining(); accounting can still read expired runs.
    pub fn verify_deadline(&self, absolute_deadline: SystemTime) -> Result<()> {
        ensure!(
            self.deadline_unix_ms == deadline_millis(absolute_deadline)?,
            "Run clock absolute deadline binding mismatch"
        );
        self.verify_sample(&sample()?)
    }

    /// Bound time by the original monotonic deadline and current UTC deadline.
    /// Reboots, time namespace changes and backward monotonic samples fail closed.
    pub fn remaining(&self) -> Result<Duration> {
        self.remaining_at(SystemTime::now(), &sample()?)
    }

    fn verify_sample(&self, current: &Sample) -> Result<()> {
        ensure!(
            self.schema == "gptgrep.run-clock-anchor.v1"
                && self.clock_kind == current.clock_kind
                && self.boot_identity == current.boot_identity
                && self.clock_namespace == current.clock_namespace
                && self.monotonic_deadline_ns > self.monotonic_started_ns
                && current.monotonic_ns >= self.monotonic_started_ns,
            "Run clock boot/clock binding is unavailable or changed"
        );
        Ok(())
    }

    fn remaining_at(&self, wall: SystemTime, current: &Sample) -> Result<Duration> {
        self.verify_sample(current)?;
        let monotonic = self
            .monotonic_deadline_ns
            .checked_sub(current.monotonic_ns)
            .filter(|ns| *ns > 0)
            .map(Duration::from_nanos)
            .ok_or_else(|| anyhow!("Run absolute deadline has expired"))?;
        let utc = (UNIX_EPOCH + Duration::from_millis(self.deadline_unix_ms))
            .duration_since(wall)
            .map_err(|_| anyhow!("Run absolute deadline has expired"))?;
        let remaining = monotonic.min(utc);
        ensure!(!remaining.is_zero(), "Run absolute deadline has expired");
        Ok(remaining)
    }
}

fn deadline_millis(deadline: SystemTime) -> Result<u64> {
    u64::try_from(
        deadline
            .duration_since(UNIX_EPOCH)
            .map_err(|_| anyhow!("Invalid run absolute deadline"))?
            .as_millis(),
    )
    .map_err(|_| anyhow!("Invalid run absolute deadline"))
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn sample() -> Result<Sample> {
    #[cfg(target_os = "macos")]
    let (clock, kind, namespace) = (libc::CLOCK_MONOTONIC_RAW, "macos.clock_monotonic_raw", None);
    #[cfg(target_os = "linux")]
    let (clock, kind, namespace) = {
        // CLOCK_BOOTTIME is virtualized by Linux time namespaces. A shared boot
        // alone is insufficient to compare samples across distinct namespaces.
        let namespace = std::fs::read_link("/proc/self/ns/time")
            .map_err(|_| anyhow!("Run clock time namespace is unavailable"))?;
        let namespace = namespace
            .to_str()
            .filter(|s| s.len() <= 128 && s.starts_with("time:[") && s.ends_with(']'))
            .ok_or_else(|| anyhow!("Invalid run clock time namespace"))?
            .to_owned();
        (
            libc::CLOCK_BOOTTIME,
            "linux.clock_boottime",
            Some(namespace),
        )
    };
    let boot_identity = boot_identity()?;
    let mut value = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: value is a valid writable timespec; the selected OS clock is read-only.
    let status = unsafe { libc::clock_gettime(clock, &mut value) };
    ensure!(
        status == 0 && value.tv_sec >= 0 && (0..1_000_000_000).contains(&value.tv_nsec),
        "Run monotonic clock is unavailable"
    );
    let monotonic_ns = u64::try_from(value.tv_sec)
        .ok()
        .and_then(|s| s.checked_mul(1_000_000_000))
        .and_then(|s| s.checked_add(value.tv_nsec as u64))
        .ok_or_else(|| anyhow!("Invalid run monotonic clock sample"))?;
    Ok(Sample {
        clock_kind: kind,
        boot_identity,
        clock_namespace: namespace,
        monotonic_ns,
    })
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn sample() -> Result<Sample> {
    Err(anyhow!(
        "Persistent run clock is unsupported on this platform"
    ))
}

#[cfg(target_os = "macos")]
fn boot_identity() -> Result<String> {
    let mut bytes = [0u8; 128];
    let mut length = bytes.len();
    // SAFETY: fixed read-only sysctl name, valid bounded output buffer and size.
    let status = unsafe {
        libc::sysctlbyname(
            c"kern.bootsessionuuid".as_ptr(),
            bytes.as_mut_ptr().cast(),
            &mut length,
            std::ptr::null_mut(),
            0,
        )
    };
    ensure!(
        status == 0 && length > 0 && length <= bytes.len(),
        "Run clock boot identity is unavailable"
    );
    let text = std::str::from_utf8(&bytes[..length])
        .map_err(|_| anyhow!("Invalid run clock boot identity"))?
        .trim_end_matches('\0');
    validate_boot_identity(text)
}

#[cfg(target_os = "linux")]
fn boot_identity() -> Result<String> {
    use std::io::Read;
    let mut bytes = Vec::new();
    std::fs::File::open("/proc/sys/kernel/random/boot_id")
        .map_err(|_| anyhow!("Run clock boot identity is unavailable"))?
        .take(129)
        .read_to_end(&mut bytes)
        .map_err(|_| anyhow!("Run clock boot identity is unavailable"))?;
    ensure!(bytes.len() <= 128, "Invalid run clock boot identity");
    let text = std::str::from_utf8(&bytes)
        .map_err(|_| anyhow!("Invalid run clock boot identity"))?
        .trim_end_matches('\n');
    validate_boot_identity(text)
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn validate_boot_identity(text: &str) -> Result<String> {
    ensure!(
        text.len() == 36
            && text.bytes().enumerate().all(|(i, b)| {
                if matches!(i, 8 | 13 | 18 | 23) {
                    b == b'-'
                } else {
                    b.is_ascii_hexdigit()
                }
            }),
        "Invalid run clock boot identity"
    );
    Ok(text.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn invented_sample(ns: u64) -> Sample {
        Sample {
            clock_kind: "invented-clock",
            boot_identity: "invented-boot".into(),
            clock_namespace: None,
            monotonic_ns: ns,
        }
    }
    #[test]
    fn original_anchor_survives_utc_rollback_and_uncapped_lifetime() {
        let wall = UNIX_EPOCH + Duration::from_secs(1000);
        let anchor =
            RunClockAnchor::capture_at(wall + Duration::from_secs(60), wall, invented_sample(100))
                .unwrap();
        assert_eq!(
            anchor.remaining_at(wall, &invented_sample(100)).unwrap(),
            Duration::from_secs(60)
        );
        let later = invented_sample(100 + 59_000_000_000);
        assert_eq!(
            anchor
                .remaining_at(wall - Duration::from_secs(3600), &later)
                .unwrap(),
            Duration::from_secs(1)
        );
        assert!(
            anchor
                .remaining_at(
                    wall - Duration::from_secs(3600),
                    &invented_sample(100 + 60_000_000_000)
                )
                .is_err()
        );
        let wire = serde_json::to_vec(&anchor).unwrap();
        let joined: RunClockAnchor = serde_json::from_slice(&wire).unwrap();
        assert!(joined == anchor);
        assert_eq!(
            joined
                .remaining_at(wall - Duration::from_secs(3600), &later)
                .unwrap(),
            Duration::from_secs(1)
        );
    }
    #[test]
    fn reboot_namespace_change_monotonic_rollback_and_utc_expiration_reject() {
        let wall = UNIX_EPOCH + Duration::from_secs(1000);
        let anchor =
            RunClockAnchor::capture_at(wall + Duration::from_secs(60), wall, invented_sample(100))
                .unwrap();
        let mut changed = invented_sample(101);
        changed.boot_identity = "other-boot".into();
        assert!(anchor.remaining_at(wall, &changed).is_err());
        changed = invented_sample(101);
        changed.clock_namespace = Some("different-namespace".into());
        assert!(anchor.remaining_at(wall, &changed).is_err());
        assert!(anchor.remaining_at(wall, &invented_sample(99)).is_err());
        assert!(
            anchor
                .remaining_at(wall + Duration::from_secs(61), &invented_sample(101))
                .is_err()
        );
    }
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn actual_clock_roundtrip_and_deadline_binding() {
        let deadline = SystemTime::now() + Duration::from_secs(60);
        let anchor = RunClockAnchor::capture(deadline).unwrap();
        assert!(anchor.remaining().unwrap() > Duration::from_secs(20));
        anchor.verify_deadline(deadline).unwrap();
        assert!(
            anchor
                .verify_deadline(deadline + Duration::from_secs(1))
                .is_err()
        );
        let joined: RunClockAnchor =
            serde_json::from_slice(&serde_json::to_vec(&anchor).unwrap()).unwrap();
        assert!(joined == anchor);
    }
}
