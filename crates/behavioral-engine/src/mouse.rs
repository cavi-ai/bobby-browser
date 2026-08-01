//! Bezier curve mouse movement simulator.
//!
//! Generates natural, non-linear mouse paths using cubic Bezier curves
//! with randomized control points, speed, and acceleration.

use rand::Rng;
use serde::{Deserialize, Serialize};
use std::time::Duration;

use crate::SessionRandom;

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
}

impl Default for MouseConfig {
    fn default() -> Self {
        Self {
            min_duration_ms: default_move_min_ms(),
            max_duration_ms: default_move_max_ms(),
            control_point_variance: default_control_point_variance(),
            curve_samples: default_curve_samples(),
            acceleration_curve: default_acceleration_curve(),
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
}

/// Bezier curve mouse simulator.
pub struct BezierMouseSimulator {
    config: MouseConfig,
}

impl BezierMouseSimulator {
    pub fn new(config: MouseConfig) -> Self {
        Self { config }
    }

    pub fn with_config(mut self, config: MouseConfig) -> Self {
        self.config = config;
        self
    }

    /// Generate a Bezier curve mouse path from (x1, y1) to (x2, y2).
    pub fn generate_path(
        &self,
        random: &mut SessionRandom,
        start_x: f64,
        start_y: f64,
        end_x: f64,
        end_y: f64,
    ) -> MousePath {
        let rng = &mut random.rng;

        // Generate randomized control points for natural curve
        let dx = (end_x - start_x).abs();
        let dy = (end_y - start_y).abs();
        let max_offset = (dx.max(dy) * self.config.control_point_variance).max(50.0);

        let cp1_x = self.control_point(start_x, end_x, max_offset, rng);
        let cp1_y = self.control_point(start_y, end_y, max_offset, rng);
        let cp2_x = self.control_point(start_x, end_x, max_offset, rng);
        let cp2_y = self.control_point(start_y, end_y, max_offset, rng);

        // Ensure control points don't create backward movements
        let cp1_x = self.clamp_control(cp1_x, start_x, end_x);
        let cp1_y = self.clamp_control(cp1_y, start_y, end_y);
        let cp2_x = self.clamp_control(cp2_x, start_x, end_x);
        let cp2_y = self.clamp_control(cp2_y, start_y, end_y);

        let duration_ms = random.next_duration(
            Duration::from_millis(self.config.min_duration_ms),
            Duration::from_millis(self.config.max_duration_ms),
        );

        let samples = self.config.curve_samples;
        let acceleration = self.config.acceleration_curve;

        let points = (0..=samples)
            .map(|i| {
                let t = i as f64 / samples as f64;
                // Apply acceleration curve for natural start/stop
                let accel_t = self.accelerate(t, acceleration);
                let (x, y) = self.bezier_point(
                    start_x, start_y, cp1_x, cp1_y, cp2_x, cp2_y, end_x, end_y, accel_t,
                );
                let timestamp_ms = (accel_t * duration_ms.as_millis() as f64) as u64;
                MousePoint {
                    x,
                    y,
                    timestamp_ms,
                }
            })
            .collect();

        MousePath {
            points,
            duration_ms: duration_ms.as_millis() as u64,
        }
    }

    fn control_point(&self, start: f64, end: f64, max_offset: f64, rng: &mut impl Rng) -> f64 {
        let mid = (start + end) / 2.0;
        let offset = rng.random_range(-max_offset..max_offset);
        mid + offset
    }

    fn clamp_control(&self, cp: f64, start: f64, end: f64) -> f64 {
        let min = start.min(end);
        let max = start.max(end);
        cp.clamp(min - (max - min) * 0.2, max + (max - min) * 0.2)
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
