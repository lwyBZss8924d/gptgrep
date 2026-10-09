use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

/// x'=a*x+c*y+e, y'=b*x+d*y+f, in explicit source/destination frames.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AffineTransform {
    pub a: f64,
    pub b: f64,
    pub c: f64,
    pub d: f64,
    pub e: f64,
    pub f: f64,
}

impl AffineTransform {
    pub const IDENTITY: Self = Self {
        a: 1.0,
        b: 0.0,
        c: 0.0,
        d: 1.0,
        e: 0.0,
        f: 0.0,
    };

    pub fn apply(&self, x: f64, y: f64) -> (f64, f64) {
        (
            self.a * x + self.c * y + self.e,
            self.b * x + self.d * y + self.f,
        )
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(
            [self.a, self.b, self.c, self.d, self.e, self.f]
                .iter()
                .all(|v| v.is_finite()),
            "invalid affine coefficients"
        );
        let determinant = self.a * self.d - self.b * self.c;
        ensure!(
            determinant.is_finite() && determinant.abs() > 1e-12,
            "noninvertible affine transform"
        );
        Ok(())
    }

    pub fn inverse(&self) -> Result<Self> {
        self.validate()?;
        let det = self.a * self.d - self.b * self.c;
        let inverse = Self {
            a: self.d / det,
            b: -self.b / det,
            c: -self.c / det,
            d: self.a / det,
            e: (self.c * self.f - self.d * self.e) / det,
            f: (self.b * self.e - self.a * self.f) / det,
        };
        inverse.validate()?;
        Ok(inverse)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BoundingBox {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl BoundingBox {
    pub fn validate(&self, page_width: f64, page_height: f64) -> Result<()> {
        ensure!(
            [
                self.x,
                self.y,
                self.width,
                self.height,
                page_width,
                page_height
            ]
            .iter()
            .all(|v| v.is_finite()),
            "invalid bounding-box values"
        );
        ensure!(
            self.x >= 0.0
                && self.y >= 0.0
                && self.width > 0.0
                && self.height > 0.0
                && self.x + self.width <= page_width + 0.001
                && self.y + self.height <= page_height + 0.001,
            "bounding box lies outside page"
        );
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PageGeometry {
    pub width_points: f64,
    pub height_points: f64,
    pub rotation_quarter_turns: i32,
    pub user_unit: f64,
    pub source_to_page: AffineTransform,
    pub page_to_source: AffineTransform,
}

impl PageGeometry {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.width_points.is_finite()
                && self.width_points > 0.0
                && self.height_points.is_finite()
                && self.height_points > 0.0
                && self.user_unit.is_finite()
                && self.user_unit > 0.0
                && (0..=3).contains(&self.rotation_quarter_turns),
            "invalid page geometry"
        );
        let inverse = self.source_to_page.inverse()?;
        ensure!(
            self.page_to_source == inverse,
            "page inverse transform mismatch"
        );
        Ok(())
    }

    /// Recipe-specific PDF frame qualification. Generic image/source frames
    /// retain `validate`; a PDF viewport additionally rotates and flips Y.
    pub(crate) fn validate_native_pdf(&self) -> Result<()> {
        self.validate()?;
        let t = self.source_to_page;
        let observed = [
            self.width_points,
            self.height_points,
            self.user_unit,
            t.a,
            t.b,
            t.c,
            t.d,
            t.e,
            t.f,
        ];
        ensure!(
            observed
                .iter()
                .all(|v| (*v as f32).is_finite() && f64::from(*v as f32) == *v),
            "native PDF frame is not an f32 adapter observation"
        );
        let u = self.user_unit;
        let expected = match self.rotation_quarter_turns {
            0 => [u, 0.0, 0.0, -u],
            1 => [0.0, u, u, 0.0],
            2 => [-u, 0.0, 0.0, u],
            3 => [0.0, -u, -u, 0.0],
            _ => unreachable!("validated rotation"),
        };
        // Match page_to_viewport's f32 device-box multiplier. The slope is
        // recovered by subtracting two integer-device probes, then rich.rs
        // probes/subtracts that f32 affine map once more. Two device quanta,
        // device-box rounding relative to this axis extent, and eight f32
        // epsilons at the translated-coordinate scale conservatively cover
        // that retained adapter's quantization and cancellation. This is not
        // a tolerance for arbitrary shear or a different quarter-turn.
        let mult = f64::from(
            1000.0_f32.min(
                (i32::MAX / 4) as f32
                    / (self.width_points as f32)
                        .max(self.height_points as f32)
                        .max(1.0),
            ),
        );
        let error = |extent: f64, offset: f64| {
            2.0 / mult
                + u / (mult * extent)
                + 8.0 * f64::from(f32::EPSILON) * (offset.abs() + u + 1.0)
        };
        let x_error = error(self.width_points, t.e);
        let y_error = error(self.height_points, t.f);
        // Require at least six binary bits of slope discrimination. At poorer
        // precision the declaration cannot be qualified by this adapter; fail
        // instead of widening the tolerance to accept a materially wrong unit.
        ensure!(
            x_error.is_finite()
                && y_error.is_finite()
                && x_error <= u / 64.0
                && y_error <= u / 64.0,
            "native PDF viewport precision cannot qualify declared UserUnit"
        );
        for ((actual, ideal), tolerance) in [t.a, t.b, t.c, t.d]
            .into_iter()
            .zip(expected)
            .zip([x_error, y_error, x_error, y_error])
        {
            ensure!(
                if ideal == 0.0 {
                    actual == 0.0
                } else {
                    actual.signum() == ideal.signum() && (actual - ideal).abs() <= tolerance
                },
                "native PDF viewport contradicts declared rotation or UserUnit"
            );
        }
        // Translation has no independently retained CropBox to compare with.
        // This check does not authenticate the original source's crop edges.
        Ok(())
    }
}
