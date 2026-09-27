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
use crate::comparison::Comparison;
use crate::planning::Money;
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
        .route("/api/override", axum::routing::post(set_override))
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

/// The page switch. It only matters with `dryrun: false` in the options.
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
struct OverrideForm {
    /// An [`Override`](crate::control::Override) key, or "auto".
    mode: String,
}

/// Sets a manual override until midnight, or clears it.
async fn set_override(
    State(shared): State<Arc<Shared>>,
    headers: axum::http::HeaderMap,
    axum::Form(form): axum::Form<OverrideForm>,
) -> Response {
    use crate::control::{OVERRIDE_MODE, OVERRIDE_UNTIL, Override};
    let result = tokio::task::block_in_place(|| {
        let store = lock(&shared.store);
        match Override::from_key(&form.mode) {
            None => store.set_setting(OVERRIDE_UNTIL, ""),
            Some(mode) => {
                let midnight = Timestamp::now()
                    .to_zoned(shared.tz.clone())
                    .date()
                    .tomorrow()?
                    .to_zoned(shared.tz.clone())?
                    .timestamp();
                store.set_setting(OVERRIDE_MODE, mode.key())?;
                store.set_setting(OVERRIDE_UNTIL, &midnight.to_string())
            }
        }
    });
    if let Err(error) = result {
        error!(%error, "saving the override");
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    info!(mode = form.mode, "manual override set on the page");
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
    let (models, (losses, capacity), (accuracy, money, days)) = tokio::task::block_in_place(|| {
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
        let money = money_by_period(&shared, &store, now).unwrap_or_else(|error| {
            error!(%error, "reading costs");
            Vec::new()
        });
        let days = crate::accuracy::daily(&store, &shared.config, &shared.tz, now, 7)
            .unwrap_or_else(|error| {
                error!(%error, "reading daily forecast accuracy");
                Vec::new()
            });
        (models, battery, (accuracy, money, days))
    });
    Html(
        render(
            &shared,
            view.as_deref(),
            &Recorded {
                history: &history,
                accuracy: &accuracy,
                money: &money,
                comparison: *shared.comparison.lock().expect("comparison lock poisoned"),
                days: &days,
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
    money: &'a [(&'static str, Money)],
    comparison: Option<Comparison>,
    days: &'a [crate::accuracy::Day],
}

/// Costs for today, yesterday, the last 7 days and this month.
fn money_by_period(
    shared: &Shared,
    store: &crate::store::Store,
    now: Timestamp,
) -> anyhow::Result<Vec<(&'static str, Money)>> {
    let Some(tariff) = &shared.tariff else {
        return Ok(Vec::new());
    };
    let today = now.to_zoned(shared.tz.clone()).date();
    let start = |date: jiff::civil::Date| -> anyhow::Result<Timestamp> {
        Ok(date.to_zoned(shared.tz.clone())?.timestamp())
    };
    let periods = [
        ("today", start(today)?, now),
        ("yesterday", start(today.yesterday()?)?, start(today)?),
        ("last 7 days", now - SignedDuration::from_hours(24 * 7), now),
        ("this month", start(today.first_of_month())?, now),
    ];
    let from = Slot::containing(periods.iter().map(|p| p.1).min().unwrap_or(now));
    let flows = store.flows(from)?;
    let spot = store.prices(from, Slot::containing(now).next())?;
    Ok(periods
        .into_iter()
        .map(|(label, from, until)| {
            let in_period: Vec<_> = flows
                .iter()
                .filter(|f| f.slot.start() >= from && f.slot.start() < until)
                .copied()
                .collect();
            (label, crate::planning::money(&in_period, &spot, tariff))
        })
        .collect())
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
                noscript { meta http-equiv="refresh" content="60"; }
                title { "dess-oxide" }
                style { (PreEscaped(CSS)) }
                script defer { (PreEscaped(SCRIPT)) }
            }
            body {
                header {
                    h1 { "dess-oxide" }
                    @if shared.config.writes_allowed() {
                        span.badge { "writes allowed by the options" }
                    } @else {
                        span.badge { "dry run · never writes to the Victron" }
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
                (money_section(shared, recorded.money, recorded.comparison))
                (accuracy_section(recorded.accuracy, recorded.days))
                (models_section(shared, models))
                @if let Some(view) = view { (battery_section(&view.battery, losses, capacity, crate::planning::learned_bypass_draw(&lock(&shared.store)).is_some())) }
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
    use crate::control::{ControlStatus, Override};
    let stale_setpoint = shared
        .status
        .lock()
        .expect("status lock poisoned")
        .ess_setpoint_setting
        .filter(|w| {
            w.abs() > crate::control::SANE_SETPOINT_SETTING_W && shared.config.writes_allowed()
        });
    let switched_on = crate::control::switched_on(shared);
    let active = crate::control::active_override(&lock(&shared.store), Timestamp::now());
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
                    strong { "Dry run." }
                    " dess-oxide plans but never writes to the Victron. To let it take control, set "
                    code { "dryrun: false" } " in the app's options, then switch it on here."
                }
                ControlStatus::Idle(reason) => {
                    strong { "Not in control: " } (reason) ". "
                    @if switched_on { (switch(false, "Switch control off")) } @else { (switch(true, "Switch control on")) }
                }
                ControlStatus::Active(decision) => {
                    strong.active { "In control." }
                    @if decision.bypass {
                        " Bypass: the battery holds, the grid passes through"
                    } @else {
                        " Grid setpoint " (format!("{:+.1} kW", decision.setpoint.0 / 1000.0))
                        ", battery " (format!("{:+.1} kW", decision.battery_ac.0 / 1000.0))
                    }
                    ", PV " (if decision.pv_on { "on" } else { "off" }) ". "
                    (switch(false, "Switch control off"))
                }
            }
            @if let Some(setting) = stale_setpoint {
                p.problem {
                    "ESS's own grid setpoint is " (format!("{setting:.0} W")) " (probably left by DAO, which writes that setting). "
                    "Plain ESS aims for it whenever dess-oxide isn't in control, "
                    @if setting > 0.0 { "so it would charge from the grid at that power. " } @else { "so it would push that much into the grid. " }
                    "Set it to about 0–50 W on the Cerbo (Settings → ESS → Grid setpoint) once DAO is off."
                }
            }
            @if shared.config.writes_allowed() {
                form.inline method="post" action="api/override" {
                    " Until midnight: "
                    select name="mode" {
                        option value="auto" selected[active.is_none()] { "follow the plan" }
                        @for mode in Override::ALL {
                            option value=(mode.key()) selected[active == Some(mode)] { (mode.label()) }
                        }
                    }
                    " "
                    button type="submit" { "Set" }
                }
                @if let Some(mode) = active {
                    " " strong { "Override: " (mode.label()) "." }
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
            div.card { span.label { "Expected over the horizon" } span.value { (eur(view.plan.expected_cost)) } span.sub { "negative is money earned" } }
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
                "of which heat pump",
                "s-hp",
                Kind::Line,
                view.heat_pump.iter().map(|h| h.map(|w| w.0 / 1000.0)),
            )
            .dashed(),
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
            p.muted { "What happened, what dess-oxide planned for it, and the setpoint DAO actually ran. No setpoint line: ESS was in external control (DAO's bypass, battery idle)." }
            (grid.render(&shared.tz))
            (load.render(&shared.tz))
            (forecast_errors(history))
        }
    }
}

fn money_section(
    shared: &Shared,
    money: &[(&'static str, Money)],
    comparison: Option<Comparison>,
) -> Markup {
    if money.iter().all(|(_, m)| m.hours == 0.0) {
        return html! {};
    }
    let replayed = comparison.filter(|c| c.hours > 0.0).map(|c| {
        html! {
            h3 { "The last week, replayed" }
            p.muted {
                "dess-oxide's own plans, made with the forecasts it had at the time, and its policy, run over the "
                "same load, PV and prices as actually happened (" (format!("{:.0}", c.hours)) " hours"
                @if let Some(at) = c.made_at { ", replayed " (local(shared, at, "%a %H:%M")) }
                "). Each is net of the change in stored energy. Perfect foresight is the best any strategy could do."
            }
            div.scroll {
                table {
                    thead { tr { th { "" } th { "cost" } th { "saved against no battery" } } }
                    tbody {
                        @for (label, cost) in [
                            ("what happened", c.actual),
                            ("dess-oxide, replayed", c.replayed),
                            ("perfect foresight", c.perfect),
                            ("without the battery", c.without_battery),
                        ] {
                            tr { td { (label) } td { (eur(cost)) } td { (eur(c.without_battery - cost)) } }
                        }
                    }
                }
            }
        }
    });
    html! {
        section {
            h2 { "Money" }
            p.muted {
                "What was imported at the buy price minus what was exported at the sell price, from the recordings. "
                "Without the battery: the same load and PV each quarter hour. "
                "While DAO is in control, this is DAO's result."
            }
            div.scroll {
                table {
                    thead { tr { th { "" } th { "cost" } th { "without the battery" } th { "saved" } th { "recorded" } } }
                    tbody {
                        @for (label, m) in money {
                            tr {
                                td { (label) }
                                td { (eur(m.actual)) }
                                td { (eur(m.without_battery)) }
                                td { (eur(m.without_battery - m.actual)) }
                                td { (format!("{:.1} h", m.hours)) }
                            }
                        }
                    }
                }
            }
            @if let Some(replayed) = replayed { (replayed) }
        }
    }
}

/// Euros with the sign in front: "−€1.20".
fn eur(value: f64) -> String {
    if value < -0.005 {
        format!("−€{:.2}", -value)
    } else {
        format!("€{:.2}", value.max(0.0))
    }
}

fn accuracy_section(accuracy: &[LeadAccuracy], days: &[crate::accuracy::Day]) -> Markup {
    if accuracy.is_empty() && days.is_empty() {
        return html! {};
    }
    let pair = |pair: Option<crate::accuracy::Pair>| match pair {
        Some(p) if p.forecast.abs() + p.actual.abs() > 0.05 => html! {
            td {
                (format!("{:.1}", p.forecast)) " → " (format!("{:.1}", p.actual)) " kWh"
                @if let Some(e) = p.error() { span.muted { (format!(" {:+.0} %", e * 100.0)) } }
            }
        },
        _ => html! { td { "–" } },
    };
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
            @if !days.is_empty() {
                p.muted {
                    "Per day: what the last plan before midnight expected → what happened, and the forecast's error. "
                    "PV counts only quarter hours with the PV on. Base load and heat pump are split once the heat pump "
                    "model makes the forecast and the meter's hourly statistics are in (they come in every six hours)."
                }
                div.scroll {
                    table {
                        thead { tr { th { "day" } th { "PV" } th { "base load" } th { "heat pump" } th { "house load" } th { "hours" } } }
                        tbody {
                            @for day in days {
                                tr {
                                    td { (day.date.strftime("%a %d %b")) }
                                    (pair(Some(day.pv)))
                                    (pair(day.base))
                                    (pair(day.heat_pump))
                                    (pair(Some(day.load)))
                                    td { (format!("{:.0}", day.hours)) }
                                }
                            }
                        }
                    }
                }
            }
            @if !accuracy.is_empty() {
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
                    @let cap = number(&model.params["cap_kw"]);
                    @let kwp: f64 = model.params["arrays"].as_array().into_iter().flatten().map(|a| number(&a["kwp"])).sum();
                    p.muted {
                        "Effective kWp includes system losses; the learned arrays needn't match the physical strings. "
                        @if kwp * 1.1 < cap {
                            "The output never gets near the inverter's limit, so the fitted limit ("
                            (format!("{cap:.1} kW")) ") only sits above everything recorded and has no effect."
                        } @else {
                            "Output levels off at about " (format!("{cap:.1} kW")) ": the inverter's limit as the recordings show it."
                        }
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
                        li {
                            "Heating at a steady 0 °C takes about " strong { (format!("{:.1} kW", hp.heating_kw(0.0))) }
                            " (" (format!("{:.0}", hp.heating_kw(0.0) * 24.0)) " kWh a day), at −7 °C about "
                            (format!("{:.1} kW", hp.heating_kw(-7.0))) " (" (format!("{:.0}", hp.heating_kw(-7.0) * 24.0)) " kWh a day); wind adds "
                            (format!("{:.0}", hp.wind_factor * 100.0)) " % per m/s."
                        }
                        li { "Frost: +" (format!("{:.1}", hp.frost_factor * 100.0)) " % use per hPa of frost potential." }
                        li { "Standby: " (format!("{:.0} W", hp.standby_kwh * 1000.0)) "." }
                        @if shared.config.openamber().is_none() {
                            li { "Hot water: " (format!("{:.1}", hp.hot_water_kwh.iter().sum::<f64>())) " kWh a day, most around " (hot_water_peak(&hp.hot_water_kwh)) ". Learned from the total by time of day: nothing tells it which hours were hot water." }
                        }
                    }
                    p.muted { "All of this is electricity: without a heat meter (such as a flow meter on OpenAmber) the heat output and the COP aren't known." }
                }
            }
            (hot_water_section(shared))
            (history_coverage(shared))
            h3 { "Base load" }
            @match load {
                None => p.muted { "Not trained yet: it needs two weeks of load history." },
                Some(model) => (model_summary(shared, model, "baseline_mae_kwh", "the same hour on the same weekday over the last four weeks")),
            }
        }
    }
}

