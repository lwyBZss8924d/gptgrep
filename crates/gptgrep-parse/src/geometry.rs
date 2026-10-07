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
