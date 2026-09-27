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
use dess_core::{Slot, Watts};
use dess_victron::probe::Severity;
use jiff::{SignedDuration, Timestamp};
use maud::{DOCTYPE, Markup, PreEscaped, html};
use tokio::sync::watch;
use tracing::{error, info};

use crate::chart::{Chart, Kind, Series};
use crate::planning::{PlanView, lock};
use crate::run::Shared;
use crate::store::{ErrorStats, HistorySlot, LeadAccuracy, StoredModel};
use dess_core::capacity::CapacityFit;
use dess_core::efficiency::LearnedLosses;

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
        .route("/api/control", axum::routing::post(set_control))
        .route("/api/outage", axum::routing::post(set_outage))
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

#[derive(serde::Deserialize)]
struct ControlForm {
    enabled: String,
}

/// The page switch. It only matters when `control: true` is set in the options.
async fn set_control(
    State(shared): State<Arc<Shared>>,
    headers: axum::http::HeaderMap,
    axum::Form(form): axum::Form<ControlForm>,
) -> Response {
    let on = form.enabled == "on";
    let saved = tokio::task::block_in_place(|| {
        lock(&shared.store).set_setting(crate::control::SWITCH, if on { "on" } else { "off" })
    });
    if let Err(error) = saved {
        error!(%error, "saving the control switch");
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    info!(on, "control switched on the page");
    // Back to the page, under Home Assistant's ingress path when there is one.
    let base = headers
        .get("x-ingress-path")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    axum::response::Redirect::to(&format!("{base}/")).into_response()
}

#[derive(serde::Deserialize)]
struct OutageForm {
    /// "on" to expect an outage, anything else to cancel.
    expected: String,
    /// Local time, as `<input type="datetime-local">` sends it.
    #[serde(default)]
    start: String,
    #[serde(default)]
    hours: String,
}

async fn set_outage(
    State(shared): State<Arc<Shared>>,
    headers: axum::http::HeaderMap,
    axum::Form(form): axum::Form<OutageForm>,
) -> Response {
    use crate::planning::{OUTAGE_EXPECTED, OUTAGE_HOURS, OUTAGE_START};
    let result = tokio::task::block_in_place(|| {
        let store = lock(&shared.store);
        if form.expected != "on" {
            return store.set_setting(OUTAGE_EXPECTED, "off");
        }
        let start = form
            .start
            .parse::<jiff::civil::DateTime>()
            .map_err(anyhow::Error::from)
            .and_then(|local| Ok(local.to_zoned(shared.tz.clone())?.timestamp()))?;
        let hours: f64 = form.hours.parse()?;
        store.set_setting(OUTAGE_START, &start.to_string())?;
        store.set_setting(OUTAGE_HOURS, &hours.clamp(0.25, 72.0).to_string())?;
        store.set_setting(OUTAGE_EXPECTED, "on")
    });
    if let Err(error) = result {
        return (
            StatusCode::BAD_REQUEST,
            format!("couldn't read that: {error:#}"),
        )
            .into_response();
    }
    info!(
        expected = form.expected,
        start = form.start,
        hours = form.hours,
        "outage settings changed"
    );
    // Plan again with the new window.
    shared.replan_now.send_modify(|n| *n += 1);
    let base = headers
        .get("x-ingress-path")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    axum::response::Redirect::to(&format!("{base}/")).into_response()
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
    let history = tokio::task::block_in_place(|| {
        lock(&shared.store).history(since, !shared.config.ev.on_input)
    })
    .unwrap_or_else(|error| {
        error!(%error, "reading history");
        Vec::new()
    });
    let (models, (losses, capacity), accuracy) = tokio::task::block_in_place(|| {
        let store = lock(&shared.store);
        let models = ["pv", "heat_pump", "load"].map(|name| {
            store.model(name).unwrap_or_else(|error| {
                error!(%error, name, "reading a model");
                None
            })
        });
        let accuracy = store
            .forecast_accuracy(
                Slot::containing(now - SignedDuration::from_hours(24 * 7)),
                !shared.config.ev.on_input,
                shared.config.victron.pv_relay_state(),
            )
            .unwrap_or_else(|error| {
                error!(%error, "reading forecast accuracy");
                Vec::new()
            });
        let battery = (
            crate::planning::learned_losses(&store, now).ok(),
            crate::planning::learned_capacity(&store, now).ok(),
        );
        (models, battery, accuracy)
    });
    Html(
        render(
            &shared,
            view.as_deref(),
            &Recorded {
                history: &history,
                accuracy: &accuracy,
            },
            &models,
            (losses.as_ref(), capacity.as_ref()),
            now,
        )
        .into_string(),
    )
}

/// What the page shows about the past.
struct Recorded<'a> {
    history: &'a [HistorySlot],
    accuracy: &'a [LeadAccuracy],
}

