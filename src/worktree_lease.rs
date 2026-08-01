use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::fs::File;
use std::io::Write;
use std::time::{SystemTime, UNIX_EPOCH};

pub(crate) const LEASE_REASON_PREFIX: &str = "ez-lease:";
pub(crate) const LEASE_REASON_VERSION: u8 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct MutationHolder {
    pid: u32,
    operation: String,
}

#[derive(Debug)]
pub(crate) struct LeaseMutationGuard {
    file: File,
}

impl LeaseMutationGuard {
    pub(crate) fn acquire(operation: &str) -> Result<Self> {
        let path = crate::git::git_common_dir()?
            .join("ez")
            .join("worktree-lease.lock");
        let holder = MutationHolder {
            pid: std::process::id(),
            operation: operation.to_string(),
        };
        let token = serde_json::to_string(&holder)?;
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)?;
        if let Err(error) = try_lock_exclusive(&file) {
            if error.kind() == std::io::ErrorKind::WouldBlock {
                let current = std::fs::read_to_string(&path)
                    .ok()
                    .and_then(|contents| serde_json::from_str::<MutationHolder>(&contents).ok());
                let held_by = current.map_or_else(
                    || "another ez process".to_string(),
                    |holder| format!("`{}` in pid {}", holder.operation, holder.pid),
                );
                bail!(
                    "another ez worktree ownership operation is in progress ({held_by})\n  \
                     → Wait for that process to finish"
                );
            }
            return Err(error.into());
        }
        if let Err(error) = file
            .set_len(0)
            .and_then(|()| file.write_all(token.as_bytes()))
        {
            let _ = unlock_file(&file);
            return Err(error.into());
        }
        Ok(Self { file })
    }
}

impl Drop for LeaseMutationGuard {
    fn drop(&mut self) {
        let _ = unlock_file(&self.file);
    }
}

#[cfg(unix)]
fn try_lock_exclusive(file: &File) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;

    unsafe extern "C" {
        fn flock(fd: std::ffi::c_int, operation: std::ffi::c_int) -> std::ffi::c_int;
    }

    const LOCK_EX: std::ffi::c_int = 2;
    const LOCK_NB: std::ffi::c_int = 4;
    let result = unsafe { flock(file.as_raw_fd(), LOCK_EX | LOCK_NB) };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(unix)]
fn unlock_file(file: &File) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;

    unsafe extern "C" {
        fn flock(fd: std::ffi::c_int, operation: std::ffi::c_int) -> std::ffi::c_int;
    }

    const LOCK_UN: std::ffi::c_int = 8;
    let result = unsafe { flock(file.as_raw_fd(), LOCK_UN) };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(not(unix))]
fn try_lock_exclusive(_file: &File) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "worktree lease coordination requires Unix file locking",
    ))
}

#[cfg(not(unix))]
fn unlock_file(_file: &File) -> std::io::Result<()> {
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Lease {
    pub(crate) version: u8,
    pub(crate) owner: String,
    pub(crate) branch: String,
    pub(crate) created_at: u64,
    pub(crate) expires_at: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct LeaseView {
    pub(crate) owner: String,
    pub(crate) branch: String,
    pub(crate) created_at: u64,
    pub(crate) expires_at: u64,
    pub(crate) stale: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TtlParseError {
    Empty,
    MissingSuffix,
    InvalidNumber,
    NonPositive,
    Overflow,
    UnknownSuffix,
}

impl fmt::Display for TtlParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => write!(f, "TTL is required"),
            Self::MissingSuffix => write!(f, "TTL must end with s, m, h, or d"),
            Self::InvalidNumber => write!(f, "TTL amount must be a positive integer"),
            Self::NonPositive => write!(f, "TTL must be greater than zero"),
            Self::Overflow => write!(f, "TTL is too large"),
            Self::UnknownSuffix => write!(f, "TTL suffix must be one of s, m, h, or d"),
        }
    }
}

impl std::error::Error for TtlParseError {}

impl Lease {
    pub(crate) fn new(
        owner: impl Into<String>,
        branch: impl Into<String>,
        created_at: u64,
        ttl_seconds: u64,
    ) -> Result<Self> {
        let expires_at = created_at
            .checked_add(ttl_seconds)
            .ok_or(TtlParseError::Overflow)?;
        let lease = Self {
            version: LEASE_REASON_VERSION,
            owner: owner.into(),
            branch: branch.into(),
            created_at,
            expires_at,
        };
        if lease.owner.is_empty() {
            bail!("lease owner is required");
        }
        if lease.branch.is_empty() {
            bail!("lease branch is required");
        }
        if ttl_seconds == 0 {
            bail!("lease TTL must be greater than zero");
        }
        Ok(lease)
    }

    pub(crate) fn is_stale_at(&self, unix_time: u64) -> bool {
        unix_time >= self.expires_at
    }

