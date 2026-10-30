//! Color matrices between camera, the Rec.2020 working space and sRGB.

pub type Mat3 = [[f32; 3]; 3];

/// Linear Rec.2020 (D65) to XYZ.
const REC2020_TO_XYZ: Mat3 = [
    [0.636_958, 0.144_617, 0.168_881],
    [0.262_700, 0.677_998, 0.059_302],
    [0.000_000, 0.028_073, 1.060_985],
];

/// Linear Rec.2020 to linear sRGB, both D65.
pub const REC2020_TO_SRGB: Mat3 = [
    [1.660_491, -0.587_641, -0.072_850],
    [-0.124_55, 1.1329, -0.008_349],
    [-0.018_151, -0.100_579, 1.11873],
];

pub fn mul(a: Mat3, b: Mat3) -> Mat3 {
    let mut out = [[0.0; 3]; 3];
    for (row, line) in out.iter_mut().enumerate() {
        for (col, value) in line.iter_mut().enumerate() {
            *value = (0..3).map(|k| a[row][k] * b[k][col]).sum();
        }
    }
    out
}

#[inline]
pub fn apply(m: &Mat3, p: [f32; 3]) -> [f32; 3] {
    [
        m[0][0] * p[0] + m[0][1] * p[1] + m[0][2] * p[2],
        m[1][0] * p[0] + m[1][1] * p[1] + m[1][2] * p[2],
        m[2][0] * p[0] + m[2][1] * p[1] + m[2][2] * p[2],
    ]
}

pub fn invert(m: Mat3) -> Option<Mat3> {
    let det = m[0][0] * (m[1][1] * m[2][2] - m[1][2] * m[2][1])
        - m[0][1] * (m[1][0] * m[2][2] - m[1][2] * m[2][0])
        + m[0][2] * (m[1][0] * m[2][1] - m[1][1] * m[2][0]);
    if det.abs() < 1e-9 || !det.is_finite() {
        return None;
    }
    let d = 1.0 / det;
    Some([
        [
            (m[1][1] * m[2][2] - m[1][2] * m[2][1]) * d,
            (m[0][2] * m[2][1] - m[0][1] * m[2][2]) * d,
            (m[0][1] * m[1][2] - m[0][2] * m[1][1]) * d,
        ],
        [
            (m[1][2] * m[2][0] - m[1][0] * m[2][2]) * d,
            (m[0][0] * m[2][2] - m[0][2] * m[2][0]) * d,
            (m[0][2] * m[1][0] - m[0][0] * m[1][2]) * d,
        ],
        [
            (m[1][0] * m[2][1] - m[1][1] * m[2][0]) * d,
            (m[0][1] * m[2][0] - m[0][0] * m[2][1]) * d,
            (m[0][0] * m[1][1] - m[0][1] * m[1][0]) * d,
        ],
    ])
}

/// The matrix from white-balanced camera RGB to Rec.2020, and the daylight white
/// balance implied by the camera's matrix. Rows of the camera-from-working matrix
/// are normalized so white-balanced camera white lands exactly on working white.
pub fn camera_to_working(xyz_to_cam: Mat3) -> Option<(Mat3, [f32; 3])> {
    let mut cam_from_working = mul(xyz_to_cam, REC2020_TO_XYZ);
    let mut daylight = [1.0; 3];
    for (row, line) in cam_from_working.iter_mut().enumerate() {
        let sum: f32 = line.iter().sum();
        if sum.abs() < 1e-6 || !sum.is_finite() {
            return None;
        }
        for value in line.iter_mut() {
            *value /= sum;
        }
        daylight[row] = 1.0 / sum;
    }
    let green = daylight[1];
    Some((
        invert(cam_from_working)?,
        [daylight[0] / green, 1.0, daylight[2] / green],
    ))
}

/// The sRGB transfer function.
#[inline]
pub fn srgb_encode(linear: f32) -> f32 {
    if linear <= 0.003_130_8 {
        12.92 * linear.max(0.0)
    } else {
        1.055 * linear.min(1.0).powf(1.0 / 2.4) - 0.055
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f32, b: f32, tolerance: f32) -> bool {
        (a - b).abs() <= tolerance
    }

    #[test]
    fn invert_undoes_a_matrix() {
        let inverse = invert(REC2020_TO_SRGB).unwrap();
        let identity = mul(REC2020_TO_SRGB, inverse);
        for (row, line) in identity.iter().enumerate() {
            for (col, value) in line.iter().enumerate() {
                let want = if row == col { 1.0 } else { 0.0 };
                assert!(close(*value, want, 1e-5));
            }
        }
    }

    #[test]
    fn singular_matrix_is_refused() {
        assert!(invert([[1.0, 2.0, 3.0], [2.0, 4.0, 6.0], [1.0, 1.0, 1.0]]).is_none());
        assert!(camera_to_working([[0.0; 3]; 3]).is_none());
    }

    #[test]
    fn working_white_stays_white_in_srgb() {
        let [r, g, b] = apply(&REC2020_TO_SRGB, [1.0, 1.0, 1.0]);
        assert!(close(r, 1.0, 1e-3) && close(g, 1.0, 1e-3) && close(b, 1.0, 1e-3));
    }

    #[test]
    fn balanced_camera_white_lands_on_working_white() {
        // Canon EOS 5D Mark IV style matrix: any plausible camera works here.
        let xyz_to_cam = [
            [0.6446, -0.0366, -0.0864],
            [-0.4436, 1.2204, 0.2513],
            [-0.0952, 0.2496, 0.6348],
        ];
        let (to_working, _) = camera_to_working(xyz_to_cam).unwrap();
        let [r, g, b] = apply(&to_working, [1.0, 1.0, 1.0]);
        assert!(close(r, 1.0, 1e-3) && close(g, 1.0, 1e-3) && close(b, 1.0, 1e-3));
    }

    #[test]
    fn srgb_encoding_matches_known_points() {
        assert!(close(srgb_encode(0.0), 0.0, 1e-6));
        assert!(close(srgb_encode(1.0), 1.0, 1e-5));
        assert!(close(srgb_encode(0.18), 0.4613, 1e-3));
        assert!(close(srgb_encode(-1.0), 0.0, 1e-6));
        assert!(close(srgb_encode(9.0), 1.0, 1e-5));
    }
}
