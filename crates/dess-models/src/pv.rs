//! The learned PV model: the same physics as the baseline, with the physical
//! parameters learned from history.
//!
//! Per array: effective kWp (system losses folded in), tilt and azimuth; plus
//! the inverter's AC limit as a soft clip. Training fits hourly PV energy with
//! Adam on a pseudo-Huber loss, so curtailed or odd hours don't dominate.
//! Burn is only used to fit; predictions use plain `f64` with the same maths.
//!
//! On top of the physics comes a correction for what it can't know: shading
//! when the sun is low, reflection at shallow angles, a weather forecast
//! that reads a little low. Boosted trees ([`crate::gbdt`]) fitted to what
//! the physics got wrong, from its own output, the weather and where the
//! sun stands. On a year of one site's data the physics made half of what
//! it predicted below 10° of sun and a tenth more than predicted above 30°;
//! the correction took a fifth off the forecast's error.

use std::f64::consts::FRAC_PI_2;

use burn::backend::{Autodiff, NdArray};
use burn::module::{Module, Param};
use burn::optim::{AdamConfig, GradientsParams, Optimizer};
use burn::tensor::backend::Backend;
use burn::tensor::{Tensor, TensorData, activation};
use dess_core::Slot;
use dess_core::solar::{self, ALBEDO};
use dess_core::weather::Weather;

use crate::features::is_validation_hour;
use crate::gbdt::{self, Gbdt, Params};

/// Power temperature coefficient of crystalline silicon, per °C.
const TEMPERATURE_COEFFICIENT: f64 = -0.004;
/// `cos(zenith)` floor for the circumsolar term, as in pvlib.
const MIN_COS_ZENITH: f64 = 0.0872;
/// Pseudo-Huber scale, kWh per hour.
const HUBER_DELTA: f64 = 0.3;

/// What the model needs for one quarter hour.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Quarter {
    pub cos_zenith: f64,
    pub sin_zenith: f64,
    /// Radians, compass bearing.
    pub sun_azimuth: f64,
    pub dni: f64,
    pub dhi: f64,
    pub ghi: f64,
    pub extraterrestrial: f64,
    pub temperature: f64,
    pub wind: f64,
}

impl Quarter {
    pub fn new(slot: Slot, weather: &Weather, latitude: f64, longitude: f64) -> Self {
        let middle = slot.start() + jiff::SignedDuration::from_secs(450);
        let sun = solar::sun_position(middle, latitude, longitude);
        let up = sun.is_up();
        let light = |v: f64| if up { v } else { 0.0 };
        Self {
            cos_zenith: sun.zenith.cos(),
            sin_zenith: sun.zenith.sin(),
            sun_azimuth: sun.azimuth,
            dni: light(weather.dni),
            dhi: light(weather.dhi),
            ghi: light(weather.ghi),
            extraterrestrial: solar::extraterrestrial(middle),
            temperature: weather.temperature,
            wind: weather.wind,
        }
    }

    fn has_light(&self) -> bool {
        self.ghi > 0.0
    }
}

/// An hour of measured PV energy with its four quarters of weather.
#[derive(Debug, Clone, PartialEq)]
pub struct Hour {
    /// Unix seconds, the hour's start.
    pub start: i64,
    pub quarters: [Quarter; 4],
    pub energy_kwh: f64,
}

/// One (virtual) array. The learned arrays needn't match the physical
/// strings: only the inverter's total is observed.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Array {
    /// Effective peak power, system losses included.
    pub kwp: f64,
    /// Degrees from horizontal.
    pub tilt: f64,
    /// Compass degrees.
    pub azimuth: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PvModel {
    pub arrays: Vec<Array>,
    /// AC limit of the inverter(s), kW.
    pub cap_kw: f64,
    /// What the physics gets wrong, learned (see the module's notes).
    pub correction: Option<Gbdt>,
}