    pub(crate) fn is_stale(&self) -> Result<bool> {
        Ok(self.is_stale_at(now_unix()?))
    }

    pub(crate) fn reason(&self) -> Result<String> {
        Ok(format!(
            "{LEASE_REASON_PREFIX}{}",
            serde_json::to_string(self)?
        ))
    }

    pub(crate) fn parse_reason(reason: &str) -> Option<Self> {
        let payload = reason.strip_prefix(LEASE_REASON_PREFIX)?;
        let lease: Self = serde_json::from_str(payload).ok()?;
        lease.is_well_formed().then_some(lease)
    }

    pub(crate) fn view(&self, unix_time: u64) -> LeaseView {
        LeaseView {
            owner: self.owner.clone(),
            branch: self.branch.clone(),
            created_at: self.created_at,
            expires_at: self.expires_at,
            stale: self.is_stale_at(unix_time),
        }
    }

    fn is_well_formed(&self) -> bool {
        self.version == LEASE_REASON_VERSION
            && !self.owner.is_empty()
            && !self.branch.is_empty()
            && self.created_at < self.expires_at
    }
}

pub(crate) fn parse_ttl(ttl: &str) -> Result<u64> {
    if ttl.is_empty() {
        bail!(TtlParseError::Empty);
    }

    let suffix = ttl.chars().last().ok_or(TtlParseError::Empty)?;
    let multiplier = match suffix {
        's' => 1_u64,
        'm' => 60,
        'h' => 60 * 60,
        'd' => 24 * 60 * 60,
        '0'..='9' => bail!(TtlParseError::MissingSuffix),
        _ => bail!(TtlParseError::UnknownSuffix),
    };

    let amount = &ttl[..ttl.len() - suffix.len_utf8()];
    if amount.is_empty() || !amount.bytes().all(|byte| byte.is_ascii_digit()) {
        bail!(TtlParseError::InvalidNumber);
    }

    let amount = amount.parse::<u64>().map_err(|_| TtlParseError::Overflow)?;
    if amount == 0 {
        bail!(TtlParseError::NonPositive);
    }

    amount
        .checked_mul(multiplier)
        .ok_or_else(|| TtlParseError::Overflow.into())
}

