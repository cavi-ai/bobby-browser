//! Bezier curve mouse movement simulator.
//!
//! Generates natural, non-linear mouse paths using cubic Bezier curves
//! with randomized control points, speed, and acceleration.

use serde::{Deserialize, Serialize};
use std::time::Duration;

use crate::{order_u64, SessionRandom};

/// A point on the mouse path.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MousePoint {
    pub x: f64,
    pub y: f64,
    pub timestamp_ms: u64,
}

/// A complete mouse movement path.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MousePath {
    pub points: Vec<MousePoint>,
    pub duration_ms: u64,
}

/// Configuration for mouse movement simulation.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MouseConfig {
    #[serde(default = "default_move_min_ms")]
    pub min_duration_ms: u64,
    #[serde(default = "default_move_max_ms")]
    pub max_duration_ms: u64,
    #[serde(default = "default_control_point_variance")]
    pub control_point_variance: f64,
    #[serde(default = "default_curve_samples")]
    pub curve_samples: usize,
    #[serde(default = "default_acceleration_curve")]
    pub acceleration_curve: f64,
    /// Probability of a small overshoot past the target before settling.
    #[serde(default = "default_overshoot_probability")]
    pub overshoot_probability: f64,
}

impl Default for MouseConfig {
    fn default() -> Self {
        Self {
            min_duration_ms: default_move_min_ms(),
            max_duration_ms: default_move_max_ms(),
            control_point_variance: default_control_point_variance(),
            curve_samples: default_curve_samples(),
            acceleration_curve: default_acceleration_curve(),
            overshoot_probability: default_overshoot_probability(),
        }
    }
}

fn default_move_min_ms() -> u64 {
    80
}

fn default_move_max_ms() -> u64 {
    450
}

fn default_control_point_variance() -> f64 {
    0.3
}

fn default_curve_samples() -> usize {
    12
}

fn default_acceleration_curve() -> f64 {
    1.2
}

fn default_overshoot_probability() -> f64 {
    0.18
}

impl MouseConfig {
    pub fn with_min_duration(mut self, ms: u64) -> Self {
        self.min_duration_ms = ms;
        self
    }

    pub fn with_max_duration(mut self, ms: u64) -> Self {
        self.max_duration_ms = ms;
        self
    }

    pub fn with_control_variance(mut self, variance: f64) -> Self {
        self.control_point_variance = variance;
        self
    }

    pub fn with_curve_samples(mut self, samples: usize) -> Self {
        self.curve_samples = samples;
        self
    }

    pub fn with_acceleration(mut self, curve: f64) -> Self {
        self.acceleration_curve = curve;
        self
    }

    pub fn with_overshoot_probability(mut self, probability: f64) -> Self {
        self.overshoot_probability = probability;
        self
    }

    pub fn sanitize(mut self) -> Self {
        let (min, max) = order_u64(self.min_duration_ms, self.max_duration_ms);
        self.min_duration_ms = min;
        self.max_duration_ms = max.max(min + 1);
        self.control_point_variance = self.control_point_variance.clamp(0.0, 2.0);
        if !self.control_point_variance.is_finite() {
            self.control_point_variance = default_control_point_variance();
        }
        self.curve_samples = self.curve_samples.clamp(3, 64);
        if !self.acceleration_curve.is_finite() || self.acceleration_curve <= 0.0 {
            self.acceleration_curve = default_acceleration_curve();
        }
        self.overshoot_probability = crate::clamp_probability(self.overshoot_probability);
        self
    }
}

/// Bezier curve mouse simulator.
pub struct BezierMouseSimulator {
    config: MouseConfig,
}

impl BezierMouseSimulator {
    pub fn new(config: MouseConfig) -> Self {
        Self {
            config: config.sanitize(),
        }
    }

    pub fn with_config(mut self, config: MouseConfig) -> Self {
        self.config = config.sanitize();
        self
    }