fn render(
    shared: &Shared,
    view: Option<&PlanView>,
    recorded: &Recorded<'_>,
    models: &[Option<StoredModel>; 3],
    (losses, capacity): (Option<&LearnedLosses>, Option<&CapacityFit>),
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
                    @if shared.config.control {
                        span.badge { "control allowed by the options" }
                    } @else {
                        span.badge { "shadow mode · never writes to the Victron" }
                    }
                }
                @if let Some(problem) = &status.problem {
                    p.problem { (problem) }
                }
                @match view {
                    Some(view) => {
                        (control_card(shared))
                        (outage_card(shared, view, now))
                        (now_cards(shared, view))
                        (plan_section(shared, view, now))
                    }
                    None => p.muted { "No plan yet: waiting for the Victron data and day-ahead prices." },
                }
                (history_section(shared, recorded.history, now))
                (accuracy_section(recorded.accuracy))
                (models_section(shared, models))
                @if let Some(view) = view { (battery_section(&view.battery, losses, capacity)) }
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

fn control_card(shared: &Shared) -> Markup {
    use crate::control::ControlStatus;
    let switched_on = crate::control::switched_on(shared);
    let switch = |on: bool, label: &str| {
        html! {
            form.inline method="post" action="api/control" {
                input type="hidden" name="enabled" value=(if on { "on" } else { "off" });
                button type="submit" { (label) }
            }
        }
    };
    html! {
        section.control {
            @match shared.control_status() {
                ControlStatus::Shadow => {
                    strong { "Shadow mode." }
                    " dess-oxide plans but never writes to the Victron. To let it take control, set "
                    code { "control: true" } " in the app's options, then switch it on here."
                }
                ControlStatus::Idle(reason) => {
                    strong { "Not in control: " } (reason) ". "
                    @if switched_on { (switch(false, "Switch control off")) } @else { (switch(true, "Switch control on")) }
                }
                ControlStatus::Active(decision) => {
                    strong.active { "In control." }
                    " Grid setpoint " (format!("{:+.1} kW", decision.setpoint.0 / 1000.0))
                    ", battery " (format!("{:+.1} kW", decision.battery_ac.0 / 1000.0))
                    ", PV " (if decision.pv_on { "on" } else { "off" }) ". "
                    (switch(false, "Switch control off"))
                }
            }
        }
    }
}

fn outage_card(shared: &Shared, view: &PlanView, now: Timestamp) -> Markup {
    let window = crate::planning::outage_window(&lock(&shared.store), now);
    let tomorrow_morning = now
        .to_zoned(shared.tz.clone())
        .date()
        .tomorrow()
        .map(|d| d.at(8, 0, 0, 0).strftime("%Y-%m-%dT%H:%M").to_string())
        .unwrap_or_default();
    html! {
        section.control {
            @match window {
                Some((start, end)) => {
                    @let prepared = view.plan.slots.iter().find(|s| s.slot.end() > start).map(|s| s.soc_start);
                    strong { "Outage expected " }
                    (local(shared, start, "%a %H:%M")) "–" (local(shared, end, "%a %H:%M")) ". "
                    "The plan runs that window on the battery (planning for 30 % more load and 30 % less PV than forecast)"
                    @if let Some(soc) = prepared { " and starts it at " strong { (format!("{soc:.0} %")) } } ". "
                    form.inline method="post" action="api/outage" {
                        input type="hidden" name="expected" value="off";
                        button type="submit" { "Cancel" }
                    }
                }
                None => {
                    form.inline method="post" action="api/outage" {
                        input type="hidden" name="expected" value="on";
                        "Expect a power outage from "
                        input type="datetime-local" name="start" value=(tomorrow_morning) required;
                        " for "
                        input type="number" name="hours" value="4" min="0.25" max="72" step="0.25" style="width: 5em" required;
                        " hours "
                        button type="submit" { "Prepare" }
                    }
                }
            }
        }
    }
}