/// What the correction sees for a quarter hour (or, in training, an hour's
/// means): the physics' power, the weather, how clear the sky is, and the
/// sun's height and compass direction in degrees.
fn correction_features(q: &Quarter, physics_kw: f64) -> [f64; 9] {
    let clearness = if q.cos_zenith > 0.05 {
        (q.ghi / (q.extraterrestrial * q.cos_zenith).max(1.0)).clamp(0.0, 1.5)
    } else {
        0.0
    };
    [
        physics_kw,
        q.ghi,
        q.dni,
        q.dhi,
        q.temperature,
        q.wind,
        clearness,
        q.cos_zenith.clamp(-1.0, 1.0).asin().to_degrees(),
        q.sun_azimuth.to_degrees(),
    ]
}

impl PvModel {
    /// AC power for a quarter hour, kW.
    pub fn power_kw(&self, q: &Quarter) -> f64 {
        let physics = self.physics_kw(q);
        match &self.correction {
            Some(trees) if q.has_light() => {
                (physics + trees.predict(&correction_features(q, physics))).clamp(0.0, self.cap_kw)
            }
            _ => physics,
        }
    }

    /// The physics alone.
    fn physics_kw(&self, q: &Quarter) -> f64 {
        let cos_zenith_guard = q.cos_zenith.max(MIN_COS_ZENITH);
        let anisotropy = (q.dni / q.extraterrestrial).clamp(0.0, 1.0);
        let dc: f64 = self
            .arrays
            .iter()
            .map(|array| {
                let tilt = array.tilt.to_radians();
                let cos_incidence = (q.cos_zenith * tilt.cos()
                    + q.sin_zenith
                        * tilt.sin()
                        * (q.sun_azimuth - array.azimuth.to_radians()).cos())
                .max(0.0);
                let poa = q.dni * cos_incidence
                    + q.dhi
                        * (anisotropy * cos_incidence / cos_zenith_guard
                            + (1.0 - anisotropy) * (1.0 + tilt.cos()) / 2.0)
                    + q.ghi * ALBEDO * (1.0 - tilt.cos()) / 2.0;
                let cell = q.temperature + poa / (25.0 + 6.84 * q.wind.max(0.0));
                array.kwp * poa / 1000.0 * (1.0 + TEMPERATURE_COEFFICIENT * (cell - 25.0))
            })
            .sum();
        soft_min(dc, self.cap_kw)
    }

    pub fn hour_kwh(&self, hour: &Hour) -> f64 {
        hour.quarters.iter().map(|q| self.power_kw(q) * 0.25).sum()
    }

    fn physics_hour_kwh(&self, hour: &Hour) -> f64 {
        hour.quarters
            .iter()
            .map(|q| self.physics_kw(q) * 0.25)
            .sum()
    }
}

/// A fitted correction and how it did.
#[derive(Debug, Clone, PartialEq)]
pub struct CorrectionFit {
    pub trees: Gbdt,
    pub hours: usize,
    /// Mean absolute error on held-out days with the correction, kWh per hour.
    pub validation_mae: f64,
    /// The same for the physics alone.
    pub physics_mae: f64,
}

impl CorrectionFit {
    pub fn improves(&self) -> bool {
        self.validation_mae < self.physics_mae
    }
}

/// Daylight hours needed before a correction is tried: about two months.
const MIN_CORRECTION_HOURS: usize = 600;

