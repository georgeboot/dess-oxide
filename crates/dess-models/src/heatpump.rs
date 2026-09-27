//! The heat pump's electricity use per hour, as a grey-box model.
//!
//! ```text
//! T_eff  = Σ wⱼ · EWMAⱼ(T_out)                         learned thermal lag
//! heat   = softplus(UA · (T_bal − T_eff) · (1 + w · wind) − g · GHI_smoothed)
//! COP    = 1 + softplus(c₀ + c₁ · T_out)                colder is less efficient
//! frost  = relu(e_air − e_ice(T_out − ΔT_coil)) · [coil below 0 °C]
//! use    = heat / COP · (1 + k · frost) + standby + hot_water[hour of day]
//! ```
//!
//! Where OpenAmber's mode says how much of an hour was hot water, that part
//! is taken out of the target and the hour-of-day hot water term is left
//! out: heating is then learned from heating alone, and hot water gets its
//! own model ([`crate::hot_water`]). The hour-of-day term remains for hours
//! without that (before OpenAmber), and for sites without it.
//!
//! The frost term is why a humid 0 °C day can cost more than a clear −8 °C
//! one: the outdoor coil runs a few degrees below the air, and ice builds
//! when the air holds more water than the coil surface can (vapour pressure
//! over ice). Defrosting costs energy. Every parameter is physical, so a fit
//! can be sanity-checked, and it extrapolates to colder weather than seen.

use burn::backend::ndarray::NdArrayDevice;
use burn::backend::{Autodiff, NdArray};
use burn::module::{Module, Param};
use burn::optim::{AdamConfig, GradientsParams, Optimizer};
use burn::tensor::backend::Backend;
use burn::tensor::{Tensor, TensorData, activation};

use crate::features::{EWMA_HOURS, HourWeather, is_validation_hour};

const HUBER_DELTA: f64 = 0.2;

/// One hour of heat pump use with its weather.
#[derive(Debug, Clone, PartialEq)]
pub struct HpHour {
    pub weather: HourWeather,
    /// 0–23, local time.
    pub local_hour: u8,
    pub energy_kwh: f64,
    /// Of that, hot water, when the mode is known for the whole hour.
    pub hot_water_kwh: Option<f64>,
}

