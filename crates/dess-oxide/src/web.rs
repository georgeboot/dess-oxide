//! The dess-oxide page, served through Home Assistant ingress.
//!
//! Server-rendered HTML with SVG charts; no JavaScript, no build step. It
//! refreshes itself every minute.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;

use axum::Router;
use axum::extract::{ConnectInfo, Request, State};
use axum::http::StatusCode;
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use dess_core::Slot;
use dess_victron::probe::Severity;
use jiff::{SignedDuration, Timestamp};
use maud::{DOCTYPE, Markup, PreEscaped, html};
use tokio::sync::watch;
use tracing::{error, info};

use crate::chart::{Chart, Kind, Series};
use crate::planning::{PlanView, lock};
use crate::run::Shared;
use crate::store::{HistorySlot, StoredModel};

/// Home Assistant's ingress proxy; the only client allowed inside HA.
const INGRESS_PROXY: IpAddr = IpAddr::V4(Ipv4Addr::new(172, 30, 32, 2));

pub async fn serve(listen: SocketAddr, shared: Arc<Shared>, mut stop: watch::Receiver<bool>) {
    let listener = match tokio::net::TcpListener::bind(listen).await {
        Ok(listener) => listener,
        Err(error) => {
            error!(%listen, %error, "can't serve the web page");
            return;
        }
    };
    info!(%listen, "serving the web page");
    let app = Router::new()
        .route("/", get(page))
        .route("/api/plan", get(plan_json))
        .layer(middleware::from_fn(ingress_only))
        .with_state(shared);
    let result = axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(async move {
        let _ = stop.wait_for(|stopping| *stopping).await;
    })
    .await;
    if let Err(error) = result {
        error!(%error, "web server failed");
    }
}

/// Inside Home Assistant, only the ingress proxy may connect.
async fn ingress_only(
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    request: Request,
    next: Next,
) -> Response {
    if std::env::var_os("SUPERVISOR_TOKEN").is_some() && peer.ip() != INGRESS_PROXY {
        return StatusCode::FORBIDDEN.into_response();
    }
    next.run(request).await
}

async fn plan_json(State(shared): State<Arc<Shared>>) -> Response {
    let Some(view) = shared.plan.borrow().clone() else {
        return (StatusCode::SERVICE_UNAVAILABLE, "no plan yet").into_response();
    };
    let slots: Vec<_> = view
        .plan
        .slots
        .iter()
        .zip(&view.forecasts)
        .map(|(s, f)| {
            serde_json::json!({
                "start": s.slot.start(),
                "buy": s.prices.buy.0,
                "sell": s.prices.sell.0,
                "estimated_price": s.estimated_price,
                "load_w": f.load.0,
                "pv_w": f.pv.0,
                "battery_w": s.battery_ac.0,
                "grid_w": s.grid.0,
                "pv_on": s.pv_on,
                "soc_end": s.soc_end,
                "stored_energy_value": s.stored_energy_value.0,
            })
        })
        .collect();
    axum::Json(serde_json::json!({
        "planned_at": view.planned_at,
        "soc": view.soc,
        "expected_cost": view.plan.expected_cost,
        "slots": slots,
    }))
    .into_response()
}

async fn page(State(shared): State<Arc<Shared>>) -> Html<String> {
    let now = Timestamp::now();
    let view = shared.plan.borrow().clone();
    let since = Slot::containing(now - SignedDuration::from_hours(24));
    let history = tokio::task::block_in_place(|| lock(&shared.store).history(since))
        .unwrap_or_else(|error| {
            error!(%error, "reading history");
            Vec::new()
        });
    let pv_model =
        tokio::task::block_in_place(|| lock(&shared.store).model("pv")).unwrap_or_else(|error| {
            error!(%error, "reading the PV model");
            None
        });
    Html(render(&shared, view.as_deref(), &history, pv_model.as_ref(), now).into_string())
}