pub(crate) fn now_unix() -> Result<u64> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{CwdGuard, init_git_repo, take_env_lock};

    fn lease() -> Lease {
        Lease::new("agent-a", "feat/demo", 100, 3600).expect("lease")
    }

    #[test]
    fn render_and_parse_round_trip_the_git_lock_reason() {
        let lease = lease();
        let reason = lease.reason().expect("reason");

        assert_eq!(
            reason,
            r#"ez-lease:{"version":1,"owner":"agent-a","branch":"feat/demo","created_at":100,"expires_at":3700}"#
        );
        assert_eq!(Lease::parse_reason(&reason), Some(lease));
    }

    #[test]
    fn parse_returns_none_for_foreign_or_malformed_reasons() {
        let cases = [
            "",
            "maintenance window",
            "ez-lease:",
            "ez-lease:not-json",
            r#"ez-lease:[]"#,
            r#"ez-lease:{"version":2,"owner":"agent-a","branch":"feat/demo","created_at":100,"expires_at":3700}"#,
            r#"ez-lease:{"version":1,"owner":"","branch":"feat/demo","created_at":100,"expires_at":3700}"#,
            r#"ez-lease:{"version":1,"owner":"agent-a","branch":"","created_at":100,"expires_at":3700}"#,
            r#"ez-lease:{"version":1,"owner":"agent-a","branch":"feat/demo","created_at":100,"expires_at":100}"#,
            r#"ez-lease:{"version":1,"owner":"agent-a","branch":"feat/demo","created_at":100,"expires_at":3700,"extra":true}"#,
            r#"ez-lease:{"version":1,"owner":"agent-a","created_at":100,"expires_at":3700}"#,
        ];

        for case in cases {
            assert_eq!(Lease::parse_reason(case), None, "case: {case}");
        }
    }

    #[test]
    fn stale_status_uses_the_supplied_unix_time() {
        let lease = lease();

        assert!(!lease.is_stale_at(3699));
        assert!(lease.is_stale_at(3700));
        assert!(lease.is_stale_at(3701));
    }

    #[test]
    fn view_is_json_serializable_and_marks_stale_at_supplied_time() {
        let value = serde_json::to_value(lease().view(3700)).expect("serialize view");

        assert_eq!(
            value,
            serde_json::json!({
                "owner": "agent-a",
                "branch": "feat/demo",
                "created_at": 100,
                "expires_at": 3700,
                "stale": true,
            })
        );
    }

    #[test]
    fn new_rejects_empty_identity_and_expiry_overflow() {
        assert!(Lease::new("", "feat/demo", 100, 1).is_err());
        assert!(Lease::new("agent-a", "", 100, 1).is_err());
        assert!(Lease::new("agent-a", "feat/demo", 100, 0).is_err());
        assert!(Lease::new("agent-a", "feat/demo", u64::MAX, 1).is_err());
    }

    #[test]
    fn parse_ttl_accepts_positive_supported_suffixes() {
        assert_eq!(parse_ttl("1s").expect("ttl"), 1);
        assert_eq!(parse_ttl("15m").expect("ttl"), 900);
        assert_eq!(parse_ttl("2h").expect("ttl"), 7200);
        assert_eq!(parse_ttl("3d").expect("ttl"), 259200);
    }

    #[test]
    fn parse_ttl_rejects_invalid_forms() {
        let cases = [
            ("", TtlParseError::Empty),
            ("15", TtlParseError::MissingSuffix),
            ("0s", TtlParseError::NonPositive),
            ("-1s", TtlParseError::InvalidNumber),
            ("1.5h", TtlParseError::InvalidNumber),
            (" h", TtlParseError::InvalidNumber),
            ("1w", TtlParseError::UnknownSuffix),
            ("1H", TtlParseError::UnknownSuffix),
        ];

        for (input, expected) in cases {
            let error = parse_ttl(input).expect_err("invalid ttl");
            assert_eq!(
                error.downcast_ref::<TtlParseError>().copied(),
                Some(expected),
                "input: {input}"
            );
        }
    }

    #[test]
    fn parse_ttl_reports_number_and_multiplier_overflow() {
        assert_eq!(
            parse_ttl("18446744073709551616s")
                .expect_err("overflow")
                .downcast_ref::<TtlParseError>()
                .copied(),
            Some(TtlParseError::Overflow)
        );

        let overflowing_days = format!("{}d", u64::MAX);
        assert_eq!(
            parse_ttl(&overflowing_days)
                .expect_err("overflow")
                .downcast_ref::<TtlParseError>()
                .copied(),
            Some(TtlParseError::Overflow)
        );
    }

    #[test]
    fn mutation_guard_serializes_lease_and_cleanup_operations() {
        let _env = take_env_lock();
        let repo = init_git_repo("lease-mutation-guard");
        let _cwd = CwdGuard::enter(&repo);
        std::fs::create_dir_all(repo.join(".git/ez")).expect("create ez metadata directory");

        let first = LeaseMutationGuard::acquire("claim feat/a").expect("first guard");
        let blocked = LeaseMutationGuard::acquire("delete feat/a").expect_err("second must block");
        assert!(blocked.to_string().contains("claim feat/a"));

        drop(first);
        let second = LeaseMutationGuard::acquire("delete feat/a").expect("guard after release");
        drop(second);
        assert!(repo.join(".git/ez/worktree-lease.lock").exists());
    }

    #[test]
    fn mutation_guard_recovers_a_demonstrably_dead_process_holder() {
        let _env = take_env_lock();
        let repo = init_git_repo("lease-mutation-dead-holder");
        let _cwd = CwdGuard::enter(&repo);
        let ez_dir = repo.join(".git/ez");
        std::fs::create_dir_all(&ez_dir).expect("create ez metadata directory");
        let lock_path = ez_dir.join("worktree-lease.lock");
        std::fs::write(
            &lock_path,
            r#"{"pid":4294967295,"operation":"claim feat/old"}"#,
        )
        .expect("write crashed holder");

        let guard = LeaseMutationGuard::acquire("claim feat/new")
            .expect("dead holder should be recovered automatically");
        drop(guard);

        let holder: MutationHolder = serde_json::from_str(
            &std::fs::read_to_string(&lock_path).expect("recovered holder metadata"),
        )
        .expect("valid recovered metadata");
        assert_eq!(holder.operation, "claim feat/new");
    }

    #[test]
    fn mutation_guard_replaces_stale_malformed_metadata_when_kernel_lock_is_free() {
        let _env = take_env_lock();
        let repo = init_git_repo("lease-mutation-malformed-holder");
        let _cwd = CwdGuard::enter(&repo);
        let ez_dir = repo.join(".git/ez");
        std::fs::create_dir_all(&ez_dir).expect("create ez metadata directory");
        let lock_path = ez_dir.join("worktree-lease.lock");
        std::fs::write(&lock_path, "not a process identity").expect("write malformed holder");

        let guard = LeaseMutationGuard::acquire("delete feat/new")
            .expect("an unlocked crash artifact cannot block future operations");
        let holder: MutationHolder = serde_json::from_str(
            &std::fs::read_to_string(&lock_path).expect("current holder metadata"),
        )
        .expect("valid holder metadata");
        assert_eq!(holder.pid, std::process::id());
        assert_eq!(holder.operation, "delete feat/new");
        drop(guard);
    }
}