impl HpHour {
    /// What the model learns: heating (and standby) where the hot water is
    /// known, else everything.
    fn target(&self) -> f64 {
        self.energy_kwh - self.hot_water_kwh.unwrap_or(0.0)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct HpModel {
    /// Weights of the smoothed temperatures, summing to 1.
    pub lag_weights: [f64; 5],
    /// Heat loss, kW per kelvin below the balance temperature.
    pub ua_kw_per_k: f64,
    /// Outdoor temperature where heating stops, °C.
    pub balance_c: f64,
    /// Extra heat loss per m/s of wind, as a fraction.
    pub wind_factor: f64,
    /// Heat from the sun, kW per W/m² of smoothed irradiance.
    pub solar_gain: f64,
    pub cop_c0: f64,
    pub cop_c1: f64,
    /// Extra use per hPa of frost potential, as a fraction.
    pub frost_factor: f64,
    /// How much colder than the air the outdoor coil runs, K.
    pub coil_delta_k: f64,
    /// Hot water by hour of the day, for hours without OpenAmber's mode.
    pub hot_water_kwh: [f64; 24],
    /// The heat pump's own draw, kWh per hour.
    pub standby_kwh: f64,
}

/// Saturation vapour pressure over water, hPa (Magnus).
fn vapour_pressure_water(celsius: f64) -> f64 {
    6.112 * (17.62 * celsius / (243.12 + celsius)).exp()
}

/// Saturation vapour pressure over ice, hPa (Magnus).
fn vapour_pressure_ice(celsius: f64) -> f64 {
    6.112 * (22.46 * celsius / (272.62 + celsius)).exp()
}

fn softplus(x: f64, beta: f64) -> f64 {
    let z = beta * x;
    (if z > 30.0 { z } else { z.exp().ln_1p() }) / beta
}

fn sigmoid(x: f64) -> f64 {
    1.0 / (1.0 + (-x).exp())
}

impl HpModel {
    /// Electricity for heating at a steady outdoor temperature, without wind,
    /// sun or frost, kW. Only electricity is metered: the model's split into
    /// heat and COP is arbitrary, this is what the data pins down.
    pub fn heating_kw(&self, celsius: f64) -> f64 {
        softplus(self.ua_kw_per_k * (self.balance_c - celsius), 2.0) / self.cop(celsius)
    }

    /// Hot water by hour of the day, and standby, over a day, kWh.
    pub fn hot_water_daily_kwh(&self) -> f64 {
        self.hot_water_kwh.iter().sum::<f64>() + 24.0 * self.standby_kwh
    }

    fn cop(&self, celsius: f64) -> f64 {
        1.0 + softplus(self.cop_c0 + self.cop_c1 * celsius, 1.0)
    }

    /// A generic starting point: a well-insulated house, COP ≈ 3.5 at 0 °C.
    pub fn initial() -> Self {
        Self {
            lag_weights: [0.2; 5],
            ua_kw_per_k: 0.25,
            balance_c: 16.0,
            wind_factor: 0.02,
            solar_gain: 0.002,
            cop_c0: 2.4,
            cop_c1: 0.07,
            frost_factor: 0.02,
            coil_delta_k: 6.0,
            hot_water_kwh: [0.1; 24],
            standby_kwh: 0.03,
        }
    }

    /// Electricity use in an hour, kWh, with hot water by hour of the day.
    pub fn hour_kwh(&self, w: &HourWeather, local_hour: u8) -> f64 {
        self.heating_kwh(w) + self.hot_water_kwh[usize::from(local_hour % 24)]
    }

    /// Electricity for heating and standby in an hour, kWh: everything but
    /// hot water, for when hot water is forecast separately.
    pub fn heating_kwh(&self, w: &HourWeather) -> f64 {
        let effective: f64 = self
            .lag_weights
            .iter()
            .zip(w.smoothed_temperature)
            .map(|(a, t)| a * t)
            .sum();
        let heat = softplus(
            self.ua_kw_per_k * (self.balance_c - effective) * (1.0 + self.wind_factor * w.wind)
                - self.solar_gain * w.smoothed_ghi,
            2.0,
        );
        let cop = 1.0 + softplus(self.cop_c0 + self.cop_c1 * w.temperature, 1.0);
        let coil = w.temperature - self.coil_delta_k;
        let air = w.humidity / 100.0 * vapour_pressure_water(w.temperature);
        let frost = (air - vapour_pressure_ice(coil)).max(0.0) * sigmoid(-2.0 * coil);
        heat / cop * (1.0 + self.frost_factor * frost) + self.standby_kwh
    }

    fn predict(&self, h: &HpHour) -> f64 {
        if h.hot_water_kwh.is_some() {
            self.heating_kwh(&h.weather)
        } else {
            self.hour_kwh(&h.weather, h.local_hour)
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct HpFit {
    pub model: HpModel,
    pub hours: usize,
    /// Mean absolute error on held-out days, kWh per hour.
    pub validation_mae: f64,
    /// The same for the same hour's average over the previous seven days.
    pub baseline_mae: f64,
    /// What the held-out hours used on average, kWh, for scale.
    pub mean_kwh: f64,
}

impl HpFit {
    pub fn improves(&self) -> bool {
        self.validation_mae < self.baseline_mae
    }
}

type Train = Autodiff<NdArray<f32>>;

/// Fits the model on hours (oldest first); every fifth day is held out.
pub fn fit(hours: &[HpHour], iterations: usize) -> HpFit {
    let usable: Vec<&HpHour> = hours
        .iter()
        .filter(|h| h.energy_kwh.is_finite() && h.energy_kwh >= 0.0)
        .collect();
    let (validation, training): (Vec<&HpHour>, Vec<&HpHour>) = usable
        .iter()
        .partition(|h| is_validation_hour(h.weather.hour));
    let device = NdArrayDevice::default();
    let batch = Batch::<Train>::new(&training, &device);
    let mut net = Net::<Train>::new(&HpModel::initial(), &device);
    let mut optimizer = AdamConfig::new().init();
    for _ in 0..iterations {
        let error = net.forward(&batch) - batch.target.clone();
        let scaled = error / HUBER_DELTA;
        let loss = ((scaled.clone() * scaled + 1.0).sqrt() - 1.0).mean();
        let gradients = GradientsParams::from_grads(loss.backward(), &net);
        net = optimizer.step(0.02, net, gradients);
    }
    let model = net.to_model();
    let validation_mae = mean(
        validation
            .iter()
            .map(|h| (model.predict(h) - h.target()).abs()),
    );
    HpFit {
        baseline_mae: seasonal_naive_mae(&usable, &validation),
        mean_kwh: mean(validation.iter().map(|h| h.target())),
        validation_mae,
        hours: usable.len(),
        model,
    }
}

fn mean(values: impl Iterator<Item = f64>) -> f64 {
    let (sum, n) = values.fold((0.0, 0usize), |(s, n), v| (s + v, n + 1));
    if n == 0 { f64::NAN } else { sum / n as f64 }
}

/// Error of forecasting each held-out hour by the same hour on the previous
/// seven days.
fn seasonal_naive_mae(all: &[&HpHour], validation: &[&HpHour]) -> f64 {
    let by_hour: std::collections::HashMap<i64, f64> =
        all.iter().map(|h| (h.weather.hour, h.target())).collect();
    mean(validation.iter().filter_map(|h| {
        let past: Vec<f64> = (1..=7)
            .filter_map(|d| by_hour.get(&(h.weather.hour - d * 86_400)).copied())
            .collect();
        (!past.is_empty())
            .then(|| (past.iter().sum::<f64>() / past.len() as f64 - h.target()).abs())
    }))
}

#[derive(Module, Debug)]
struct Net<B: Backend> {
    lag_logits: Param<Tensor<B, 1>>,
    raw_ua: Param<Tensor<B, 1>>,
    balance: Param<Tensor<B, 1>>,
    raw_wind: Param<Tensor<B, 1>>,
    raw_solar: Param<Tensor<B, 1>>,
    cop: Param<Tensor<B, 1>>,
    raw_frost: Param<Tensor<B, 1>>,
    raw_coil: Param<Tensor<B, 1>>,
    raw_hot_water: Param<Tensor<B, 1>>,
    raw_standby: Param<Tensor<B, 1>>,
}

struct Batch<B: Backend> {
    smoothed: Tensor<B, 2>,
    temperature: Tensor<B, 1>,
    wind: Tensor<B, 1>,
    smoothed_ghi: Tensor<B, 1>,
    vapour_air: Tensor<B, 1>,
    hour_one_hot: Tensor<B, 2>,
    /// 1 where the hour's hot water isn't known (the hour-of-day term applies).
    unlabelled: Tensor<B, 1>,
    target: Tensor<B, 1>,
}

impl<B: Backend> Batch<B> {
    fn new(hours: &[&HpHour], device: &B::Device) -> Self {
        let n = hours.len();
        let column = |f: &dyn Fn(&HpHour) -> f64| {
            let values: Vec<f32> = hours.iter().map(|h| f(h) as f32).collect();
            Tensor::<B, 1>::from_data(TensorData::new(values, [n]), device)
        };
        let smoothed: Vec<f32> = hours
            .iter()
            .flat_map(|h| h.weather.smoothed_temperature.map(|t| t as f32))
            .collect();
        let one_hot: Vec<f32> = hours
            .iter()
            .flat_map(|h| (0..24u8).map(move |i| if i == h.local_hour % 24 { 1.0 } else { 0.0 }))
            .collect();
        Self {
            smoothed: Tensor::from_data(TensorData::new(smoothed, [n, EWMA_HOURS.len()]), device),
            temperature: column(&|h| h.weather.temperature),
            wind: column(&|h| h.weather.wind),
            smoothed_ghi: column(&|h| h.weather.smoothed_ghi),
            vapour_air: column(&|h| {
                h.weather.humidity / 100.0 * vapour_pressure_water(h.weather.temperature)
            }),
            hour_one_hot: Tensor::from_data(TensorData::new(one_hot, [n, 24]), device),
            unlabelled: column(&|h| if h.hot_water_kwh.is_some() { 0.0 } else { 1.0 }),
            target: column(&|h| h.target()),
        }
    }
}

fn inverse_softplus(y: f64) -> f32 {
    (y.exp() - 1.0).max(1e-6).ln() as f32
}

impl<B: Backend> Net<B> {
    fn new(m: &HpModel, device: &B::Device) -> Self {
        let param = |values: Vec<f32>| {
            let n = values.len();
            Param::from_tensor(Tensor::from_data(TensorData::new(values, [n]), device))
        };
        Self {
            lag_logits: param(m.lag_weights.iter().map(|w| w.ln() as f32).collect()),
            raw_ua: param(vec![inverse_softplus(m.ua_kw_per_k)]),
            balance: param(vec![m.balance_c as f32]),
            raw_wind: param(vec![inverse_softplus(m.wind_factor)]),
            raw_solar: param(vec![inverse_softplus(m.solar_gain)]),
            cop: param(vec![m.cop_c0 as f32, m.cop_c1 as f32]),
            raw_frost: param(vec![inverse_softplus(m.frost_factor)]),
            raw_coil: param(vec![inverse_softplus(m.coil_delta_k)]),
            raw_hot_water: param(
                m.hot_water_kwh
                    .iter()
                    .map(|&k| inverse_softplus(k))
                    .collect(),
            ),
            raw_standby: param(vec![inverse_softplus(m.standby_kwh)]),
        }
    }

    fn forward(&self, x: &Batch<B>) -> Tensor<B, 1> {
        let n = x.temperature.dims()[0];
        let scalar = |p: &Param<Tensor<B, 1>>| p.val().reshape([1]);
        let weights = activation::softmax(self.lag_logits.val(), 0).reshape([EWMA_HOURS.len(), 1]);
        let effective = x.smoothed.clone().matmul(weights).reshape([n]);
        let ua = activation::softplus(scalar(&self.raw_ua), 1.0);
        let wind = activation::softplus(scalar(&self.raw_wind), 1.0);
        let solar = activation::softplus(scalar(&self.raw_solar), 1.0);
        let heat = activation::softplus(
            ua * (scalar(&self.balance) - effective) * (wind * x.wind.clone() + 1.0)
                - solar * x.smoothed_ghi.clone(),
            2.0,
        );
        let cop_params = self.cop.val();
        let (c0, c1) = (
            cop_params.clone().narrow(0, 0, 1),
            cop_params.narrow(0, 1, 1),
        );
        let cop = activation::softplus(c0 + c1 * x.temperature.clone(), 1.0) + 1.0;
        let coil = x.temperature.clone() - activation::softplus(scalar(&self.raw_coil), 1.0);
        let vapour_ice = ((coil.clone() * 22.46) / (coil.clone() + 272.62)).exp() * 6.112;
        let below_freezing = activation::sigmoid(coil * -2.0);
        let frost = activation::relu(x.vapour_air.clone() - vapour_ice) * below_freezing;
        let frost_factor = activation::softplus(scalar(&self.raw_frost), 1.0);
        let hot_water = x
            .hour_one_hot
            .clone()
            .matmul(activation::softplus(self.raw_hot_water.val(), 1.0).reshape([24, 1]))
            .reshape([n]);
        let standby = activation::softplus(scalar(&self.raw_standby), 1.0);
        heat / cop * (frost_factor * frost + 1.0) + standby + hot_water * x.unlabelled.clone()
    }

    fn to_model(&self) -> HpModel {
        let values =
            |t: Tensor<B, 1>| -> Vec<f64> { t.into_data().iter::<f32>().map(f64::from).collect() };
        let one = |t: Tensor<B, 1>| values(t)[0];
        let lag = values(activation::softmax(self.lag_logits.val(), 0));
        let cop = values(self.cop.val());
        let hot_water = values(activation::softplus(self.raw_hot_water.val(), 1.0));
        HpModel {
            lag_weights: std::array::from_fn(|i| lag[i]),
            ua_kw_per_k: one(activation::softplus(self.raw_ua.val(), 1.0)),
            balance_c: one(self.balance.val()),
            wind_factor: one(activation::softplus(self.raw_wind.val(), 1.0)),
            solar_gain: one(activation::softplus(self.raw_solar.val(), 1.0)),
            cop_c0: cop[0],
            cop_c1: cop[1],
            frost_factor: one(activation::softplus(self.raw_frost.val(), 1.0)),
            coil_delta_k: one(activation::softplus(self.raw_coil.val(), 1.0)),
            hot_water_kwh: std::array::from_fn(|i| hot_water[i]),
            standby_kwh: one(activation::softplus(self.raw_standby.val(), 1.0)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::features;
    use dess_core::Slot;
    use dess_core::weather::Weather;
    use std::collections::BTreeMap;

    /// A winter half-year of weather: seasonal and daily temperature swings,
    /// with humid spells.
    fn winter() -> Vec<HourWeather> {
        let mut weather = BTreeMap::new();
        let mut slot = Slot::containing("2025-10-01T00:00:00Z".parse().unwrap());
        for q in 0..(180 * 96) {
            let day = f64::from(q) / 96.0;
            let hour_of_day = f64::from(q % 96) / 4.0;
            let temperature = 8.0 - 9.0 * (day / 180.0 * std::f64::consts::PI).sin()
                + 3.0 * ((hour_of_day - 14.0) / 24.0 * std::f64::consts::TAU).cos()
                + 3.0 * (day * 0.9).sin();
            let humidity = 75.0 + 20.0 * (day * 0.37).sin();
            let ghi = (400.0 * ((hour_of_day - 12.0) / 12.0 * std::f64::consts::PI).cos()).max(0.0)
                * (0.5 + 0.5 * (day * 0.53).sin());
            weather.insert(
                slot,
                Weather {
                    ghi,
                    dni: 0.0,
                    dhi: ghi,
                    temperature,
                    humidity,
                    wind: 3.0 + 2.0 * (day * 0.7).sin(),
                },
            );
            slot = slot.next();
        }
        features::hourly(&weather)
    }

    fn truth() -> HpModel {
        HpModel {
            lag_weights: [0.05, 0.1, 0.2, 0.4, 0.25],
            ua_kw_per_k: 0.18,
            balance_c: 15.0,
            wind_factor: 0.03,
            solar_gain: 0.001,
            cop_c0: 2.6,
            cop_c1: 0.08,
            frost_factor: 0.08,
            coil_delta_k: 5.0,
            hot_water_kwh: std::array::from_fn(|h| if h == 13 { 1.5 } else { 0.0 }),
            standby_kwh: 0.05,
        }
    }

    #[test]
    fn frost_costs_extra_in_humid_air_near_freezing_only() {
        let with_frost = truth();
        let without = HpModel {
            frost_factor: 0.0,
            ..truth()
        };
        let foggy = HourWeather {
            hour: 0,
            temperature: 0.0,
            humidity: 98.0,
            wind: 2.0,
            ghi: 0.0,
            smoothed_temperature: [0.0; 5],
            smoothed_ghi: 0.0,
        };
        let dry_cold = HourWeather {
            temperature: -8.0,
            humidity: 50.0,
            smoothed_temperature: [-8.0; 5],
            ..foggy
        };
        let mild_wet = HourWeather {
            temperature: 9.0,
            humidity: 98.0,
            smoothed_temperature: [9.0; 5],
            ..foggy
        };
        // Humid air at 0 °C: the coil runs well below freezing and ices up.
        assert!(with_frost.hour_kwh(&foggy, 3) > 1.1 * without.hour_kwh(&foggy, 3));
        // Dry cold air holds too little water; mild air keeps the coil above 0 °C.
        for weather in [dry_cold, mild_wet] {
            assert!(
                (with_frost.hour_kwh(&weather, 3) - without.hour_kwh(&weather, 3)).abs() < 0.01
            );
        }
    }

    #[test]
    fn learns_the_house_from_history() {
        let truth = truth();
        let hours: Vec<HpHour> = winter()
            .into_iter()
            .map(|w| {
                let local_hour = ((w.hour / 3600) % 24) as u8;
                let energy_kwh = truth.hour_kwh(&w, local_hour);
                HpHour {
                    weather: w,
                    local_hour,
                    energy_kwh,
                    hot_water_kwh: None,
                }
            })
            .collect();
        let fit = fit(&hours, 1500);
        assert!(fit.improves(), "{fit:?}");
        assert!(fit.validation_mae < 0.1, "{fit:?}");
        assert!(
            fit.model.hot_water_kwh[13] > 1.0,
            "finds the hot water run at 13:00"
        );
    }

    #[test]
    fn heating_is_learned_without_the_known_hot_water() {
        // The second half of the winter has OpenAmber: its hot water is known
        // and differs from the hour-of-day pattern (more of it, at 15:00).
        let truth = truth();
        let all = winter();
        let half = all.len() / 2;
        let hours: Vec<HpHour> = all
            .into_iter()
            .enumerate()
            .map(|(i, w)| {
                let local_hour = ((w.hour / 3600) % 24) as u8;
                if i < half {
                    let energy_kwh = truth.hour_kwh(&w, local_hour);
                    HpHour {
                        weather: w,
                        local_hour,
                        energy_kwh,
                        hot_water_kwh: None,
                    }
                } else {
                    let hot_water = if local_hour == 15 { 2.5 } else { 0.0 };
                    let energy_kwh = truth.heating_kwh(&w) + hot_water;
                    HpHour {
                        weather: w,
                        local_hour,
                        energy_kwh,
                        hot_water_kwh: Some(hot_water),
                    }
                }
            })
            .collect();
        let fit = fit(&hours, 1500);
        assert!(fit.validation_mae < 0.1, "{fit:?}");
        // Heating alone matches, and the old 13:00 run stays in the old term.
        for celsius in [-5.0, 0.0, 5.0] {
            let (learned, real) = (fit.model.heating_kw(celsius), truth.heating_kw(celsius));
            assert!(
                (learned - real).abs() < 0.15 * real + 0.05,
                "{celsius} °C: {learned} vs {real}"
            );
        }
        assert!(
            fit.model.hot_water_kwh[15] < 0.3,
            "{:?}",
            fit.model.hot_water_kwh
        );
    }
}