fn render(
    shared: &Shared,
    view: Option<&PlanView>,
    history: &[HistorySlot],
    pv_model: Option<&StoredModel>,
    now: Timestamp,
) -> Markup {
    let status = shared.status.lock().expect("status lock poisoned").clone();
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                meta http-equiv="refresh" content="60";
                title { "dess-oxide" }
                style { (PreEscaped(CSS)) }
            }
            body {
                header {
                    h1 { "dess-oxide" }
                    span.badge { "shadow mode · never writes to the Victron" }
                }
                @if let Some(problem) = &status.problem {
                    p.problem { (problem) }
                }
                @match view {
                    Some(view) => {
                        (now_cards(view))
                        (plan_section(shared, view, now))
                    }
                    None => p.muted { "No plan yet: waiting for the Victron data and day-ahead prices." },
                }
                (history_section(shared, history, now))
                (models_section(shared, pv_model))
                @if let Some(view) = view {
                    (slot_table(shared, view))
                }
                footer {
                    @if !status.findings.is_empty() {
                        details {
                            summary { "Victron configuration findings" }
                            ul {
                                @for (severity, message) in &status.findings {
                                    li { strong { (severity_label(*severity)) } " " (message) }
                                }
                            }
                        }
                    }
                    p.muted {
                        @if let Some(view) = view {
                            "Planned " (local(shared, view.planned_at, "%H:%M:%S")) ". "
                        }
                        @if let Some(at) = status.prices_updated { "Prices checked " (local(shared, at, "%H:%M")) ". " }
                        @if let Some(at) = status.pv_updated { "PV forecast " (local(shared, at, "%H:%M")) ". " }
                        "Refreshes every minute."
                    }
                }
            }
        }
    }
}

fn now_cards(view: &PlanView) -> Markup {
    let Some(first) = view.plan.slots.first() else {
        return html! {};
    };
    let battery = first.battery_ac.0 / 1000.0;
    let action = if battery > 0.05 {
        "charging"
    } else if battery < -0.05 {
        "discharging"
    } else {
        "idle"
    };
    html! {
        section.cards {
            div.card { span.label { "Battery" } span.value { (format!("{:.0} %", view.soc)) } span.sub { (format!("{:.1} kWh usable", view.battery.capacity.0 / 1000.0)) } }
            div.card { span.label { "This quarter hour" } span.value { (format!("{battery:+.1} kW")) } span.sub { (action) ", grid " (format!("{:+.1} kW", first.grid.0 / 1000.0)) } }
            div.card { span.label { "Price now" } span.value { (format!("€{:.3}", first.prices.buy.0)) } span.sub { "sell €" (format!("{:.3}", first.prices.sell.0)) } }
            div.card { span.label { "Stored energy worth" } span.value { (format!("€{:.3}", first.stored_energy_value.0)) } span.sub { "per kWh, at the end of this slot" } }
            div.card { span.label { "Expected over the horizon" } span.value { (format!("€{:.2}", view.plan.expected_cost)) } span.sub { "negative is money earned" } }
        }
    }
}

fn plan_section(shared: &Shared, view: &PlanView, now: Timestamp) -> Markup {
    let slots: Vec<Timestamp> = view.plan.slots.iter().map(|s| s.slot.start()).collect();
    let shade_from = view.plan.slots.iter().position(|s| s.estimated_price);
    let kw = |w: f64| Some(w / 1000.0);
    let prices = Chart {
        slots: &slots,
        series: vec![
            Series::new(
                "buy",
                "s-buy",
                Kind::Step,
                view.plan.slots.iter().map(|s| Some(s.prices.buy.0)),
            ),
            Series::new(
                "sell",
                "s-sell",
                Kind::Step,
                view.plan.slots.iter().map(|s| Some(s.prices.sell.0)),
            ),
        ],
        unit: "€/kWh",
        height: 180.0,
        now: Some(now),
        shade_from,
        y_range: None,
    };
    let power = Chart {
        slots: &slots,
        series: vec![
            Series::new(
                "battery (+ charging)",
                "s-bat",
                Kind::Bars,
                view.plan.slots.iter().map(|s| kw(s.battery_ac.0)),
            ),
            Series::new(
                "PV forecast",
                "s-pv",
                Kind::Line,
                view.forecasts.iter().map(|f| kw(f.pv.0)),
            ),
            Series::new(
                "load forecast",
                "s-load",
                Kind::Line,
                view.forecasts.iter().map(|f| kw(f.load.0)),
            ),
            Series::new(
                "grid (+ import)",
                "s-grid",
                Kind::Step,
                view.plan.slots.iter().map(|s| kw(s.grid.0)),
            ),
        ],
        unit: "kW",
        height: 240.0,
        now: Some(now),
        shade_from,
        y_range: None,
    };
    let soc = Chart {
        slots: &slots,
        series: vec![Series::new(
            "SoC",
            "s-soc",
            Kind::Line,
            view.plan.slots.iter().map(|s| Some(s.soc_end)),
        )],
        unit: "%",
        height: 140.0,
        now: Some(now),
        shade_from,
        y_range: Some((0.0, 100.0)),
    };
    html! {
        section {
            h2 { "Plan" }
            p.muted { "The shaded part uses estimated prices; only the current quarter hour would be executed." }
            (prices.render(&shared.tz))
            (power.render(&shared.tz))
            (soc.render(&shared.tz))
        }
    }
}