/// Fits the correction to what `physics` gets wrong on `hours`; every fifth
/// day is held out. `None` without enough daylight hours.
pub fn fit_correction(hours: &[Hour], physics: &PvModel) -> Option<CorrectionFit> {
    let physics = PvModel {
        correction: None,
        ..physics.clone()
    };
    let usable: Vec<&Hour> = hours
        .iter()
        .filter(|h| h.quarters.iter().any(Quarter::has_light) && h.energy_kwh.is_finite())
        .collect();
    if usable.len() < MIN_CORRECTION_HOURS {
        return None;
    }
    let (validation, training): (Vec<&Hour>, Vec<&Hour>) =
        usable.iter().partition(|h| is_validation_hour(h.start));
    // An hour's features are the means over its quarters; the target is what
    // the physics missed, as mean power.
    let row = |h: &Hour| {
        let mut mean = [0.0; 9];
        for q in &h.quarters {
            for (m, f) in mean
                .iter_mut()
                .zip(correction_features(q, physics.physics_kw(q)))
            {
                *m += f / 4.0;
            }
        }
        mean.to_vec()
    };
    let rows: Vec<Vec<f64>> = training.iter().map(|h| row(h)).collect();
    let targets: Vec<f64> = training
        .iter()
        .map(|h| h.energy_kwh - physics.physics_hour_kwh(h))
        .collect();
    let trees = gbdt::fit(
        &rows,
        &targets,
        &Params {
            rounds: 200,
            max_depth: 4,
            ..Params::default()
        },
    );
    let corrected = PvModel {
        correction: Some(trees),
        ..physics.clone()
    };
    let mae = |m: &PvModel| {
        validation
            .iter()
            .map(|h| (m.hour_kwh(h) - h.energy_kwh).abs())
            .sum::<f64>()
            / validation.len().max(1) as f64
    };
    Some(CorrectionFit {
        hours: usable.len(),
        validation_mae: mae(&corrected),
        physics_mae: mae(&physics),
        trees: corrected.correction?,
    })
}

/// `min(x, cap)`, smoothed as in the training graph.
fn soft_min(x: f64, cap: f64) -> f64 {
    const BETA: f64 = 4.0;
    let z = BETA * (cap - x);
    // softplus(z)/β, computed stably.
    let softplus = if z > 30.0 { z } else { z.exp().ln_1p() };
    cap - softplus / BETA
}

/// How a fit went.
#[derive(Debug, Clone, PartialEq)]
pub struct FitReport {
    pub model: PvModel,
    pub hours: usize,
    /// Mean absolute error on the held-out days, kWh.
    pub validation_mae: f64,
    /// The same for the starting model (the configured arrays).
    pub initial_validation_mae: f64,
}

impl FitReport {
    /// Whether the learned model beats the one it started from.
    pub fn improves(&self) -> bool {
        self.validation_mae < self.initial_validation_mae
    }
}

type Train = Autodiff<NdArray<f32>>;

/// Fits the physics to `hours`, starting from `initial` (its correction is
/// left out).
///
/// Every fifth day is held out for validation, so every season is in both
/// sets: the sun's height matters as much as the weather.
pub fn fit(hours: &[Hour], initial: &PvModel, iterations: usize) -> FitReport {
    let usable: Vec<&Hour> = hours
        .iter()
        .filter(|h| {
            h.quarters.iter().any(Quarter::has_light)
                && h.energy_kwh.is_finite()
                && h.energy_kwh >= 0.0
        })
        .collect();
    let (validation, training): (Vec<&Hour>, Vec<&Hour>) =
        usable.iter().partition(|h| is_validation_hour(h.start));
    let (training, validation) = (&training[..], &validation[..]);
    let initial = &PvModel {
        correction: None,
        ..initial.clone()
    };

    let device = burn::backend::ndarray::NdArrayDevice::default();
    let batch = Batch::<Train>::new(training, &device);
    let mut net = Net::<Train>::new(initial, &device);
    let mut optimizer = AdamConfig::new().init();
    for _ in 0..iterations {
        let predicted = net.forward(&batch);
        let error = predicted - batch.target.clone();
        let scaled = error / HUBER_DELTA;
        let loss = ((scaled.clone() * scaled + 1.0).sqrt() - 1.0).mean();
        let gradients = GradientsParams::from_grads(loss.backward(), &net);
        net = optimizer.step(0.02, net, gradients);
    }
    let model = net.to_model(initial.arrays.len());

    let mae = |m: &PvModel| {
        if validation.is_empty() {
            return f64::NAN;
        }
        validation
            .iter()
            .map(|h| (m.hour_kwh(h) - h.energy_kwh).abs())
            .sum::<f64>()
            / validation.len() as f64
    };
    FitReport {
        validation_mae: mae(&model),
        initial_validation_mae: mae(initial),
        hours: usable.len(),
        model,
    }
}

