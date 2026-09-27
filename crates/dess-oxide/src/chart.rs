//! Server-rendered SVG charts over 15-minute slots. Colours come from CSS
//! classes, so light and dark themes are handled by the page. Each chart
//! also carries its values as JSON, for the page's small script to show a
//! tooltip on hover; without the script it's a plain picture.

use std::fmt::Write as _;

use jiff::Timestamp;
use jiff::tz::TimeZone;
use maud::{Markup, PreEscaped, html};

use crate::i18n::Lang;

const WIDTH: f64 = 960.0;
const LEFT: f64 = 52.0;
const RIGHT: f64 = 8.0;
const TOP: f64 = 8.0;
const BOTTOM: f64 = 22.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// A value held over each slot.
    Step,
    /// Points at slot centres, joined.
    Line,
    /// A bar per slot, from zero.
    Bars,
}

pub struct Series<'a> {
    pub label: &'a str,
    /// CSS class setting the colour, e.g. `s-buy`.
    pub class: &'a str,
    pub kind: Kind,
    pub dashed: bool,
    pub values: Vec<Option<f64>>,
}

impl<'a> Series<'a> {
    pub fn new(
        label: &'a str,
        class: &'a str,
        kind: Kind,
        values: impl IntoIterator<Item = Option<f64>>,
    ) -> Self {
        Self {
            label,
            class,
            kind,
            dashed: false,
            values: values.into_iter().collect(),
        }
    }

    #[must_use]
    pub fn dashed(mut self) -> Self {
        self.dashed = true;
        self
    }
}

pub struct Chart<'a> {
    /// Slot starts, 15 minutes apart.
    pub slots: &'a [Timestamp],
    pub series: Vec<Series<'a>>,
    pub unit: &'a str,
    pub height: f64,
    pub now: Option<Timestamp>,
    /// Shade slots from this index on (estimated prices).
    pub shade_from: Option<usize>,
    /// Fixed y range, e.g. 0–100 for SoC.
    pub y_range: Option<(f64, f64)>,
    pub lang: Lang,
}