fn history_section(shared: &Shared, history: &[HistorySlot], now: Timestamp) -> Markup {
    if history.is_empty() {
        return html! { section { h2 { "Last 24 hours" } p.muted { "Nothing recorded yet." } } };
    }
    // The full window, so the time axis is stable; unrecorded slots are gaps.
    let recorded: std::collections::HashMap<Slot, &HistorySlot> =
        history.iter().map(|h| (h.slot, h)).collect();
    let mut window = Vec::new();
    let mut slot = Slot::containing(now - SignedDuration::from_hours(24));
    while slot.start() <= now {
        window.push(slot);
        slot = slot.next();
    }
    let slots: Vec<Timestamp> = window.iter().map(|s| s.start()).collect();
    let values = |pick: fn(&HistorySlot) -> Option<dess_core::Watts>| {
        window
            .iter()
            .map(|slot| {
                recorded
                    .get(slot)
                    .and_then(|h| pick(h))
                    .map(|w| w.0 / 1000.0)
            })
            .collect::<Vec<_>>()
    };
    let grid = Chart {
        slots: &slots,
        series: vec![
            Series::new("measured", "s-grid", Kind::Step, values(|h| Some(h.grid))),
            Series::new(
                "dess-oxide plan",
                "s-plan",
                Kind::Step,
                values(|h| h.planned_grid),
            )
            .dashed(),
            Series::new(
                "ESS setpoint (DAO)",
                "s-dao",
                Kind::Step,
                values(|h| h.setpoint),
            ),
        ],
        unit: "grid kW",
        height: 200.0,
        now: Some(now),
        shade_from: None,
        y_range: None,
    };
    let load = Chart {
        slots: &slots,
        series: vec![
            Series::new(
                "load measured",
                "s-load",
                Kind::Step,
                values(|h| Some(h.load)),
            ),
            Series::new(
                "load forecast",
                "s-forecast",
                Kind::Step,
                values(|h| h.forecast_load),
            )
            .dashed(),
            Series::new("PV measured", "s-pv", Kind::Step, values(|h| Some(h.pv))),
            Series::new(
                "PV forecast",
                "s-pv-forecast",
                Kind::Step,
                values(|h| h.forecast_pv),
            )
            .dashed(),
        ],
        unit: "kW",
        height: 200.0,
        now: Some(now),
        shade_from: None,
        y_range: None,
    };
    html! {
        section {
            h2 { "Last 24 hours" }
            p.muted { "What happened, what dess-oxide planned for it, and the setpoint DAO actually ran." }
            (grid.render(&shared.tz))
            (load.render(&shared.tz))
            (forecast_errors(history))
        }
    }
}

