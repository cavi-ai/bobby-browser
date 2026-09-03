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
    /// Pause after arriving, before click (hover / aim settle).
    #[serde(default)]
    pub hover_dwell_ms: u64,
}

impl MousePath {
    /// Keep every sample inside element-origin bounds that stay on-viewport.
    ///
    /// Firefox BiDi rejects `pointerMove` with `origin: element` when the
    /// converted viewport coordinate is negative or past the far edge.
    pub fn clamp_element_origin(&self, min_x: f64, max_x: f64, min_y: f64, max_y: f64) -> Self {
        let min_x = min_x.min(0.0);
        let max_x = max_x.max(0.0);
        let min_y = min_y.min(0.0);
        let max_y = max_y.max(0.0);
        Self {
            points: self
                .points
                .iter()
                .map(|point| MousePoint {
                    x: point.x.clamp(min_x, max_x),
                    y: point.y.clamp(min_y, max_y),
                    timestamp_ms: point.timestamp_ms,
                })
                .collect(),
            duration_ms: self.duration_ms,
            hover_dwell_ms: self.hover_dwell_ms,
        }
    }
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
    /// Random landing offset from the exact target (px).
    #[serde(default = "default_landing_jitter_px")]
    pub landing_jitter_px: f64,
    /// Per-sample path noise (px), excluding endpoints.
    #[serde(default = "default_path_noise_px")]
    pub path_noise_px: f64,
    #[serde(default = "default_hover_dwell_min_ms")]
    pub hover_dwell_min_ms: u64,
    #[serde(default = "default_hover_dwell_max_ms")]
    pub hover_dwell_max_ms: u64,
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
            landing_jitter_px: default_landing_jitter_px(),
            path_noise_px: default_path_noise_px(),
            hover_dwell_min_ms: default_hover_dwell_min_ms(),
            hover_dwell_max_ms: default_hover_dwell_max_ms(),
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

fn default_landing_jitter_px() -> f64 {
    3.0
}

fn default_path_noise_px() -> f64 {
    1.2
}

fn default_hover_dwell_min_ms() -> u64 {
    35
}

fn default_hover_dwell_max_ms() -> u64 {
    140
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

    pub fn with_landing_jitter(mut self, px: f64) -> Self {
        self.landing_jitter_px = px;
        self
    }

    pub fn with_path_noise(mut self, px: f64) -> Self {
        self.path_noise_px = px;
        self
    }

    pub fn with_hover_dwell_range(mut self, min_ms: u64, max_ms: u64) -> Self {
        self.hover_dwell_min_ms = min_ms;
        self.hover_dwell_max_ms = max_ms;
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
        if !self.landing_jitter_px.is_finite() || self.landing_jitter_px < 0.0 {
            self.landing_jitter_px = 0.0;
        }
        self.landing_jitter_px = self.landing_jitter_px.min(24.0);
        if !self.path_noise_px.is_finite() || self.path_noise_px < 0.0 {
            self.path_noise_px = 0.0;
        }
        self.path_noise_px = self.path_noise_px.min(8.0);
        let (dmin, dmax) = order_u64(self.hover_dwell_min_ms, self.hover_dwell_max_ms);
        self.hover_dwell_min_ms = dmin;
        self.hover_dwell_max_ms = dmax.max(dmin);
        self
    }
}

fn finite_or(value: f64, fallback: f64) -> f64 {
    if value.is_finite() {
        value
    } else {
        fallback
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
        let start_x = finite_or(start_x, 0.0);
        let start_y = finite_or(start_y, 0.0);
        let end_x = finite_or(end_x, 0.0);
        let end_y = finite_or(end_y, 0.0);

        let land_x = end_x + random.offset(self.config.landing_jitter_px);
        let land_y = end_y + random.offset(self.config.landing_jitter_px);

        let dx = land_x - start_x;
        let dy = land_y - start_y;
        let distance = (dx * dx + dy * dy).sqrt().max(1.0);

        let max_offset = (distance * self.config.control_point_variance).max(50.0);
        let cp1_x = self.clamp_control(
            start_x + dx * 0.3 + random.offset(max_offset),
            start_x,
            land_x,
        );
        let cp1_y = self.clamp_control(
            start_y + dy * 0.3 + random.offset(max_offset),
            start_y,
            land_y,
        );
        let cp2_x = self.clamp_control(
            start_x + dx * 0.7 + random.offset(max_offset),
            start_x,
            land_x,
        );
        let cp2_y = self.clamp_control(
            start_y + dy * 0.7 + random.offset(max_offset),
            start_y,
            land_y,
        );

        let (min_ms, max_ms) = order_u64(self.config.min_duration_ms, self.config.max_duration_ms);
        let span = (max_ms - min_ms) as f64;
        let distance_factor = (distance / 900.0).clamp(0.0, 1.0);
        let target_ms = min_ms as f64 + span * (0.25 + 0.75 * distance_factor);
        let jitter = random.next_f64(0.85, 1.15);
        let duration_ms = ((target_ms * jitter) as u64).clamp(min_ms, max_ms);
        let duration = Duration::from_millis(duration_ms);

        let samples = ((distance / 40.0) as usize).clamp(3, self.config.curve_samples.max(3));
        let acceleration = self.config.acceleration_curve;
        let noise = self.config.path_noise_px;

        let mut points: Vec<MousePoint> = (0..=samples)
            .map(|i| {
                let t = i as f64 / samples as f64;
                let accel_t = self.accelerate(t, acceleration);
                let (mut x, mut y) = self.bezier_point(
                    (start_x, start_y),
                    (cp1_x, cp1_y),
                    (cp2_x, cp2_y),
                    (land_x, land_y),
                    accel_t,
                );
                // Keep endpoints exact; add tremor only on intermediate samples.
                if i > 0 && i < samples && noise > 0.0 {
                    x += random.offset(noise);
                    y += random.offset(noise);
                }
                let timestamp_ms = (t * duration.as_millis() as f64) as u64;
                MousePoint { x, y, timestamp_ms }
            })
            .collect();

        if let Some(first) = points.first_mut() {
            first.x = start_x;
            first.y = start_y;
            first.timestamp_ms = 0;
        }
        if let Some(last) = points.last_mut() {
            last.x = land_x;
            last.y = land_y;
            last.timestamp_ms = duration_ms;
        }

        let mut duration_ms = duration_ms;
        if random.chance(self.config.overshoot_probability) && distance > 20.0 {
            let overshoot_scale = random.next_f64(0.02, 0.08);
            let ox = land_x + dx.signum() * distance * overshoot_scale + random.offset(4.0);
            let oy = land_y + dy.signum() * distance * overshoot_scale + random.offset(4.0);
            let overshoot_at = duration_ms.saturating_add(random.gen_u32(20, 60) as u64);
            let settle_at = overshoot_at.saturating_add(random.gen_u32(30, 90) as u64);
            points.push(MousePoint {
                x: ox,
                y: oy,
                timestamp_ms: overshoot_at,
            });
            points.push(MousePoint {
                x: land_x,
                y: land_y,
                timestamp_ms: settle_at,
            });
            duration_ms = settle_at;
        }

        let hover_dwell_ms = if self.config.hover_dwell_max_ms == 0 {
            0
        } else {
            random
                .next_duration(
                    Duration::from_millis(self.config.hover_dwell_min_ms),
                    Duration::from_millis(
                        self.config
                            .hover_dwell_max_ms
                            .max(self.config.hover_dwell_min_ms + 1),
                    ),
                )
                .as_millis() as u64
        };

        MousePath {
            points,
            duration_ms,
            hover_dwell_ms,
        }
    }

    /// Path from a randomized approach offset to a jittered element-local origin.
    ///
    /// Vertical approach is biased from below the target so BiDi element-origin
    /// moves stay inside the viewport after `scrollIntoView` places the target
    /// near the top/middle of the window.
    pub fn generate_approach_path(&self, random: &mut SessionRandom) -> MousePath {
        let mut start_x = random.next_f64(-140.0, 140.0);
        if start_x.abs() < 28.0 {
            let sign = if start_x >= 0.0 { 1.0 } else { -1.0 };
            start_x = sign * 28.0 + random.next_f64(8.0, 36.0) * sign;
        }
        // Prefer approach from below (positive element-local Y). Negative Y
        // frequently exits the viewport when the target sits near the top edge.
        let start_y = random.next_f64(36.0, 110.0);
        self.generate_path(random, start_x, start_y, 0.0, 0.0)
    }

    fn clamp_control(&self, cp: f64, start: f64, end: f64) -> f64 {
        let min = start.min(end);
        let max = start.max(end);
        let pad = (max - min) * 0.2;
        finite_or(cp, (min + max) / 2.0).clamp(min - pad, max + pad)
    }

    fn bezier_point(
        &self,
        (x0, y0): (f64, f64),
        (x1, y1): (f64, f64),
        (x2, y2): (f64, f64),
        (x3, y3): (f64, f64),
        t: f64,
    ) -> (f64, f64) {
        let u = 1.0 - t;
        let tt = t * t;
        let uu = u * u;
        let uuu = uu * u;
        let ttt = tt * t;

        let x = uuu * x0 + 3.0 * uu * t * x1 + 3.0 * u * tt * x2 + ttt * x3;
        let y = uuu * y0 + 3.0 * uu * t * y1 + 3.0 * u * tt * y2 + ttt * y3;

        (finite_or(x, x3), finite_or(y, y3))
    }

    fn accelerate(&self, t: f64, curve: f64) -> f64 {
        if t <= 0.0 {
            return 0.0;
        }
        if t >= 1.0 {
            return 1.0;
        }
        let numerator = t.powf(curve);
        let denom = numerator + (1.0 - t).powf(curve);
        if denom == 0.0 || !denom.is_finite() {
            return t;
        }
        let value = numerator / denom;
        finite_or(value, t)
    }
}
