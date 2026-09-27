//! Sun position and irradiance on tilted panels.
//!
//! Sun position uses PSA+ (Blanco, Milidonis & Bonanos, 2020), accurate to
//! well under a degree for 2020–2050, which is plenty for PV. Transposition
//! uses the Hay–Davies model: beam plus diffuse split into a circumsolar part
//! (following the beam) and an isotropic part, plus ground reflection.
//!
//! Angles are radians inside, degrees at the edges. Azimuths are compass
//! bearings: 0 = north, 90° = east, 180° = south.

use std::f64::consts::{PI, TAU};

use jiff::Timestamp;

/// Julian date of the J2000.0 epoch.
const J2000: f64 = 2_451_545.0;
/// Mean Earth radius over one astronomical unit, for parallax.
const PARALLAX: f64 = 6371.01 / 149_597_890.0;
/// Solar constant, W/m².
const SOLAR_CONSTANT: f64 = 1361.0;
/// Typical ground reflectance.
pub const ALBEDO: f64 = 0.2;

/// Where the sun is, seen from a place on Earth.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SunPosition {
    /// Angle from straight up, radians. Above 90° the sun is down.
    pub zenith: f64,
    /// Compass bearing, radians.
    pub azimuth: f64,
}

impl SunPosition {
    pub fn elevation_deg(self) -> f64 {
        90.0 - self.zenith.to_degrees()
    }

    pub fn is_up(self) -> bool {
        self.zenith < PI / 2.0
    }
}

/// The sun's position at `at` for a place at `latitude`/`longitude` (degrees).
pub fn sun_position(at: Timestamp, latitude: f64, longitude: f64) -> SunPosition {
    let julian_date = at.as_millisecond() as f64 / 86_400_000.0 + 2_440_587.5;
    let n = julian_date - J2000;
    let hours = (at.as_second().rem_euclid(86_400) as f64
        + f64::from(at.subsec_nanosecond()) * 1e-9)
        / 3600.0;

    // Ecliptic coordinates.
    let omega = 2.267_127_827 - 9.300_339_267e-4 * n;
    let mean_longitude = 4.895_036_035 + 1.720_279_602e-2 * n;
    let mean_anomaly = 6.239_468_336 + 1.720_200_135e-2 * n;
    let ecliptic_longitude = mean_longitude
        + 3.338_320_972e-2 * mean_anomaly.sin()
        + 3.497_596_876e-4 * (2.0 * mean_anomaly).sin()
        - 1.544_353_226e-4
        - 8.689_729_360e-6 * omega.sin();
    let obliquity = 4.090_904_909e-1 - 6.213_605_399e-9 * n + 4.418_094_944e-5 * omega.cos();

    // Celestial coordinates.
    let right_ascension = (obliquity.cos() * ecliptic_longitude.sin())
        .atan2(ecliptic_longitude.cos())
        .rem_euclid(TAU);
    let declination = (obliquity.sin() * ecliptic_longitude.sin()).asin();

    // Local coordinates.
    let gmst_hours = 6.697_096_103 + 6.570_984_737e-2 * n + hours;
    let local_sidereal = (gmst_hours * 15.0 + longitude).to_radians();
    let hour_angle = local_sidereal - right_ascension;
    let latitude = latitude.to_radians();
    let zenith = (latitude.cos() * hour_angle.cos() * declination.cos()
        + declination.sin() * latitude.sin())
    .acos();
    let azimuth = (-hour_angle.sin())
        .atan2(declination.tan() * latitude.cos() - latitude.sin() * hour_angle.cos())
        .rem_euclid(TAU);
    SunPosition {
        zenith: zenith + PARALLAX * zenith.sin(),
        azimuth,
    }
}

/// Irradiance on a horizontal surface and normal to the sun, W/m².
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Irradiance {
    /// Global horizontal.
    pub ghi: f64,
    /// Direct normal.
    pub dni: f64,
    /// Diffuse horizontal.
    pub dhi: f64,
}

/// A panel orientation, degrees.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Orientation {
    /// From horizontal.
    pub tilt: f64,
    /// Compass bearing the panel faces.
    pub azimuth: f64,
}

/// Extraterrestrial normal irradiance on the day of `at`, W/m².
pub fn extraterrestrial(at: Timestamp) -> f64 {
    let day_of_year = (at.as_second().rem_euclid(365 * 86_400) / 86_400) as f64;
    SOLAR_CONSTANT * (1.0 + 0.033 * (TAU * day_of_year / 365.0).cos())
}

/// Cosine of the angle between the sun and the panel's normal (may be negative).
pub fn cos_incidence(sun: SunPosition, panel: Orientation) -> f64 {
    let tilt = panel.tilt.to_radians();
    sun.zenith.cos() * tilt.cos()
        + sun.zenith.sin() * tilt.sin() * (sun.azimuth - panel.azimuth.to_radians()).cos()
}