fn models_section(shared: &Shared, pv: Option<&StoredModel>) -> Markup {
    html! {
        section {
            h2 { "Learned models" }
            @match pv {
                None => p.muted { "PV: not trained yet. It needs two weeks of history with weather, and [[pv]] arrays to start from." },
                Some(model) => {
                    p {
                        strong { "PV" } " — trained " (local(shared, model.trained_at, "%a %d %b %H:%M"))
                        " on " (model.metrics["hours"]) " hours. Held-out error "
                        (format!("{:.3}", model.metrics["validation_mae_kwh"].as_f64().unwrap_or(f64::NAN)))
                        " kWh/h, against "
                        (format!("{:.3}", model.metrics["configured_validation_mae_kwh"].as_f64().unwrap_or(f64::NAN)))
                        " for the configured arrays: "
                        @if model.promoted { strong { "in use" } } @else { "not better, so not used" }
                        "."
                    }
                    div.scroll {
                        table {
                            thead { tr { th { "array" } th { "effective kWp" } th { "tilt" } th { "azimuth" } } }
                            tbody {
                                @for (i, array) in model.params["arrays"].as_array().into_iter().flatten().enumerate() {
                                    tr {
                                        td { (i + 1) }
                                        td { (format!("{:.2}", array["kwp"].as_f64().unwrap_or(f64::NAN))) }
                                        td { (format!("{:.0}°", array["tilt"].as_f64().unwrap_or(f64::NAN))) }
                                        td { (format!("{:.0}°", array["azimuth"].as_f64().unwrap_or(f64::NAN))) }
                                    }
                                }
                            }
                        }
                    }
                    p.muted {
                        "Inverter limit " (format!("{:.1}", model.params["cap_kw"].as_f64().unwrap_or(f64::NAN)))
                        " kW. Effective kWp includes system losses; the learned arrays needn't match the physical strings."
                    }
                }
            }
        }
    }
}

/// Mean absolute error of the baseline forecasts over the history.
fn forecast_errors(history: &[HistorySlot]) -> Markup {
    let mean_error = |pick: fn(&HistorySlot) -> Option<(f64, f64)>| {
        let errors: Vec<f64> = history
            .iter()
            .filter_map(pick)
            .map(|(forecast, actual)| (forecast - actual).abs())
            .collect();
        (!errors.is_empty()).then(|| {
            (
                errors.iter().sum::<f64>() / errors.len() as f64 / 1000.0,
                errors.len(),
            )
        })
    };
    let load = mean_error(|h| h.forecast_load.map(|f| (f.0, h.load.0)));
    let pv = mean_error(|h| h.forecast_pv.map(|f| (f.0, h.pv.0)));
    html! {
        p.muted {
            "Mean absolute forecast error: "
            @match load { Some((e, n)) => { "load " (format!("{e:.2} kW")) " over " (n) " slots" }, None => "load –" }
            "; "
            @match pv { Some((e, n)) => { "PV " (format!("{e:.2} kW")) " over " (n) " slots" }, None => "PV –" }
            ". These are the baseline forecasts; M2 replaces them."
        }
    }
}

fn slot_table(shared: &Shared, view: &PlanView) -> Markup {
    html! {
        details {
            summary { "All " (view.plan.slots.len()) " planned slots" }
            div.scroll {
                table {
                    thead { tr { th { "slot" } th { "buy" } th { "sell" } th { "load" } th { "PV" } th { "battery" } th { "grid" } th { "SoC" } th { "PV on" } } }
                    tbody {
                        @for (s, f) in view.plan.slots.iter().zip(&view.forecasts) {
                            tr class=[s.estimated_price.then_some("estimated")] {
                                td { (local(shared, s.slot.start(), "%a %H:%M")) }
                                td { (format!("{:.3}", s.prices.buy.0)) }
                                td { (format!("{:.3}", s.prices.sell.0)) }
                                td { (format!("{:.2}", f.load.0 / 1000.0)) }
                                td { (format!("{:.2}", f.pv.0 / 1000.0)) }
                                td { (format!("{:+.2}", s.battery_ac.0 / 1000.0)) }
                                td { (format!("{:+.2}", s.grid.0 / 1000.0)) }
                                td { (format!("{:.0}", s.soc_end)) }
                                td { (if s.pv_on { "on" } else { "off" }) }
                            }
                        }
                    }
                }
            }
        }
    }
}

fn local(shared: &Shared, at: Timestamp, format: &str) -> String {
    at.to_zoned(shared.tz.clone()).strftime(format).to_string()
}

fn severity_label(severity: Severity) -> &'static str {
    match severity {
        Severity::Blocker => "Blocker:",
        Severity::Warning => "Warning:",
        Severity::Info => "Info:",
    }
}