#[derive(Module, Debug)]
struct Net<B: Backend> {
    /// softplus → kWp
    raw_kwp: Param<Tensor<B, 1>>,
    /// sigmoid × 90° → tilt
    raw_tilt: Param<Tensor<B, 1>>,
    /// radians
    azimuth: Param<Tensor<B, 1>>,
    /// softplus → kW
    raw_cap: Param<Tensor<B, 1>>,
}

/// Training inputs, `[hours, 4]` each, and targets `[hours]`.
struct Batch<B: Backend> {
    cos_zenith: Tensor<B, 2>,
    sin_zenith: Tensor<B, 2>,
    cos_zenith_guard: Tensor<B, 2>,
    sun_azimuth: Tensor<B, 2>,
    dni: Tensor<B, 2>,
    dhi: Tensor<B, 2>,
    ghi: Tensor<B, 2>,
    anisotropy: Tensor<B, 2>,
    temperature: Tensor<B, 2>,
    wind: Tensor<B, 2>,
    target: Tensor<B, 1>,
}

impl<B: Backend> Batch<B> {
    fn new(hours: &[&Hour], device: &B::Device) -> Self {
        let n = hours.len();
        let field = |f: fn(&Quarter) -> f64| {
            let values: Vec<f32> = hours
                .iter()
                .flat_map(|h| h.quarters.iter().map(|q| f(q) as f32))
                .collect();
            Tensor::<B, 2>::from_data(TensorData::new(values, [n, 4]), device)
        };
        let targets: Vec<f32> = hours.iter().map(|h| h.energy_kwh as f32).collect();
        Self {
            cos_zenith: field(|q| q.cos_zenith),
            sin_zenith: field(|q| q.sin_zenith),
            cos_zenith_guard: field(|q| q.cos_zenith.max(MIN_COS_ZENITH)),
            sun_azimuth: field(|q| q.sun_azimuth),
            dni: field(|q| q.dni),
            dhi: field(|q| q.dhi),
            ghi: field(|q| q.ghi),
            anisotropy: field(|q| (q.dni / q.extraterrestrial).clamp(0.0, 1.0)),
            temperature: field(|q| q.temperature),
            wind: field(|q| q.wind.max(0.0)),
            target: Tensor::from_data(TensorData::new(targets, [n]), device),
        }
    }
}

impl<B: Backend> Net<B> {
    fn new(initial: &PvModel, device: &B::Device) -> Self {
        let param = |values: Vec<f32>| {
            let n = values.len();
            Param::from_tensor(Tensor::from_data(TensorData::new(values, [n]), device))
        };
        let inverse_softplus = |y: f64| (y.exp() - 1.0).max(1e-6).ln() as f32;
        let logit = |p: f64| {
            let p = p.clamp(0.01, 0.99);
            (p / (1.0 - p)).ln() as f32
        };
        Self {
            raw_kwp: param(
                initial
                    .arrays
                    .iter()
                    .map(|a| inverse_softplus(a.kwp))
                    .collect(),
            ),
            raw_tilt: param(
                initial
                    .arrays
                    .iter()
                    .map(|a| logit(a.tilt / 90.0))
                    .collect(),
            ),
            azimuth: param(
                initial
                    .arrays
                    .iter()
                    .map(|a| a.azimuth.to_radians() as f32)
                    .collect(),
            ),
            raw_cap: param(vec![inverse_softplus(initial.cap_kw)]),
        }
    }

