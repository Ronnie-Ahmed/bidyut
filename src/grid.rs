const LAT_MIN: f64 = 20.5;
const LAT_MAX: f64 = 26.7;
const LNG_MIN: f64 = 88.0;
const LNG_MAX: f64 = 92.7;

/// 0.01 degrees is roughly 1.1 km, so 100 cells per degree.
const CELLS_PER_DEGREE: f64 = 100.0;

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub struct CellIndex {
    pub lat_idx: i32,
    pub lng_idx: i32,
}

pub fn in_bangladesh(lat: f64, lng: f64) -> bool {
    (LAT_MIN..=LAT_MAX).contains(&lat) && (LNG_MIN..=LNG_MAX).contains(&lng)
}

pub fn cell_for(lat: f64, lng: f64) -> CellIndex {
    CellIndex {
        lat_idx: (lat * CELLS_PER_DEGREE).floor() as i32,
        lng_idx: (lng * CELLS_PER_DEGREE).floor() as i32,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dhaka_maps_to_expected_cell() {
        let cell = cell_for(23.8103, 90.4125);
        assert_eq!(
            cell,
            CellIndex {
                lat_idx: 2381,
                lng_idx: 9041
            }
        );
    }

    #[test]
    fn nearby_points_share_a_cell() {
        assert_eq!(cell_for(23.8103, 90.4125), cell_for(23.8150, 90.4180));
    }

    #[test]
    fn dhaka_is_inside_bangladesh() {
        assert!(in_bangladesh(23.8103, 90.4125));
    }

    #[test]
    fn outside_points_are_rejected() {
        assert!(!in_bangladesh(51.5074, -0.1278)); // London
        assert!(!in_bangladesh(0.0, 0.0));
    }
}
