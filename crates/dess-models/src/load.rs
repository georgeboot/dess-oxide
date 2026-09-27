//! The house's base load per hour (everything except the heat pump and EV),
//! as a small neural network.
//!
//! Unlike PV and the heat pump there's no physics to lean on: base load is
//! habits. A small MLP learns them from time of day, weekday, Dutch public
//! holidays, temperature and daylight. To be used, it has to beat the classic
//! "same hour, same weekday, last four weeks" forecast on held-out days.

use burn::backend::ndarray::NdArrayDevice;
use burn::backend::{Autodiff, NdArray};
use burn::module::Module;
use burn::nn::{Linear, LinearConfig};
use burn::optim::decay::WeightDecayConfig;
use burn::optim::{AdamConfig, GradientsParams, Optimizer};
use burn::tensor::backend::Backend;
use burn::tensor::{Tensor, TensorData, activation};

use crate::features::{HourWeather, is_validation_hour};

const HIDDEN: usize = 32;
const FEATURES: usize = 16;
const HUBER_DELTA: f64 = 0.3;

/// One hour of base load with what the model sees.
#[derive(Debug, Clone, PartialEq)]
pub struct LoadHour {
    pub weather: HourWeather,
    /// 0–23, local time.
    pub local_hour: u8,
    /// 0 = Monday.
    pub weekday: u8,
    pub holiday: bool,
    pub energy_kwh: f64,
}

/// Inputs, scaled to roughly unit range.
pub fn features(
    weather: &HourWeather,
    local_hour: u8,
    weekday: u8,
    holiday: bool,
) -> [f64; FEATURES] {
    let angle = f64::from(local_hour) / 24.0 * std::f64::consts::TAU;
    // Holidays behave like Sundays.
    let day = if holiday { 6 } else { weekday.min(6) };
    let mut x = [0.0; FEATURES];
    x[0] = angle.sin();
    x[1] = angle.cos();
    x[2] = (2.0 * angle).sin();
    x[3] = (2.0 * angle).cos();
    x[4 + usize::from(day)] = 1.0;
    x[11] = f64::from(u8::from(holiday));
    x[12] = weather.temperature / 20.0;
    x[13] = weather.smoothed_temperature[3] / 20.0;
    x[14] = weather.ghi / 1000.0;
    x[15] = weather.smoothed_ghi / 1000.0;
    x
}

/// A dense layer: `y = x · W + b`, with `W` stored row by row (`inputs × outputs`).
#[derive(Debug, Clone, PartialEq)]
pub struct Dense {
    pub inputs: usize,
    pub outputs: usize,
    pub weights: Vec<f64>,
    pub bias: Vec<f64>,
}