const CSS: &str = r"
:root {
  --bg: #f7f7f5; --card: #ffffff; --fg: #1d1d1b; --muted: #6b6b66; --line: #e2e2dc;
  --buy: #c2410c; --sell: #2563eb; --pv: #ca8a04; --load: #7c3aed; --bat: #059669;
  --grid: #334155; --soc: #0d9488; --plan: #2563eb; --dao: #dc2626; --forecast: #7c3aed; --shade: #eef0f3;
}
@media (prefers-color-scheme: dark) {
  :root {
    --bg: #111312; --card: #1b1d1c; --fg: #e8e8e3; --muted: #9a9a93; --line: #2d302e;
    --buy: #fb923c; --sell: #60a5fa; --pv: #facc15; --load: #a78bfa; --bat: #34d399;
    --grid: #cbd5e1; --soc: #2dd4bf; --plan: #60a5fa; --dao: #f87171; --forecast: #a78bfa; --shade: #232625;
  }
}
* { box-sizing: border-box; }
body { margin: 0 auto; max-width: 1100px; padding: 16px; background: var(--bg); color: var(--fg);
  font: 15px/1.45 system-ui, -apple-system, 'Segoe UI', sans-serif; }
header { display: flex; flex-wrap: wrap; align-items: baseline; gap: 12px; }
h1 { font-size: 22px; margin: 0; } h2 { font-size: 17px; margin: 28px 0 4px; }
.badge { font-size: 12px; padding: 2px 8px; border-radius: 999px; border: 1px solid var(--line); color: var(--muted); }
.muted { color: var(--muted); font-size: 13px; }
.problem { background: color-mix(in srgb, var(--buy) 15%, transparent); padding: 8px 12px; border-radius: 8px; }
.cards { display: grid; grid-template-columns: repeat(auto-fit, minmax(170px, 1fr)); gap: 10px; margin-top: 16px; }
.card { background: var(--card); border: 1px solid var(--line); border-radius: 10px; padding: 10px 12px; display: flex; flex-direction: column; }
.label { font-size: 12px; color: var(--muted); } .value { font-size: 22px; font-variant-numeric: tabular-nums; } .sub { font-size: 12px; color: var(--muted); }
figure { margin: 10px 0; background: var(--card); border: 1px solid var(--line); border-radius: 10px; padding: 6px; }
svg.chart { width: 100%; height: auto; display: block; }
figcaption { font-size: 12px; color: var(--muted); padding: 2px 6px; display: flex; flex-wrap: wrap; gap: 12px; }
.unit { font-weight: 600; }
.legend i { display: inline-block; width: 12px; height: 3px; margin-right: 5px; vertical-align: middle; background: currentColor; }
.grid { stroke: var(--line); stroke-width: 1; } .zero { stroke: var(--muted); stroke-width: 1; }
.now { stroke: var(--fg); stroke-width: 1.5; stroke-dasharray: 2 3; }
.axis { fill: var(--muted); font-size: 12px; font-family: system-ui, sans-serif; }
.shade { fill: var(--shade); }
.line { fill: none; stroke-width: 2; stroke-linejoin: round; }
.bar { stroke: none; opacity: .75; }
.s-buy { color: var(--buy); stroke: var(--buy); } .s-sell { color: var(--sell); stroke: var(--sell); }
.s-pv { color: var(--pv); stroke: var(--pv); } .s-pv-forecast { color: var(--pv); stroke: var(--pv); }
.s-load { color: var(--load); stroke: var(--load); } .s-forecast { color: var(--forecast); stroke: var(--forecast); }
.s-bat { color: var(--bat); fill: var(--bat); } .s-grid { color: var(--grid); stroke: var(--grid); }
.s-soc { color: var(--soc); stroke: var(--soc); } .s-plan { color: var(--plan); stroke: var(--plan); }
.s-dao { color: var(--dao); stroke: var(--dao); }
details { margin-top: 20px; } summary { cursor: pointer; }
.scroll { overflow-x: auto; }
table { border-collapse: collapse; font-size: 13px; font-variant-numeric: tabular-nums; margin-top: 8px; }
th, td { padding: 3px 10px; text-align: right; border-bottom: 1px solid var(--line); white-space: nowrap; }
th:first-child, td:first-child { text-align: left; }
tr.estimated td { color: var(--muted); }
footer { margin-top: 28px; }
";