impl Chart<'_> {
    pub fn render(&self, tz: &TimeZone) -> Markup {
        let n = self.slots.len().max(1) as f64;
        let plot_width = WIDTH - LEFT - RIGHT;
        let slot_width = plot_width / n;
        let x = |i: f64| LEFT + i * slot_width;
        let (low, high) = self.y_range.unwrap_or_else(|| self.value_range());
        let plot_height = self.height - TOP - BOTTOM;
        let y = |v: f64| TOP + (high - v) / (high - low) * plot_height;

        let mut svg = String::new();
        let _ = write!(
            svg,
            r#"<svg viewBox="0 0 {WIDTH} {h}" class="chart" role="img" preserveAspectRatio="none">"#,
            h = self.height
        );
        if let Some(from) = self.shade_from.filter(|&i| i < self.slots.len()) {
            let _ = write!(
                svg,
                r#"<rect class="shade" x="{:.1}" y="{TOP}" width="{:.1}" height="{plot_height:.1}"><title>estimated prices</title></rect>"#,
                x(from as f64),
                x(n) - x(from as f64)
            );
        }
        for tick in ticks(low, high) {
            let _ = write!(
                svg,
                r#"<line class="grid" x1="{LEFT}" x2="{:.1}" y1="{y:.1}" y2="{y:.1}"/><text class="axis" x="{:.1}" y="{:.1}" text-anchor="end">{}</text>"#,
                WIDTH - RIGHT,
                LEFT - 6.0,
                y(tick) + 4.0,
                format_tick(tick),
                y = y(tick),
            );
        }
        if low < 0.0 && high > 0.0 {
            let _ = write!(
                svg,
                r#"<line class="zero" x1="{LEFT}" x2="{:.1}" y1="{y:.1}" y2="{y:.1}"/>"#,
                WIDTH - RIGHT,
                y = y(0.0)
            );
        }
        for (i, slot) in self.slots.iter().enumerate() {
            let local = slot.to_zoned(tz.clone());
            if local.minute() != 0 || local.hour() % 3 != 0 {
                continue;
            }
            let label = if local.hour() == 0 {
                self.lang.strftime(&local, "%a")
            } else {
                local.strftime("%H:%M").to_string()
            };
            let xi = x(i as f64);
            let _ = write!(
                svg,
                r#"<line class="grid" x1="{xi:.1}" x2="{xi:.1}" y1="{TOP}" y2="{:.1}"/><text class="axis" x="{xi:.1}" y="{:.1}" text-anchor="middle">{label}</text>"#,
                TOP + plot_height,
                self.height - 6.0
            );
        }
        for (i, series) in self.series.iter().enumerate() {
            let _ = write!(svg, r#"<g data-i="{i}">"#);
            draw(&mut svg, series, &x, &y, slot_width);
            svg.push_str("</g>");
        }
        if let (Some(now), Some(first)) = (self.now, self.slots.first()) {
            let offset = now.duration_since(*first).as_secs_f64() / 900.0;
            if (0.0..=n).contains(&offset) {
                let _ = write!(
                    svg,
                    r#"<line class="now" x1="{xn:.1}" x2="{xn:.1}" y1="{TOP}" y2="{:.1}"/>"#,
                    TOP + plot_height,
                    xn = x(offset)
                );
            }
        }
        let _ = write!(
            svg,
            r#"<line class="cursor" x1="0" x2="0" y1="{TOP}" y2="{:.1}"/>"#,
            TOP + plot_height
        );
        svg.push_str("</svg>");

        html! {
            figure data-chart=(self.data(tz, plot_width)) {
                (PreEscaped(svg))
                div.tip hidden {}
                figcaption {
                    span.unit { (self.unit) }
                    @for (i, series) in self.series.iter().enumerate() {
                        span.legend data-i=(i) title=(self.lang.t("click to hide or show", "klik om te verbergen of te tonen")) { i class=(series.class) {} (series.label) }
                    }
                }
            }
        }
    }

    /// The values as JSON for the page's hover script.
    fn data(&self, tz: &TimeZone, plot_width: f64) -> String {
        let round = |v: &Option<f64>| v.map(|v| (v * 1000.0).round() / 1000.0);
        serde_json::json!({
            "labels": self
                .slots
                .iter()
                .map(|s| self.lang.strftime(&s.to_zoned(tz.clone()), "%a %H:%M"))
                .collect::<Vec<_>>(),
            "unit": self.unit,
            "left": LEFT,
            "plot": plot_width,
            "width": WIDTH,
            "series": self
                .series
                .iter()
                .map(|s| serde_json::json!({
                    "label": s.label,
                    "class": s.class,
                    "values": s.values.iter().map(round).collect::<Vec<_>>(),
                }))
                .collect::<Vec<_>>(),
        })
        .to_string()
    }

    fn value_range(&self) -> (f64, f64) {
        let values = self
            .series
            .iter()
            .flat_map(|s| s.values.iter().flatten().copied());
        let (low, high) = values.fold((0.0f64, 0.0f64), |(lo, hi), v| (lo.min(v), hi.max(v)));
        if high - low < 1e-9 {
            return (low - 1.0, high + 1.0);
        }
        let pad = (high - low) * 0.05;
        (if low < 0.0 { low - pad } else { low }, high + pad)
    }
}