impl Dense {
    fn apply(&self, x: &[f64]) -> Vec<f64> {
        (0..self.outputs)
            .map(|o| {
                self.bias[o]
                    + (0..self.inputs)
                        .map(|i| x[i] * self.weights[i * self.outputs + o])
                        .sum::<f64>()
            })
            .collect()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct LoadModel {
    pub layers: [Dense; 3],
}

impl LoadModel {
    /// Base load in an hour, kWh.
    pub fn hour_kwh(
        &self,
        weather: &HourWeather,
        local_hour: u8,
        weekday: u8,
        holiday: bool,
    ) -> f64 {
        let relu = |v: Vec<f64>| v.into_iter().map(|x| x.max(0.0)).collect::<Vec<_>>();
        let x = features(weather, local_hour, weekday, holiday);
        let h1 = relu(self.layers[0].apply(&x));
        let h2 = relu(self.layers[1].apply(&h1));
        let out = self.layers[2].apply(&h2)[0];
        // softplus
        if out > 30.0 { out } else { out.exp().ln_1p() }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct LoadFit {
    pub model: LoadModel,
    pub hours: usize,
    /// Mean absolute error on held-out days, kWh per hour.
    pub validation_mae: f64,
    /// The same for "same hour and weekday, the last four weeks".
    pub baseline_mae: f64,
}

impl LoadFit {
    pub fn improves(&self) -> bool {
        self.validation_mae < self.baseline_mae
    }
}

type Train = Autodiff<NdArray<f32>>;

/// Fits the network on hours (oldest first); every fifth day is held out.
pub fn fit(hours: &[LoadHour], iterations: usize) -> LoadFit {
    let usable: Vec<&LoadHour> = hours
        .iter()
        .filter(|h| h.energy_kwh.is_finite() && h.energy_kwh >= 0.0)
        .collect();
    let (validation, training): (Vec<&LoadHour>, Vec<&LoadHour>) = usable
        .iter()
        .partition(|h| is_validation_hour(h.weather.hour));
    let device = NdArrayDevice::default();
    Train::seed(&device, 42);
    let batch = Batch::<Train>::new(&training, &device);
    let mut net = Net::<Train>::new(&device);
    let mut optimizer = AdamConfig::new()
        .with_weight_decay(Some(WeightDecayConfig::new(1e-4)))
        .init();
    for _ in 0..iterations {
        let error = net.forward(batch.x.clone()) - batch.target.clone();
        let scaled = error / HUBER_DELTA;
        let loss = ((scaled.clone() * scaled + 1.0).sqrt() - 1.0).mean();
        let gradients = GradientsParams::from_grads(loss.backward(), &net);
        net = optimizer.step(0.005, net, gradients);
    }
    let model = net.to_model();
    let validation_mae = mean(validation.iter().map(|h| {
        (model.hour_kwh(&h.weather, h.local_hour, h.weekday, h.holiday) - h.energy_kwh).abs()
    }));
    LoadFit {
        baseline_mae: seasonal_naive_mae(&usable, &validation),
        validation_mae,
        hours: usable.len(),
        model,
    }
}

fn mean(values: impl Iterator<Item = f64>) -> f64 {
    let (sum, n) = values.fold((0.0, 0usize), |(s, n), v| (s + v, n + 1));
    if n == 0 { f64::NAN } else { sum / n as f64 }
}

/// Error of forecasting each held-out hour by the same hour of the same
/// weekday over the previous four weeks.
fn seasonal_naive_mae(all: &[&LoadHour], validation: &[&LoadHour]) -> f64 {
    let by_hour: std::collections::HashMap<i64, f64> =
        all.iter().map(|h| (h.weather.hour, h.energy_kwh)).collect();
    mean(validation.iter().filter_map(|h| {
        let past: Vec<f64> = (1..=4)
            .filter_map(|w| by_hour.get(&(h.weather.hour - w * 7 * 86_400)).copied())
            .collect();
        (!past.is_empty())
            .then(|| (past.iter().sum::<f64>() / past.len() as f64 - h.energy_kwh).abs())
    }))
}

#[derive(Module, Debug)]
struct Net<B: Backend> {
    input: Linear<B>,
    hidden: Linear<B>,
    output: Linear<B>,
}

struct Batch<B: Backend> {
    x: Tensor<B, 2>,
    target: Tensor<B, 1>,
}

impl<B: Backend> Batch<B> {
    fn new(hours: &[&LoadHour], device: &B::Device) -> Self {
        let n = hours.len();
        let x: Vec<f32> = hours
            .iter()
            .flat_map(|h| {
                features(&h.weather, h.local_hour, h.weekday, h.holiday).map(|v| v as f32)
            })
            .collect();
        let target: Vec<f32> = hours.iter().map(|h| h.energy_kwh as f32).collect();
        Self {
            x: Tensor::from_data(TensorData::new(x, [n, FEATURES]), device),
            target: Tensor::from_data(TensorData::new(target, [n]), device),
        }
    }
}

impl<B: Backend> Net<B> {
    fn new(device: &B::Device) -> Self {
        Self {
            input: LinearConfig::new(FEATURES, HIDDEN).init(device),
            hidden: LinearConfig::new(HIDDEN, HIDDEN).init(device),
            output: LinearConfig::new(HIDDEN, 1).init(device),
        }
    }

    fn forward(&self, x: Tensor<B, 2>) -> Tensor<B, 1> {
        let h = activation::relu(self.input.forward(x));
        let h = activation::relu(self.hidden.forward(h));
        let out = self.output.forward(h);
        let n = out.dims()[0];
        activation::softplus(out, 1.0).reshape([n])
    }

    fn to_model(&self) -> LoadModel {
        let dense = |layer: &Linear<B>| {
            let [inputs, outputs] = layer.weight.val().dims();
            let values = |t: Tensor<B, 1>| -> Vec<f64> {
                t.into_data().iter::<f32>().map(f64::from).collect()
            };
            Dense {
                inputs,
                outputs,
                weights: values(layer.weight.val().reshape([inputs * outputs])),
                bias: layer
                    .bias
                    .as_ref()
                    .map_or_else(|| vec![0.0; outputs], |b| values(b.val())),
            }
        };
        LoadModel {
            layers: [dense(&self.input), dense(&self.hidden), dense(&self.output)],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dess_core::calendar;

    /// A household: mornings and evenings, busier weekends, more light when dark.
    fn household() -> Vec<LoadHour> {
        (0..24 * 7 * 20)
            .map(|i: i64| {
                let hour = 1_767_225_600 + i * 3600; // from 2026-01-01
                let local_hour = (i % 24) as u8;
                let weekday = ((i / 24 + 3) % 7) as u8; // 2026-01-01 is a Thursday
                let date = jiff::Timestamp::from_second(hour)
                    .unwrap()
                    .to_zoned(jiff::tz::TimeZone::UTC)
                    .date();
                let holiday = calendar::is_holiday(date);
                let weekend = weekday >= 5 || holiday;
                let ghi = if (8..17).contains(&local_hour) {
                    300.0
                } else {
                    0.0
                };
                let mut kwh = 0.25;
                if (7..9).contains(&local_hour) {
                    kwh += 0.6;
                }
                if (17..22).contains(&local_hour) {
                    kwh += 0.9;
                }
                if weekend && (10..17).contains(&local_hour) {
                    kwh += 0.5;
                }
                if ghi == 0.0 && (17..23).contains(&local_hour) {
                    kwh += 0.1;
                }
                let weather = HourWeather {
                    hour,
                    temperature: 5.0,
                    humidity: 80.0,
                    wind: 3.0,
                    ghi,
                    smoothed_temperature: [5.0; 5],
                    smoothed_ghi: ghi,
                };
                LoadHour {
                    weather,
                    local_hour,
                    weekday,
                    holiday,
                    energy_kwh: kwh,
                }
            })
            .collect()
    }

    #[test]
    fn learns_the_weekly_rhythm() {
        let fit = fit(&household(), 1500);
        assert!(
            fit.validation_mae < 0.08,
            "{} vs baseline {}",
            fit.validation_mae,
            fit.baseline_mae
        );
    }

    #[test]
    fn exported_weights_match_the_network() {
        let data = household();
        let hours: Vec<&LoadHour> = data.iter().skip(100).take(24).collect();
        let device = NdArrayDevice::default();
        NdArray::<f32>::seed(&device, 7);
        let net = Net::<NdArray<f32>>::new(&device);
        let tensor: Vec<f32> = net
            .forward(Batch::new(&hours, &device).x)
            .into_data()
            .iter::<f32>()
            .collect();
        let model = net.to_model();
        for (h, t) in hours.iter().zip(tensor) {
            let y = model.hour_kwh(&h.weather, h.local_hour, h.weekday, h.holiday);
            assert!((y - f64::from(t)).abs() < 1e-4, "{y} vs {t}");
        }
    }
}
