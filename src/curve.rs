//! Fan curve, hysteresis and the duty floor.
//!
//! The curve is a table of steps. A step names a duty cycle and the CPU,
//! disk and controller temperatures at which that duty becomes necessary;
//! the highest step whose limit is reached by *any* sensor wins. Increases
//! take effect immediately. Decreases need the temperatures to be
//! `hysteresis` degrees below the limit, are limited to `drop_step`
//! percentage points and are spaced `drop_interval` apart.

use std::time::Duration;

use crate::pmc::{MAX_DUTY, MIN_DUTY};

/// Plausible sensor range in degrees Celsius; anything else is a fault.
pub const VALID_TEMPS: std::ops::RangeInclusive<f64> = 5.0..=110.0;

/// Returns `value` if it is a plausible temperature.
pub fn checked_temperature(value: f64) -> Option<f64> {
    VALID_TEMPS.contains(&value).then_some(value)
}

/// One row of the fan curve.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Step {
    pub duty: u8,
    pub cpu: f64,
    pub disk: f64,
    pub pmc: f64,
}

/// A set of temperature readings. `disk` is `None` when no disk is monitored.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Temps {
    pub cpu: f64,
    pub disk: Option<f64>,
    pub pmc: f64,
}

/// The curve and its drop policy.
#[derive(Clone, Debug, PartialEq)]
pub struct Curve {
    steps: Vec<Step>,
    floor: u8,
    hysteresis: f64,
    drop_step: u8,
    drop_interval: Duration,
}

/// Why a curve was rejected.
#[derive(Debug, PartialEq, Eq)]
pub struct CurveError(pub String);

impl std::fmt::Display for CurveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl Default for Curve {
    fn default() -> Self {
        Curve {
            steps: Curve::default_steps(),
            floor: MIN_DUTY,
            hysteresis: 3.0,
            drop_step: 15,
            drop_interval: Duration::from_secs(30),
        }
    }
}

impl Curve {
    /// Builds a curve, checking that duties are in range and strictly rising.
    pub fn new(
        steps: Vec<Step>,
        floor: u8,
        hysteresis: f64,
        drop_step: u8,
        drop_interval: Duration,
    ) -> Result<Curve, CurveError> {
        if !(MIN_DUTY..=MAX_DUTY).contains(&floor) {
            return Err(CurveError(format!(
                "min_duty must be within {MIN_DUTY}..={MAX_DUTY}"
            )));
        }
        let mut previous = floor;
        for step in &steps {
            if !(MIN_DUTY..=MAX_DUTY).contains(&step.duty) {
                return Err(CurveError(format!(
                    "curve duty {} is outside {MIN_DUTY}..={MAX_DUTY}",
                    step.duty
                )));
            }
            if step.duty <= previous {
                return Err(CurveError(format!(
                    "curve duties must rise strictly and exceed min_duty ({}), got {}",
                    floor, step.duty
                )));
            }
            previous = step.duty;
        }
        if hysteresis.is_nan() || !(0.0..=20.0).contains(&hysteresis) {
            return Err(CurveError("hysteresis must be within 0..=20".into()));
        }
        if drop_step == 0 {
            return Err(CurveError("drop_step must be at least 1".into()));
        }
        Ok(Curve {
            steps,
            floor,
            hysteresis,
            drop_step,
            drop_interval,
        })
    }

    /// The built-in curve: a conservative policy for the PR2100.
    pub fn default_steps() -> Vec<Step> {
        [
            (45, 50.0, 40.0, 45.0),
            (60, 60.0, 44.0, 50.0),
            (80, 70.0, 48.0, 55.0),
            (99, 80.0, 52.0, 60.0),
        ]
        .into_iter()
        .map(|(duty, cpu, disk, pmc)| Step {
            duty,
            cpu,
            disk,
            pmc,
        })
        .collect()
    }

    /// Lowest duty this curve produces.
    pub fn floor(&self) -> u8 {
        self.floor
    }

    /// True while the hottest sensor is at or above the top curve step.
    ///
    /// `active` says whether the condition was already reported: once it is,
    /// the temperatures must fall `hysteresis` degrees below the limit before
    /// it clears, so the front LED does not flicker around the threshold.
    pub fn over_temperature(&self, temps: &Temps, active: bool) -> bool {
        let Some(top) = self.steps.last() else {
            return false;
        };
        let margin = if active { self.hysteresis } else { 0.0 };
        temps.cpu >= top.cpu - margin
            || temps.pmc >= top.pmc - margin
            || temps.disk.is_some_and(|d| d >= top.disk - margin)
    }

    /// The duty demanded by `temps` if each limit is lowered by `margin`.
    pub fn requested(&self, temps: &Temps, margin: f64) -> u8 {
        let mut result = self.floor;
        for step in &self.steps {
            let hot = temps.cpu >= step.cpu - margin
                || temps.pmc >= step.pmc - margin
                || temps.disk.is_some_and(|d| d >= step.disk - margin);
            if hot {
                result = step.duty;
            }
        }
        result
    }
}

/// Tracks the current duty and applies the drop policy.
#[derive(Debug)]
pub struct Controller {
    curve: Curve,
    current: u8,
    last_drop: Option<Duration>,
}

impl Controller {
    /// Starts at `initial` (normally the safe maximum).
    pub fn new(curve: Curve, initial: u8) -> Controller {
        Controller {
            curve,
            current: initial,
            last_drop: None,
        }
    }

    /// Replaces the curve (configuration reload) and keeps the current duty.
    pub fn set_curve(&mut self, curve: Curve) {
        self.curve = curve;
    }

    /// Chooses the next duty. `now` is any monotonic offset.
    pub fn decide(&mut self, temps: &Temps, now: Duration) -> u8 {
        let target = self.curve.requested(temps, 0.0);
        if target > self.current {
            self.current = target;
            self.last_drop = Some(now);
            return self.current;
        }
        let stable = self.curve.requested(temps, self.curve.hysteresis);
        let due = self
            .last_drop
            .is_none_or(|t| now.saturating_sub(t) >= self.curve.drop_interval);
        if stable < self.current && due {
            self.current = stable
                .max(self.current.saturating_sub(self.curve.drop_step))
                .max(self.curve.floor);
            self.last_drop = Some(now);
        }
        self.current
    }

    /// Forces `duty` after a fault and restarts the drop timer.
    pub fn fail_high(&mut self, duty: u8, now: Duration) -> u8 {
        self.current = duty;
        self.last_drop = Some(now);
        duty
    }
}