fn now_cards(shared: &Shared, view: &PlanView) -> Markup {
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
            div.card { span.label { "Battery" } span.value { (format!("{:.1} %", shared.soc(Timestamp::now()).unwrap_or(view.soc))) } span.sub { (format!("{:.1} kWh usable", view.battery.capacity.0 / 1000.0)) } }
            div.card { span.label { "This quarter hour" } span.value { (format!("{battery:+.1} kW")) } span.sub { (action) ", grid " (format!("{:+.1} kW", first.grid.0 / 1000.0)) } }
            div.card { span.label { "Price now" } span.value { (format!("€{:.3}", first.prices.buy.0)) } span.sub { "sell €" (format!("{:.3}", first.prices.sell.0)) } }
            div.card { span.label { "Stored energy is worth" } span.value { (format!("€{:.3}/kWh", first.stored_energy_value.0)) } span.sub { "what the last kWh in the battery will save or earn later — not what it cost" } }
            div.card { span.label { "Expected over the horizon" } span.value { (format!("€{:.2}", view.plan.expected_cost)) } span.sub { "negative is money earned" } }
            (cheapest_start_card(shared))
        }
    }
}

fn cheapest_start_card(shared: &Shared) -> Markup {
    let run = &shared.config.cheapest_start;
    let label = format!("Cheapest start ({} h, {} kWh)", run.hours, run.kwh);
    let Some(best) = *shared.cheapest_start.borrow() else {
        return html! {
            div.card { span.label { (label) } span.value { "—" } span.sub { "no room for it in the plan's night windows" } }
        };
    };
    html! {
        div.card {
            span.label { (label) }
            span.value { (local(shared, best.start, "%a %H:%M")) }
            span.sub {
                "done " (local(shared, best.end, "%H:%M")) ", about " (format!("€{:.2}", best.cost))
                @if best.first_cost - best.cost > 0.005 {
                    " (€" (format!("{:.2}", best.first_cost)) " starting at " (local(shared, best.first_start, "%H:%M")) ")"
                }
                @if best.estimated_price { "; tomorrow's prices are estimated" }
            }
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

fn accuracy_section(accuracy: &[LeadAccuracy]) -> Markup {
    if accuracy.is_empty() {
        return html! {};
    }
    let cell = |stats: Option<ErrorStats>| match stats {
        None => html! { td { "–" } td { "–" } },
        Some(s) => {
            let percent = |w: Watts| {
                if s.mean_actual.0.abs() > 1.0 {
                    format!(" ({:+.0} %)", w.0 / s.mean_actual.0 * 100.0)
                } else {
                    String::new()
                }
            };
            html! {
                td { (format!("{:.2} kW", s.mae.0 / 1000.0)) (percent(s.mae).replace('+', "")) }
                td { (format!("{:+.2} kW", s.bias.0 / 1000.0)) (percent(s.bias)) }
            }
        }
    };
    html! {
        section {
            h2 { "Forecast accuracy" }
            p.muted {
                "The last 7 days, by how far ahead the forecast was made: mean absolute error, and bias "
                "(positive = forecast too high), as a share of the actual mean. PV counts daylight slots with PV on."
            }
            div.scroll {
                table {
                    thead { tr { th { "ahead" } th { "load error" } th { "load bias" } th { "PV error" } th { "PV bias" } th { "forecasts" } } }
                    tbody {
                        @for row in accuracy {
                            tr {
                                td { (row.lead) }
                                (cell(row.load))
                                (cell(row.pv))
                                td { (row.load.map_or(0, |s| s.slots)) }
                            }
                        }
                    }
                }
            }
        }
    }
}

fn models_section(shared: &Shared, models: &[Option<StoredModel>; 3]) -> Markup {
    let [pv, heat_pump, load] = models;
    let number = |v: &serde_json::Value| v.as_f64().unwrap_or(f64::NAN);
    html! {
        section {
            h2 { "Learned models" }
            p.muted { "Trained shortly after startup and nightly. A model is only used when it beats its baseline on held-out days." }
            h3 { "PV" }
            @match pv {
                None => p.muted { "Not trained yet: it needs two weeks of history with weather, and [[pv]] arrays to start from." },
                Some(model) => {
                    (model_summary(shared, model, "configured_validation_mae_kwh", "the configured arrays"))
                    div.scroll {
                        table {
                            thead { tr { th { "array" } th { "effective kWp" } th { "tilt" } th { "azimuth" } } }
                            tbody {
                                @for (i, array) in model.params["arrays"].as_array().into_iter().flatten().enumerate() {
                                    tr {
                                        td { (i + 1) }
                                        td { (format!("{:.2}", number(&array["kwp"]))) }
                                        td { (format!("{:.0}°", number(&array["tilt"]))) }
                                        td { (format!("{:.0}°", number(&array["azimuth"]))) }
                                    }
                                }
                            }
                        }
                    }
                    p.muted {
                        "Inverter limit " (format!("{:.1}", number(&model.params["cap_kw"])))
                        " kW. Effective kWp includes system losses; the learned arrays needn't match the physical strings."
                    }
                }
            }
            h3 { "Heat pump" }
            @match heat_pump.as_ref().and_then(|m| crate::training::hp_model_from_json(&m.params).map(|hp| (m, hp))) {
                None => p.muted { "Not trained yet: it needs two weeks of the heat pump meter's history (history.heat_pump)." },
                Some((stored, hp)) => {
                    (model_summary(shared, stored, "baseline_mae_kwh", "last week's same hours"))
                    ul {
                        li { "Heating stops above about " strong { (format!("{:.1} °C", hp.balance_c)) } " outdoors, smoothed by the house's thermal lag." }
                        li { "Heat loss " (format!("{:.2}", hp.ua_kw_per_k)) " kW per degree below that, +" (format!("{:.0}", hp.wind_factor * 100.0)) " % per m/s of wind." }
                        li { "COP " (format!("{:.1}", cop(&hp, 0.0))) " at 0 °C, " (format!("{:.1}", cop(&hp, -7.0))) " at −7 °C." }
                        li { "Frost: the coil runs " (format!("{:.1}", hp.coil_delta_k)) " K below the air; +" (format!("{:.1}", hp.frost_factor * 100.0)) " % use per hPa of frost potential." }
                        li { "Hot water peaks around " (hot_water_peak(&hp.hot_water_kwh)) "." }
                    }
                }
            }
            h3 { "Base load" }
            @match load {
                None => p.muted { "Not trained yet: it needs two weeks of load history." },
                Some(model) => (model_summary(shared, model, "baseline_mae_kwh", "the same hour on the same weekday over the last four weeks")),
            }
        }
    }
}

fn cop(hp: &dess_models::heatpump::HpModel, celsius: f64) -> f64 {
    1.0 + (hp.cop_c0 + hp.cop_c1 * celsius).exp().ln_1p()
}

fn model_summary(
    shared: &Shared,
    model: &StoredModel,
    baseline_key: &str,
    baseline: &str,
) -> Markup {
    let number = |key: &str| model.metrics[key].as_f64().unwrap_or(f64::NAN);
    html! {
        p {
            "Trained " (local(shared, model.trained_at, "%a %d %b %H:%M")) " on " (model.metrics["hours"]) " hours. Held-out error "
            (format!("{:.3}", number("validation_mae_kwh"))) " kWh/h, against " (format!("{:.3}", number(baseline_key)))
            " for " (baseline) ": "
            @if model.promoted { strong { "in use" } } @else { "not better yet, so not used" }
            "."
        }
    }
}

fn hot_water_peak(profile: &[f64; 24]) -> String {
    let (hour, _) =
        profile.iter().enumerate().fold(
            (0, f64::MIN),
            |best, (h, &v)| if v > best.1 { (h, v) } else { best },
        );
    format!("{hour:02}:00")
}

/// The inverter/charger losses the planner uses: learned or the prior.
fn battery_section(
    battery: &dess_core::battery::BatteryModel,
    losses: Option<&LearnedLosses>,
    capacity: Option<&CapacityFit>,
) -> Markup {
    let kwh = |wh: f64| format!("{:.1} kWh", wh / 1000.0);
    let learned = |side: bool| {
        losses.is_some_and(|l| {
            if side {
                l.charge.is_some()
            } else {
                l.discharge.is_some()
            }
        })
    };
    let charge = |p: f64| {
        let c = battery.charge_loss;
        (p - c.linear * p - c.quadratic * p * p) / p * 100.0
    };
    let discharge = |p: f64| {
        let dc = p + battery.discharge_loss.linear * p + battery.discharge_loss.quadratic * p * p;
        p / dc * 100.0
    };
    html! {
        h3 { "Battery and inverters" }
        p {
            "Usable capacity " strong { (kwh(battery.capacity.0)) }
            @match capacity.filter(|c| c.usable_wh().is_some()) {
                Some(c) => {
                    ", learned from " (c.stretches) " long charge or discharge stretches"
                    @if let (Some(charge), Some(discharge)) = (c.charge_wh, c.discharge_wh) {
                        " (charging " (kwh(charge)) ", discharging " (kwh(discharge))
                        @if let Some(rt) = c.round_trip() { ": the cells' own round trip is " (format!("{:.1} %", rt * 100.0)) }
                        ")"
                    }
                    ", unless set in the options."
                }
                None => ", from the options or the GX device until it has seen a 30 % stretch each way.",
            }
        }
        p {
            "Charging curve: " (if learned(true) { "learned" } else { "prior (not enough steady data yet)" })
            "; discharging: " (if learned(false) { "learned" } else { "prior" })
            ". Standby " (format!("{:.0} W", battery.standby.0))
            (if losses.is_some_and(|l| l.standby.is_some()) { " (learned)" } else { " (prior)" }) "."
        }
        div.scroll {
            table {
                thead { tr { th { "AC power" } th { "charge efficiency" } th { "discharge efficiency" } } }
                tbody {
                    @for kw in [1.0, 3.0, 6.0, 10.0] {
                        tr {
                            td { (format!("{kw:.0} kW")) }
                            td { (format!("{:.1} %", charge(kw * 1000.0))) }
                            td { (format!("{:.1} %", discharge(kw * 1000.0))) }
                        }
                    }
                }
            }
        }
        p.muted { "Conversion efficiency without the standby draw, which the planner counts separately." }
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
            ". Each uses the learned model once it's in use (see below), else the baseline."
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
.control { margin-top: 16px; padding: 10px 12px; border-radius: 10px; border: 1px solid var(--line); background: var(--card); }
.control .active { color: var(--bat); }
form.inline { display: inline; }
input { font: inherit; padding: 2px 6px; border-radius: 6px; border: 1px solid var(--line); background: var(--bg); color: var(--fg); }
button { font: inherit; padding: 3px 10px; border-radius: 6px; border: 1px solid var(--line); background: var(--bg); color: var(--fg); cursor: pointer; }
h3 { font-size: 15px; margin: 18px 0 4px; }
ul { margin: 4px 0; padding-left: 20px; }
";