    /// Energy per hour, kWh.
    fn forward(&self, x: &Batch<B>) -> Tensor<B, 1> {
        let kwp = activation::softplus(self.raw_kwp.val(), 1.0);
        let tilt = activation::sigmoid(self.raw_tilt.val()) * FRAC_PI_2;
        let azimuth = self.azimuth.val();
        let mut total = x.dni.zeros_like();
        for i in 0..kwp.dims()[0] {
            let one = |t: &Tensor<B, 1>| t.clone().narrow(0, i, 1).reshape([1, 1]);
            let (kwp, tilt, azimuth) = (one(&kwp), one(&tilt), one(&azimuth));
            let (cos_tilt, sin_tilt) = (tilt.clone().cos(), tilt.sin());
            let cos_incidence = activation::relu(
                x.cos_zenith.clone() * cos_tilt.clone()
                    + x.sin_zenith.clone() * sin_tilt * (x.sun_azimuth.clone() - azimuth).cos(),
            );
            let beam = x.dni.clone() * cos_incidence.clone();
            let diffuse = x.dhi.clone()
                * (x.anisotropy.clone() * cos_incidence / x.cos_zenith_guard.clone()
                    + (x.anisotropy.clone().neg() + 1.0) * ((cos_tilt.clone() + 1.0) / 2.0));
            let ground = x.ghi.clone() * ((cos_tilt.neg() + 1.0) * (ALBEDO / 2.0));
            let poa = beam + diffuse + ground;
            let cell = x.temperature.clone() + poa.clone() / (x.wind.clone() * 6.84 + 25.0);
            let temperature_factor = (cell - 25.0) * TEMPERATURE_COEFFICIENT + 1.0;
            total = total + poa * temperature_factor * kwp / 1000.0;
        }
        let cap = activation::softplus(self.raw_cap.val(), 1.0).reshape([1, 1]);
        let clipped = cap.clone() - activation::softplus(cap - total, 4.0);
        let hours = clipped.dims()[0];
        clipped.sum_dim(1).reshape([hours]) * 0.25
    }