/// Hot water from OpenAmber's mode: the model, and the measured split per day.
fn hot_water_section(shared: &Shared) -> Markup {
    if shared.config.openamber().is_none() {
        return html! {};
    }
    let now = Timestamp::now();
    let today = now.to_zoned(shared.tz.clone()).date();
    let first = today.checked_sub(jiff::ToSpan::days(6)).unwrap_or(today);
    let from = first
        .to_zoned(shared.tz.clone())
        .map_or(Slot::containing(now), |z| Slot::containing(z.timestamp()));
    let slots = lock(&shared.store)
        .heat_pump_modes(from)
        .unwrap_or_default();
    // Per day: heating, hot water, legionella, and hours with the mode known.
    let mut days: std::collections::BTreeMap<jiff::civil::Date, [f64; 4]> =
        std::collections::BTreeMap::new();
    for s in &slots {
        let day = days
            .entry(s.slot.start().to_zoned(shared.tz.clone()).date())
            .or_default();
        let known = s.labelled_seconds / s.covered_seconds.max(1.0);
        day[0] += (s.total_wh * known - s.hot_water_wh) / 1000.0;
        day[1] += (s.hot_water_wh - s.legionella_wh) / 1000.0;
        day[2] += s.legionella_wh / 1000.0;
        day[3] += s.labelled_seconds / 3600.0;
    }
    let model = shared.hot_water_model.borrow().clone();
    let next = *shared.next_legionella.lock().expect("lock poisoned");
    html! {
        h3 { "Hot water (OpenAmber)" }
        @match &model {
            Some(m) => p {
                "From " (m.days) " days with OpenAmber's mode: " strong { (format!("{:.1} kWh", m.base_kwh)) }
                " a day at 15 °C and warmer"
                @if m.per_degree_kwh > 0.0 { ", " (format!("{:+.2}", m.per_degree_kwh)) " kWh per degree colder" }
                ", mostly around " (hot_water_peak(&m.profile)) " (daily error " (format!("{:.2}", m.daily_mae)) " kWh)."
                @if m.legionella_kwh > 0.0 { " A legionella run takes " (format!("{:.1} kWh", m.legionella_kwh)) "." }
                " Heating is learned without it, and it's forecast on its own."
            },
            None => p.muted { "Hot water gets its own forecast after a week of days with OpenAmber's mode (the recorder's history counts)." },
        }
        @if let Some(at) = next { p.muted { "Next legionella run: " (local(shared, at, "%a %d %b %H:%M")) "." } }
        @if !days.is_empty() {
            div.scroll {
                table {
                    thead { tr { th { "day" } th { "heating" } th { "hot water" } th { "legionella" } th { "mode known" } } }
                    tbody {
                        @for (date, [heating, hot_water, legionella, hours]) in days.iter().rev() {
                            tr {
                                td { (date.strftime("%a %d %b")) }
                                td { (format!("{heating:.1} kWh")) }
                                td { (format!("{hot_water:.1} kWh")) }
                                td { @if *legionella > 0.005 { (format!("{legionella:.1} kWh")) } @else { "–" } }
                                td { (format!("{hours:.0} h")) }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// How far back each Home Assistant sensor's statistics go: the heat pump
/// model needs its meter, the base load all of the sensors in the same hour.
fn history_coverage(shared: &Shared) -> Markup {
    let coverage = lock(&shared.store).ha_coverage().unwrap_or_default();
    let roles = shared.config.history.entities();
    if coverage.is_empty() || roles.is_empty() {
        return html! {};
    }
    html! {
        details {
            summary { "History imported from Home Assistant" }
            div.scroll {
                table {
                    thead { tr { th { "sensor" } th { "from" } th { "until" } th { "hours" } } }
                    tbody {
                        @for (role, entity) in &roles {
                            @let found = coverage.iter().find(|c| c.0 == *entity);
                            tr {
                                td { (role) " " span.muted { (entity) } }
                                @match found {
                                    Some((_, first, last, hours)) => {
                                        td { (local(shared, *first, "%Y-%m-%d")) }
                                        td { (local(shared, *last, "%Y-%m-%d")) }
                                        td { (hours) }
                                    }
                                    None => { td colspan="3" { "nothing imported" } }
                                }
                            }
                        }
                    }
                }
            }
            p.muted {
                "Weather is archived from 2024-07-01. The heat pump model trains on the hours with its meter and weather; "
                "the base load on the hours where every sensor has a value (or dess-oxide recorded the house itself)."
            }
        }
    }
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
    bypass_learned: bool,
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
            @if let Some(bypass) = battery.bypass_draw {
                " In bypass (ESS in external control, the battery idle) the inverters draw "
                (format!("{:.0} W", bypass.0)) (if bypass_learned { " (measured)" } else { " (prior)" })
                ", so when the battery has nothing worthwhile to do the plan holds it in bypass rather than trickling."
            }
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
                                td { @if s.bypass { "bypass" } @else { (format!("{:+.2}", s.battery_ac.0 / 1000.0)) } }
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

/// Hover tooltips and legend toggles for the charts, and a refresh every
/// minute that waits while someone is looking at a value. Progressive: the
/// page works without it.
const SCRIPT: &str = r#"
document.addEventListener('DOMContentLoaded', () => {
  const fmt = v => v == null ? '–' : Math.abs(v) >= 100 ? v.toFixed(0) : Math.abs(v) >= 10 ? v.toFixed(1) : v.toFixed(2);
  document.querySelectorAll('figure[data-chart]').forEach(fig => {
    const d = JSON.parse(fig.dataset.chart);
    const svg = fig.querySelector('svg'), cursor = svg.querySelector('.cursor'), tip = fig.querySelector('.tip');
    const n = d.labels.length, hidden = new Set();
    const hide = () => { cursor.style.display = 'none'; tip.hidden = true; };
    const show = ev => {
      const r = svg.getBoundingClientRect();
      const i = Math.floor(((ev.clientX - r.left) / r.width * d.width - d.left) / d.plot * n);
      if (i < 0 || i >= n) return hide();
      const x = d.left + (i + 0.5) * d.plot / n;
      cursor.setAttribute('x1', x); cursor.setAttribute('x2', x); cursor.style.display = 'inline';
      tip.replaceChildren();
      const title = document.createElement('b'); title.textContent = d.labels[i]; tip.append(title);
      d.series.forEach((s, k) => {
        if (hidden.has(k)) return;
        const row = document.createElement('div'), swatch = document.createElement('i'), value = document.createElement('span');
        swatch.className = s.class; value.textContent = fmt(s.values[i]) + ' ' + d.unit;
        row.append(swatch, s.label + ' ', value); tip.append(row);
      });
      tip.hidden = false;
      const left = ev.clientX - fig.getBoundingClientRect().left;
      tip.style.left = (left + 14 + tip.offsetWidth > fig.clientWidth ? left - 14 - tip.offsetWidth : left + 14) + 'px';
    };
    svg.addEventListener('pointermove', show);
    svg.addEventListener('pointerdown', show);
    svg.addEventListener('pointerleave', hide);
    fig.querySelectorAll('.legend[data-i]').forEach(el => el.addEventListener('click', () => {
      const k = Number(el.dataset.i), off = !hidden.has(k);
      off ? hidden.add(k) : hidden.delete(k);
      el.classList.toggle('off', off);
      svg.querySelectorAll('g[data-i="' + k + '"]').forEach(g => g.style.display = off ? 'none' : '');
    }));
  });
  let busy = 0;
  ['pointermove', 'pointerdown', 'keydown', 'scroll'].forEach(e => addEventListener(e, () => busy = Date.now(), { passive: true }));
  setInterval(() => {
    const idle = Date.now() - busy > 30000, focus = document.activeElement === document.body;
    if (idle && focus && !document.hidden) location.reload();
  }, 60000);
});
"#;

const CSS: &str = r"
:root {
  --bg: #f7f7f5; --card: #ffffff; --fg: #1d1d1b; --muted: #6b6b66; --line: #e2e2dc;
  --buy: #c2410c; --sell: #2563eb; --pv: #ca8a04; --load: #7c3aed; --bat: #059669;
  --grid: #334155; --soc: #0d9488; --plan: #2563eb; --dao: #dc2626; --forecast: #7c3aed; --shade: #eef0f3; --hp: #db2777;
}
@media (prefers-color-scheme: dark) {
  :root {
    --bg: #111312; --card: #1b1d1c; --fg: #e8e8e3; --muted: #9a9a93; --line: #2d302e;
    --buy: #fb923c; --sell: #60a5fa; --pv: #facc15; --load: #a78bfa; --bat: #34d399;
    --grid: #cbd5e1; --soc: #2dd4bf; --plan: #60a5fa; --dao: #f87171; --forecast: #a78bfa; --shade: #232625; --hp: #f472b6;
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
figure { margin: 10px 0; background: var(--card); border: 1px solid var(--line); border-radius: 10px; padding: 6px; position: relative; }
.cursor { stroke: var(--muted); stroke-width: 1; display: none; }
.tip { position: absolute; top: 10px; z-index: 1; pointer-events: none; background: var(--card); border: 1px solid var(--line);
  border-radius: 8px; padding: 6px 9px; font-size: 12px; line-height: 1.5; white-space: nowrap; box-shadow: 0 2px 8px rgba(0,0,0,.15); }
.tip b { display: block; margin-bottom: 2px; } .tip i { display: inline-block; width: 10px; height: 3px; margin-right: 6px; vertical-align: middle; background: currentColor; }
.tip span { font-variant-numeric: tabular-nums; }
.legend[data-i] { cursor: pointer; user-select: none; } .legend.off { opacity: .35; text-decoration: line-through; }
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
.s-dao { color: var(--dao); stroke: var(--dao); } .s-hp { color: var(--hp); stroke: var(--hp); }
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
