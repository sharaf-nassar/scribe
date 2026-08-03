//! Shared cooperative controls for untrusted terminal-image decoders.

use std::error::Error;
use std::fmt;
use std::time::Instant;

/// Caller-selected work, deadline, and observation limits for one decode.
#[derive(Clone, Copy, Debug)]
pub struct DecodeLimits {
    pub max_width_pixels: usize,
    pub max_height_pixels: usize,
    pub max_pixels: usize,
    pub max_rgba_bytes: usize,
    pub max_work_units: u64,
    pub deadline: Instant,
    pub check_interval_work_units: u64,
}

impl DecodeLimits {
    /// Frozen terminal-images-v1 limits with a caller-selected deadline.
    pub const fn terminal_images_v1(deadline: Instant) -> Self {
        Self {
            max_width_pixels: 4_096,
            max_height_pixels: 4_096,
            max_pixels: 16_777_216,
            max_rgba_bytes: 67_108_864,
            max_work_units: 134_217_728,
            deadline,
            check_interval_work_units: 4_096,
        }
    }

    pub const fn validate(self) -> Result<(), BudgetError> {
        if self.max_width_pixels == 0 || self.max_height_pixels == 0 {
            return Err(BudgetError::InvalidLimits);
        }
        if self.max_pixels == 0
            || self.max_rgba_bytes == 0
            || self.max_work_units == 0
            || self.check_interval_work_units == 0
        {
            return Err(BudgetError::InvalidLimits);
        }
        Ok(())
    }
}

/// Payload-free allocation denial from a caller hook.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AllocationDenied;

impl fmt::Display for AllocationDenied {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("allocation denied")
    }
}

impl Error for AllocationDenied {}

/// Cooperative controls owned by the decode caller.
pub trait DecodeHooks {
    fn is_cancelled(&self) -> bool;

    fn before_allocation(&self, _requested_bytes: usize) -> Result<(), AllocationDenied> {
        Ok(())
    }
}

/// Default hooks for callers that need only deadline/work enforcement.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoopHooks;

impl DecodeHooks for NoopHooks {
    fn is_cancelled(&self) -> bool {
        false
    }
}

/// Shared payload-free failures at cooperative decode boundaries.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BudgetError {
    InvalidLimits,
    WorkBudgetExceeded { requested: u64, maximum: u64 },
    DecodeDeadlineExceeded { work_units: u64 },
    DecodeCancelled { work_units: u64 },
    AllocationFailed { requested_bytes: usize },
}

impl fmt::Display for BudgetError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidLimits => formatter.write_str("invalid decode limits"),
            Self::WorkBudgetExceeded { requested, maximum } => {
                write!(formatter, "work budget exceeded: {requested} > {maximum}")
            }
            Self::DecodeDeadlineExceeded { work_units } => {
                write!(formatter, "decode deadline exceeded at {work_units} work units")
            }
            Self::DecodeCancelled { work_units } => {
                write!(formatter, "decode cancelled at {work_units} work units")
            }
            Self::AllocationFailed { requested_bytes } => {
                write!(formatter, "allocation failed for {requested_bytes} bytes")
            }
        }
    }
}

impl Error for BudgetError {}

/// Observable statistics for one completed or rejected decode.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DecodeStats {
    pub work_units: u64,
    pub cooperative_checks: u64,
    pub peak_live_allocation_bytes: usize,
}

// @lat: [[terminal-images#Terminal Images#Shared Decode Budget]]
/// Caller-owned cumulative work, cancellation, deadline, and allocation state.
pub struct DecodeBudget<'a> {
    limits: DecodeLimits,
    hooks: &'a dyn DecodeHooks,
    work_units: u64,
    next_check: u64,
    checks: u64,
    live_allocation_bytes: usize,
    peak_live_allocation_bytes: usize,
}

impl<'a> DecodeBudget<'a> {
    pub fn new(limits: DecodeLimits, hooks: &'a impl DecodeHooks) -> Result<Self, BudgetError> {
        limits.validate()?;
        let mut budget = Self {
            limits,
            hooks,
            work_units: 0,
            next_check: limits.check_interval_work_units,
            checks: 0,
            live_allocation_bytes: 0,
            peak_live_allocation_bytes: 0,
        };
        budget.check_now()?;
        Ok(budget)
    }

    pub const fn limits(&self) -> DecodeLimits {
        self.limits
    }

    pub fn charge(&mut self, units: u64) -> Result<(), BudgetError> {
        let requested =
            self.work_units.checked_add(units).ok_or(BudgetError::WorkBudgetExceeded {
                requested: u64::MAX,
                maximum: self.limits.max_work_units,
            })?;
        if requested > self.limits.max_work_units {
            return Err(BudgetError::WorkBudgetExceeded {
                requested,
                maximum: self.limits.max_work_units,
            });
        }
        while self.next_check <= requested {
            self.work_units = self.next_check;
            self.check_now()?;
            self.next_check = self
                .next_check
                .checked_add(self.limits.check_interval_work_units)
                .ok_or(BudgetError::WorkBudgetExceeded {
                    requested,
                    maximum: self.limits.max_work_units,
                })?;
        }
        self.work_units = requested;
        Ok(())
    }

    pub fn check_now(&mut self) -> Result<(), BudgetError> {
        self.checks = self.checks.saturating_add(1);
        if self.hooks.is_cancelled() {
            return Err(BudgetError::DecodeCancelled { work_units: self.work_units });
        }
        if Instant::now() >= self.limits.deadline {
            return Err(BudgetError::DecodeDeadlineExceeded { work_units: self.work_units });
        }
        Ok(())
    }

    pub fn begin_allocation(&mut self, bytes: usize) -> Result<(), BudgetError> {
        self.hooks
            .before_allocation(bytes)
            .map_err(|_| BudgetError::AllocationFailed { requested_bytes: bytes })?;
        self.live_allocation_bytes = self
            .live_allocation_bytes
            .checked_add(bytes)
            .ok_or(BudgetError::AllocationFailed { requested_bytes: bytes })?;
        self.peak_live_allocation_bytes =
            self.peak_live_allocation_bytes.max(self.live_allocation_bytes);
        Ok(())
    }

    pub fn end_allocation(&mut self, bytes: usize) {
        self.live_allocation_bytes = self.live_allocation_bytes.saturating_sub(bytes);
    }

    pub const fn stats(&self) -> DecodeStats {
        DecodeStats {
            work_units: self.work_units,
            cooperative_checks: self.checks,
            peak_live_allocation_bytes: self.peak_live_allocation_bytes,
        }
    }
}