/// Plane-of-array irradiance (Hay–Davies), W/m².
pub fn plane_of_array(
    sun: SunPosition,
    irradiance: Irradiance,
    panel: Orientation,
    extraterrestrial: f64,
) -> f64 {
    if !sun.is_up() {
        return 0.0;
    }
    let tilt = panel.tilt.to_radians();
    let cos_incidence = cos_incidence(sun, panel).max(0.0);
    // Guard against the sun at the horizon, as pvlib does.
    let cos_zenith = sun.zenith.cos().max(0.087_2);
    let beam = irradiance.dni * cos_incidence;
    let anisotropy = (irradiance.dni / extraterrestrial).clamp(0.0, 1.0);
    let diffuse = irradiance.dhi
        * (anisotropy * cos_incidence / cos_zenith + (1.0 - anisotropy) * (1.0 + tilt.cos()) / 2.0);
    let ground = irradiance.ghi * ALBEDO * (1.0 - tilt.cos()) / 2.0;
    (beam + diffuse + ground).max(0.0)
}

/// Clear-sky global horizontal irradiance (Haurwitz), W/m².
pub fn clear_sky_ghi(sun: SunPosition) -> f64 {
    let cos_zenith = sun.zenith.cos();
    if cos_zenith <= 0.0 {
        return 0.0;
    }
    1098.0 * cos_zenith * (-0.059 / cos_zenith).exp()
}

#[cfg(test)]
mod tests {
    use super::*;

    const AMSTERDAM: (f64, f64) = (52.37, 4.89);

    fn at(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    /// The sun is highest when it's due south; search the day for that moment.
    fn solar_noon(day: &str) -> (Timestamp, SunPosition) {
        let start = at(&format!("{day}T00:00:00Z"));
        (0..24 * 60)
            .map(|m| {
                let t = start + jiff::SignedDuration::from_mins(m);
                (t, sun_position(t, AMSTERDAM.0, AMSTERDAM.1))
            })
            .min_by(|a, b| a.1.zenith.total_cmp(&b.1.zenith))
            .unwrap()
    }

    #[test]
    fn noon_height_follows_the_seasons() {
        // At solar noon the zenith is latitude − declination.
        let (noon, summer) = solar_noon("2026-06-21");
        assert!(
            (summer.zenith.to_degrees() - (52.37 - 23.44)).abs() < 0.1,
            "{}",
            summer.zenith.to_degrees()
        );
        assert!((summer.azimuth.to_degrees() - 180.0).abs() < 1.0);
        // Amsterdam's solar noon is around 11:40 UTC in June.
        assert_eq!(noon.to_string()[11..13].to_owned(), "11");
        let (_, winter) = solar_noon("2026-12-21");
        assert!((winter.zenith.to_degrees() - (52.37 + 23.44)).abs() < 0.1);
        let (_, equinox) = solar_noon("2026-03-20");
        assert!((equinox.zenith.to_degrees() - 52.37).abs() < 0.5);
    }

    #[test]
    fn rises_in_the_east_and_sets_in_the_west() {
        // Midsummer: sunrise around 03:20 UTC in the north-east, sunset around 20:05 UTC.
        let morning = sun_position(at("2026-06-21T03:20:00Z"), AMSTERDAM.0, AMSTERDAM.1);
        assert!(
            (morning.elevation_deg()).abs() < 1.5,
            "{}",
            morning.elevation_deg()
        );
        assert!((40.0..60.0).contains(&morning.azimuth.to_degrees()));
        let evening = sun_position(at("2026-06-21T20:05:00Z"), AMSTERDAM.0, AMSTERDAM.1);
        assert!(
            (evening.elevation_deg()).abs() < 1.5,
            "{}",
            evening.elevation_deg()
        );
        assert!((300.0..320.0).contains(&evening.azimuth.to_degrees()));
    }

    #[test]
    fn a_panel_facing_the_sun_catches_all_of_the_beam() {
        let sun = SunPosition {
            zenith: 40f64.to_radians(),
            azimuth: 180f64.to_radians(),
        };
        let facing = Orientation {
            tilt: 40.0,
            azimuth: 180.0,
        };
        assert!((cos_incidence(sun, facing) - 1.0).abs() < 1e-12);
        let away = Orientation {
            tilt: 40.0,
            azimuth: 0.0,
        };
        assert!(cos_incidence(sun, away) < 0.2);
    }

    #[test]
    fn flat_panels_see_global_horizontal() {
        let sun = SunPosition {
            zenith: 50f64.to_radians(),
            azimuth: 200f64.to_radians(),
        };
        let irradiance = Irradiance {
            ghi: 500.0,
            dni: 450.0,
            dhi: 500.0 - 450.0 * 50f64.to_radians().cos(),
        };
        let flat = Orientation {
            tilt: 0.0,
            azimuth: 180.0,
        };
        let poa = plane_of_array(sun, irradiance, flat, 1361.0);
        assert!((poa - 500.0).abs() < 1.0, "{poa}");
        // Tilting towards the sun catches more.
        let tilted = Orientation {
            tilt: 35.0,
            azimuth: 200.0,
        };
        assert!(plane_of_array(sun, irradiance, tilted, 1361.0) > 600.0);
    }

    #[test]
    fn clear_sky_peaks_around_900_at_high_sun() {
        let sun = SunPosition {
            zenith: 30f64.to_radians(),
            azimuth: PI,
        };
        let ghi = clear_sky_ghi(sun);
        assert!((850.0..950.0).contains(&ghi), "{ghi}");
        assert_eq!(
            clear_sky_ghi(SunPosition {
                zenith: 2.0,
                azimuth: 0.0
            }),
            0.0
        );
    }
}