    /// Generate a Bezier curve mouse path from (x1, y1) to (x2, y2).
    ///
    /// Duration scales with distance (Fitts-like). Optional overshoot settles on the target.
    pub fn generate_path(
        &self,
        random: &mut SessionRandom,
        start_x: f64,
        start_y: f64,
        end_x: f64,
        end_y: f64,
    ) -> MousePath {
        let dx = end_x - start_x;
        let dy = end_y - start_y;
        let distance = (dx * dx + dy * dy).sqrt().max(1.0);

        let max_offset = (distance * self.config.control_point_variance).max(50.0);
        let cp1_x = self.clamp_control(start_x + dx * 0.3 + random.offset(max_offset), start_x, end_x);
        let cp1_y = self.clamp_control(start_y + dy * 0.3 + random.offset(max_offset), start_y, end_y);
        let cp2_x = self.clamp_control(start_x + dx * 0.7 + random.offset(max_offset), start_x, end_x);
        let cp2_y = self.clamp_control(start_y + dy * 0.7 + random.offset(max_offset), start_y, end_y);

        // Fitts-inspired duration: longer travels take longer, still within configured band.
        let (min_ms, max_ms) = order_u64(self.config.min_duration_ms, self.config.max_duration_ms);
        let span = (max_ms - min_ms) as f64;
        let distance_factor = (distance / 900.0).clamp(0.0, 1.0);
        let target_ms = min_ms as f64 + span * (0.25 + 0.75 * distance_factor);
        let jitter = random.next_f64(0.85, 1.15);
        let duration_ms = ((target_ms * jitter) as u64).clamp(min_ms, max_ms);
        let duration = Duration::from_millis(duration_ms);

        let samples = ((distance / 40.0) as usize)
            .clamp(3, self.config.curve_samples.max(3));
        let acceleration = self.config.acceleration_curve;

        let mut points: Vec<MousePoint> = (0..=samples)
            .map(|i| {
                let t = i as f64 / samples as f64;
                let accel_t = self.accelerate(t, acceleration);
                let (x, y) = self.bezier_point(
                    start_x, start_y, cp1_x, cp1_y, cp2_x, cp2_y, end_x, end_y, accel_t,
                );
                // Wall-clock timestamps follow sample index so move durations stay monotonic.
                let timestamp_ms = (t * duration.as_millis() as f64) as u64;
                MousePoint {
                    x,
                    y,
                    timestamp_ms,
                }
            })
            .collect();

        if let Some(last) = points.last_mut() {
            last.x = end_x;
            last.y = end_y;
            last.timestamp_ms = duration_ms;
        }

        if random.chance(self.config.overshoot_probability) && distance > 20.0 {
            let overshoot_scale = random.next_f64(0.02, 0.08);
            let ox = end_x + dx.signum() * distance * overshoot_scale + random.offset(4.0);
            let oy = end_y + dy.signum() * distance * overshoot_scale + random.offset(4.0);
            let overshoot_at = duration_ms.saturating_add(random.gen_u32(20, 60) as u64);
            let settle_at = overshoot_at.saturating_add(random.gen_u32(30, 90) as u64);
            points.push(MousePoint {
                x: ox,
                y: oy,
                timestamp_ms: overshoot_at,
            });
            points.push(MousePoint {
                x: end_x,
                y: end_y,
                timestamp_ms: settle_at,
            });
            return MousePath {
                points,
                duration_ms: settle_at,
            };
        }

        MousePath {
            points,
            duration_ms,
        }
    }

    /// Path from a randomized approach offset to the element-local origin `(0, 0)`.
    pub fn generate_approach_path(&self, random: &mut SessionRandom) -> MousePath {
        let start_x = random.next_f64(-180.0, 180.0);
        let start_y = random.next_f64(-140.0, 140.0);
        // Avoid near-zero starts that collapse the path.
        let start_x = if start_x.abs() < 24.0 {
            start_x.signum() * 24.0 + random.next_f64(10.0, 40.0)
        } else {
            start_x
        };
        let start_y = if start_y.abs() < 24.0 {
            start_y.signum() * 24.0 + random.next_f64(10.0, 40.0)
        } else {
            start_y
        };
        self.generate_path(random, start_x, start_y, 0.0, 0.0)
    }

    fn clamp_control(&self, cp: f64, start: f64, end: f64) -> f64 {
        let min = start.min(end);
        let max = start.max(end);
        let pad = (max - min) * 0.2;
        cp.clamp(min - pad, max + pad)
    }

    fn bezier_point(
        &self,
        x0: f64,
        y0: f64,
        x1: f64,
        y1: f64,
        x2: f64,
        y2: f64,
        x3: f64,
        y3: f64,
        t: f64,
    ) -> (f64, f64) {
        let u = 1.0 - t;
        let tt = t * t;
        let uu = u * u;
        let uuu = uu * u;
        let ttt = tt * t;

        let x = uuu * x0 + 3.0 * uu * t * x1 + 3.0 * u * tt * x2 + ttt * x3;
        let y = uuu * y0 + 3.0 * uu * t * y1 + 3.0 * u * tt * y2 + ttt * y3;

        (x, y)
    }

    fn accelerate(&self, t: f64, curve: f64) -> f64 {
        if t <= 0.0 {
            return 0.0;
        }
        if t >= 1.0 {
            return 1.0;
        }
        t.powf(curve) / (t.powf(curve) + (1.0 - t).powf(curve))
    }
}