    fn to_model(&self, arrays: usize) -> PvModel {
        let values =
            |t: Tensor<B, 1>| -> Vec<f64> { t.into_data().iter::<f32>().map(f64::from).collect() };
        let kwp = values(activation::softplus(self.raw_kwp.val(), 1.0));
        let tilt = values(activation::sigmoid(self.raw_tilt.val()) * 90.0);
        let azimuth = values(self.azimuth.val());
        let cap = values(activation::softplus(self.raw_cap.val(), 1.0));
        PvModel {
            arrays: (0..arrays)
                .map(|i| Array {
                    kwp: kwp[i],
                    tilt: tilt[i],
                    azimuth: azimuth[i].to_degrees().rem_euclid(360.0),
                })
                .collect(),
            cap_kw: cap[0],
            correction: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LATITUDE: f64 = 52.29;
    const LONGITUDE: f64 = 5.79;

    /// Clear-ish days with some cloudy ones, over a spring.
    fn hours(truth: &PvModel) -> Vec<Hour> {
        let mut out = Vec::new();
        let mut slot = Slot::containing("2026-03-01T00:00:00Z".parse().unwrap());
        for day in 0..90u32 {
            let cloudiness = f64::from((day * 7) % 10) / 10.0;
            for _hour in 0..24 {
                let start = slot.start_unix();
                let quarters: [Quarter; 4] = std::array::from_fn(|_| {
                    let middle = slot.start() + jiff::SignedDuration::from_secs(450);
                    let sun = solar::sun_position(middle, LATITUDE, LONGITUDE);
                    let clear = solar::clear_sky_ghi(sun);
                    let ghi = clear * (1.0 - 0.7 * cloudiness);
                    let dni = if sun.is_up() {
                        (ghi * (1.0 - cloudiness) * 0.8 / sun.zenith.cos().max(0.1)).min(900.0)
                    } else {
                        0.0
                    };
                    let dhi = (ghi - dni * sun.zenith.cos().max(0.0)).max(0.0);
                    let weather = Weather {
                        ghi,
                        dni,
                        dhi,
                        temperature: 12.0,
                        humidity: 70.0,
                        wind: 3.0,
                    };
                    let q = Quarter::new(slot, &weather, LATITUDE, LONGITUDE);
                    slot = slot.next();
                    q
                });
                let mut hour = Hour {
                    start,
                    quarters,
                    energy_kwh: 0.0,
                };
                hour.energy_kwh = truth.hour_kwh(&hour);
                out.push(hour);
            }
        }
        out
    }

    #[test]
    fn the_f64_and_tensor_forward_passes_agree() {
        let model = PvModel {
            arrays: vec![
                Array {
                    kwp: 5.0,
                    tilt: 33.0,
                    azimuth: 193.0,
                },
                Array {
                    kwp: 3.0,
                    tilt: 9.0,
                    azimuth: 193.0,
                },
            ],
            cap_kw: 7.0,
            correction: None,
        };
        let data = hours(&model);
        let sample: Vec<&Hour> = data.iter().skip(24 * 30 + 10).take(4).collect();
        let device = burn::backend::ndarray::NdArrayDevice::default();
        let net = Net::<NdArray<f32>>::new(&model, &device);
        let tensor: Vec<f32> = net
            .forward(&Batch::new(&sample, &device))
            .into_data()
            .iter::<f32>()
            .collect();
        for (hour, t) in sample.iter().zip(tensor) {
            assert!(
                (model.hour_kwh(hour) - f64::from(t)).abs() < 1e-3,
                "{} vs {t}",
                model.hour_kwh(hour)
            );
        }
    }

    #[test]
    fn learns_a_wrong_orientation_back() {
        let truth = PvModel {
            arrays: vec![Array {
                kwp: 6.0,
                tilt: 35.0,
                azimuth: 210.0,
            }],
            cap_kw: 20.0,
            correction: None,
        };
        let data = hours(&truth);
        let guess = PvModel {
            arrays: vec![Array {
                kwp: 4.0,
                tilt: 20.0,
                azimuth: 170.0,
            }],
            cap_kw: 20.0,
            correction: None,
        };
        let report = fit(&data, &guess, 600);
        let learned = report.model.arrays[0];
        assert!(report.improves());
        assert!((learned.kwp - 6.0).abs() < 0.6, "{learned:?}");
        assert!((learned.azimuth - 210.0).abs() < 10.0, "{learned:?}");
        assert!(report.validation_mae < 0.1, "{report:?}");
    }
    #[test]
    fn the_correction_learns_shading_at_low_sun() {
        // The truth: the physics, but half of it is lost below 15° of sun
        // (trees on the horizon).
        let physics = PvModel {
            arrays: vec![Array {
                kwp: 6.0,
                tilt: 35.0,
                azimuth: 180.0,
            }],
            cap_kw: 20.0,
            correction: None,
        };
        let mut data = hours(&physics);
        for hour in &mut data {
            hour.energy_kwh = hour
                .quarters
                .iter()
                .map(|q| {
                    let shaded = q.cos_zenith < 15f64.to_radians().sin();
                    physics.power_kw(q) * if shaded { 0.5 } else { 1.0 } * 0.25
                })
                .sum();
        }
        let fit = fit_correction(&data, &physics).unwrap();
        assert!(
            fit.improves(),
            "{} vs {}",
            fit.validation_mae,
            fit.physics_mae
        );
        assert!(fit.validation_mae < 0.4 * fit.physics_mae, "{fit:?}");
        // A quarter hour with the sun at 8°: about half of the physics.
        let corrected = PvModel {
            correction: Some(fit.trees),
            ..physics.clone()
        };
        let low = data
            .iter()
            .flat_map(|h| h.quarters.iter())
            .find(|q| q.has_light() && (0.12..0.16).contains(&q.cos_zenith) && q.dni > 200.0)
            .unwrap();
        let ratio = corrected.power_kw(low) / physics.power_kw(low);
        assert!((ratio - 0.5).abs() < 0.15, "{ratio}");
        // Too few hours: no correction.
        assert!(fit_correction(&data[..24 * 20], &physics).is_none());
    }
}