fn draw(
    svg: &mut String,
    series: &Series<'_>,
    x: &impl Fn(f64) -> f64,
    y: &impl Fn(f64) -> f64,
    slot_width: f64,
) {
    let dash = if series.dashed {
        r#" stroke-dasharray="5 4""#
    } else {
        ""
    };
    match series.kind {
        Kind::Bars => {
            let zero = y(0.0);
            for (i, value) in series.values.iter().enumerate() {
                let Some(v) = value else { continue };
                let (top, bottom) = if *v >= 0.0 {
                    (y(*v), zero)
                } else {
                    (zero, y(*v))
                };
                let _ = write!(
                    svg,
                    r#"<rect class="{} bar" x="{:.1}" y="{top:.1}" width="{:.1}" height="{:.1}"/>"#,
                    series.class,
                    x(i as f64) + slot_width * 0.1,
                    slot_width * 0.8,
                    (bottom - top).max(0.5)
                );
            }
        }
        Kind::Step | Kind::Line => {
            // One polyline per run of values, so gaps stay gaps.
            let mut run = String::new();
            let mut flush = |run: &mut String| {
                if !run.is_empty() {
                    let _ = write!(
                        svg,
                        r#"<polyline class="{} line" points="{run}"{dash}/>"#,
                        series.class
                    );
                    run.clear();
                }
            };
            for (i, value) in series.values.iter().enumerate() {
                let Some(v) = value else {
                    flush(&mut run);
                    continue;
                };
                let i = i as f64;
                if series.kind == Kind::Step {
                    let _ = write!(
                        run,
                        "{:.1},{:.1} {:.1},{:.1} ",
                        x(i),
                        y(*v),
                        x(i + 1.0),
                        y(*v)
                    );
                } else {
                    let _ = write!(run, "{:.1},{:.1} ", x(i + 0.5), y(*v));
                }
            }
            flush(&mut run);
        }
    }
}

/// Round tick values within `[low, high]`: the step of 1, 2 or 5 × 10ⁿ that
/// gives closest to five ticks.
fn ticks(low: f64, high: f64) -> Vec<f64> {
    let span = high - low;
    if span <= 0.0 || !span.is_finite() {
        return vec![];
    }
    let magnitude = 10f64.powf((span / 5.0).log10().floor());
    let step = [1.0, 2.0, 5.0, 10.0]
        .into_iter()
        .map(|m| m * magnitude)
        .min_by(|a, b| (span / a - 5.0).abs().total_cmp(&(span / b - 5.0).abs()))
        .unwrap_or(magnitude);
    let first = (low / step).ceil() as i64;
    let last = (high / step + 1e-9).floor() as i64;
    (first..=last).map(|k| k as f64 * step).collect()
}

fn format_tick(v: f64) -> String {
    if v.fract().abs() < 1e-9 {
        format!("{v:.0}")
    } else if (v * 10.0).fract().abs() < 1e-9 {
        format!("{v:.1}")
    } else {
        format!("{v:.2}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ticks_are_round() {
        assert_eq!(ticks(0.0, 100.0), vec![0.0, 20.0, 40.0, 60.0, 80.0, 100.0]);
        assert_eq!(ticks(-4.3, 7.9), vec![-4.0, -2.0, 0.0, 2.0, 4.0, 6.0]);
        let small = ticks(0.0, 0.37);
        assert_eq!(small.len(), 4);
        assert!((small[3] - 0.3).abs() < 1e-12);
    }

    #[test]
    fn renders_steps_and_gaps() {
        let slots: Vec<Timestamp> = (0..4)
            .map(|i| Timestamp::from_second(1_790_000_100 + i * 900 - 100).unwrap())
            .collect();
        let chart = Chart {
            slots: &slots,
            series: vec![Series::new(
                "buy",
                "s-buy",
                Kind::Step,
                [Some(1.0), Some(2.0), None, Some(1.0)],
            )],
            unit: "€/kWh",
            height: 200.0,
            now: None,
            shade_from: Some(3),
            y_range: None,
            lang: Lang::En,
        };
        let svg = chart.render(&TimeZone::UTC).into_string();
        assert_eq!(
            svg.matches("<polyline").count(),
            2,
            "the gap splits the line"
        );
        assert!(svg.contains(r#"class="shade""#));
    }
}
