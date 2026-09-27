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
use crate::i18n::Lang;
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
    let page = render(
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
    .into_string();
    Html(match shared.lang() {
        Lang::Nl => decimal_commas(&page),
        Lang::En => page,
    })
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
    let l = shared.lang();
    let Some(tariff) = &shared.tariff else {
        return Ok(Vec::new());
    };
    let today = now.to_zoned(shared.tz.clone()).date();
    let start = |date: jiff::civil::Date| -> anyhow::Result<Timestamp> {
        Ok(date.to_zoned(shared.tz.clone())?.timestamp())
    };
    let periods = [
        ((l.t("today", "vandaag")), start(today)?, now),
        (
            (l.t("yesterday", "gisteren")),
            start(today.yesterday()?)?,
            start(today)?,
        ),
        (
            (l.t("last 7 days", "afgelopen 7 dagen")),
            now - SignedDuration::from_hours(24 * 7),
            now,
        ),
        (
            (l.t("this month", "deze maand")),
            start(today.first_of_month())?,
            now,
        ),
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
    let l = shared.lang();
    let status = shared.status.lock().expect("status lock poisoned").clone();
    html! {
        (DOCTYPE)
        html lang=(l.t("en", "nl")) {
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
                        span.badge { (l.t("writes allowed by the options", "schrijven toegestaan in de opties")) }
                    } @else {
                        span.badge { (l.t("dry run · never writes to the Victron", "proefmodus · schrijft nooit naar de Victron")) }
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
                    None => p.muted { (l.t("No plan yet: waiting for the Victron data and day-ahead prices.", "Nog geen planning: wacht op de gegevens van de Victron en de day-aheadprijzen.")) },
                }
                (history_section(shared, recorded.history, now))
                (money_section(shared, recorded.money, recorded.comparison))
                (accuracy_section(l, recorded.accuracy, recorded.days))
                (models_section(shared, models))
                @if let Some(view) = view { (battery_section(l, &view.battery, losses, capacity, crate::planning::learned_bypass_draw(&lock(&shared.store)).is_some())) }
                @if let Some(view) = view {
                    (slot_table(shared, view))
                }
                footer {
                    @if !status.findings.is_empty() {
                        details {
                            summary { (l.t("Victron configuration findings", "Bevindingen over de Victron-configuratie")) }
                            ul {
                                @for (severity, message) in &status.findings {
                                    li { strong { (severity_label(l, *severity)) } " " (message) }
                                }
                            }
                        }
                    }
                    p.muted {
                        @if let Some(view) = view {
                            (l.t("Planned ", "Gepland om ")) (local(shared, view.planned_at, "%H:%M:%S")) ". "
                        }
                        @if let Some(at) = status.prices_updated { (l.t("Prices checked ", "Prijzen gecontroleerd om ")) (local(shared, at, "%H:%M")) ". " }
                        @if let Some(at) = status.pv_updated { (l.t("PV forecast ", "PV-verwachting van ")) (local(shared, at, "%H:%M")) ". " }
                        (l.t("Refreshes every minute.", "Ververst elke minuut."))
                    }
                }
            }
        }
    }
}

fn control_card(shared: &Shared) -> Markup {
    use crate::control::{ControlStatus, Override};
    let l = shared.lang();
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
                    strong { (l.t("Dry run.", "Proefmodus.")) }
                    (l.t(" dess-oxide plans but never writes to the Victron. To let it take control, set ", " dess-oxide plant, maar schrijft nooit naar de Victron. Om het de sturing te geven: zet "))
                    code { "dryrun: false" } (l.t(" in the app's options, then switch it on here.", " in de opties van de app, en zet de sturing hier aan."))
                }
                ControlStatus::Idle(why) => {
                    strong { (l.t("Not in control: ", "Stuurt niet: ")) } (reason(l, &why)) ". "
                    @if switched_on { (switch(false, l.t("Switch control off", "Sturing uitzetten"))) } @else { (switch(true, l.t("Switch control on", "Sturing aanzetten"))) }
                }
                ControlStatus::Active(decision) => {
                    strong.active { (l.t("In control.", "Stuurt.")) }
                    @if decision.bypass {
                        (l.t(" Bypass: the battery holds, the grid passes through", " Bypass: de accu staat stil, het net gaat erdoorheen"))
                    } @else {
                        (l.t(" Grid setpoint ", " Net-setpoint ")) (format!("{:+.1} kW", decision.setpoint.0 / 1000.0))
                        (l.t(", battery ", ", accu ")) (format!("{:+.1} kW", decision.battery_ac.0 / 1000.0))
                    }
                    ", PV " (if decision.pv_on { l.t("on", "aan") } else { l.t("off", "uit") }) ". "
                    (switch(false, l.t("Switch control off", "Sturing uitzetten")))
                }
            }
            @if let Some(setting) = stale_setpoint {
                p.problem {
                    (l.t("ESS's own grid setpoint is ", "Het eigen net-setpoint van ESS is ")) (format!("{setting:.0} W")) (l.t(" (probably left by DAO, which writes that setting). ", " (waarschijnlijk achtergelaten door DAO, dat die instelling schrijft). "))
                    (l.t("Plain ESS aims for it whenever dess-oxide isn't in control, ", "Gewone ESS stuurt daarop zodra dess-oxide niet stuurt, "))
                    @if setting > 0.0 { (l.t("so it would charge from the grid at that power. ", "dus dan laadt hij met dat vermogen uit het net. ")) } @else { (l.t("so it would push that much into the grid. ", "dus dan levert hij zoveel terug aan het net. ")) }
                    (l.t("Set it to about 0–50 W on the Cerbo (Settings → ESS → Grid setpoint) once DAO is off.", "Zet het op de Cerbo op zo'n 0–50 W (Instellingen → ESS → Grid setpoint) zodra DAO uit staat."))
                }
            }
            @if shared.config.writes_allowed() {
                form.inline method="post" action="api/override" {
                    (l.t(" Until midnight: ", " Tot middernacht: "))
                    select name="mode" {
                        option value="auto" selected[active.is_none()] { (l.t("follow the plan", "volg de planning")) }
                        @for mode in Override::ALL {
                            option value=(mode.key()) selected[active == Some(mode)] { (mode.label(l)) }
                        }
                    }
                    " "
                    button type="submit" { (l.t("Set", "Instellen")) }
                }
                @if let Some(mode) = active {
                    " " strong { (l.t("Override: ", "Handmatig: ")) (mode.label(l)) "." }
                }
            }
        }
    }
}

fn outage_card(shared: &Shared, view: &PlanView, now: Timestamp) -> Markup {
    let l = shared.lang();
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
                    strong { (l.t("Outage expected ", "Stroomstoring verwacht ")) }
                    (local(shared, start, "%a %H:%M")) "–" (local(shared, end, "%a %H:%M")) ". "
                    (l.t("The plan runs that window on the battery (planning for 30 % more load and 30 % less PV than forecast)", "De planning overbrugt die tijd met de accu (met 30 % meer verbruik en 30 % minder PV dan verwacht)"))
                    @if let Some(soc) = prepared { (l.t(" and starts it at ", " en begint eraan met ")) strong { (format!("{soc:.0} %")) } } ". "
                    form.inline method="post" action="api/outage" {
                        input type="hidden" name="expected" value="off";
                        button type="submit" { (l.t("Cancel", "Annuleren")) }
                    }
                }
                None => {
                    form.inline method="post" action="api/outage" {
                        input type="hidden" name="expected" value="on";
                        (l.t("Expect a power outage from ", "Stroomstoring verwacht vanaf "))
                        input type="datetime-local" name="start" value=(tomorrow_morning) required;
                        (l.t(" for ", " voor "))
                        input type="number" name="hours" value="4" min="0.25" max="72" step="0.25" style="width: 5em" required;
                        (l.t(" hours ", " uur "))
                        button type="submit" { (l.t("Prepare", "Voorbereiden")) }
                    }
                }
            }
        }
    }
}

fn now_cards(shared: &Shared, view: &PlanView) -> Markup {
    let l = shared.lang();
    let Some(first) = view.plan.slots.first() else {
        return html! {};
    };
    let battery = first.battery_ac.0 / 1000.0;
    // The plan's value of stored energy, as break-even prices after losses.
    let value = first.stored_energy_value.0;
    let efficiency = |ac: f64| view.battery.dc_for_ac(dess_core::Watts(ac)).0 / ac;
    let charge_below = value * efficiency(3000.0);
    let discharge_above = value * efficiency(-3000.0);
    let action = if battery > 0.05 {
        l.t("charging", "laden")
    } else if battery < -0.05 {
        l.t("discharging", "ontladen")
    } else {
        l.t("idle", "stil")
    };
    html! {
        section.cards {
            div.card { span.label { (l.t("Battery", "Accu")) } span.value { (format!("{:.1} %", shared.soc(Timestamp::now()).unwrap_or(view.soc))) } span.sub { (format!("{:.1} kWh", view.battery.capacity.0 / 1000.0)) (l.t(" usable", " bruikbaar")) } }
            div.card { span.label { (l.t("This quarter hour", "Dit kwartier")) } span.value { (format!("{battery:+.1} kW")) } span.sub { (action) (l.t(", grid ", ", net ")) (format!("{:+.1} kW", first.grid.0 / 1000.0)) } }
            div.card {
                span.label { (l.t("Price now", "Prijs nu")) }
                span.value { (format!("€{:.3}", first.prices.buy.0)) }
                span.sub {
                    (l.t("sell €", "teruglevering €")) (format!("{:.3}", first.prices.sell.0))
                    @if let Some(why) = sell_note(shared, first.slot) { " · " (why) }
                }
            }
            div.card {
                span.label { (l.t("One more kWh in the battery is worth", "Eén kWh extra in de accu is waard")) }
                span.value { (format!("€{:.3}", value)) }
                span.sub {
                    (l.t("So charging from the grid pays below €", "Laden uit het net loont dus onder €")) (format!("{:.3}", charge_below))
                    (l.t(" a kWh, and discharging into the grid above €", " per kWh, en ontladen naar het net boven €")) (format!("{:.3}", discharge_above))
                    (l.t(" (with the losses at 3 kW). In between, the battery holds.", " (met de verliezen bij 3 kW). Daartussen blijft de accu staan."))
                }
            }
            div.card { span.label { (l.t("Expected over the horizon", "Verwacht over de horizon")) } span.value { (eur(view.plan.expected_cost)) } span.sub { (l.t("negative is money earned", "negatief is geld verdiend")) } }
            (cheapest_start_card(shared))
        }
    }
}

fn cheapest_start_card(shared: &Shared) -> Markup {
    let l = shared.lang();
    let run = &shared.config.cheapest_start;
    let label = format!(
        "{} ({} {}, {} kWh)",
        l.t("Cheapest start", "Goedkoopste start"),
        run.hours,
        l.t("h", "u"),
        run.kwh
    );
    let Some(best) = *shared.cheapest_start.borrow() else {
        return html! {
            div.card { span.label { (label) } span.value { "—" } span.sub { (l.t("no room for it in the plan's night windows", "geen ruimte voor in de nachtvensters van de planning")) } }
        };
    };
    html! {
        div.card {
            span.label { (label) }
            span.value { (local(shared, best.start, "%a %H:%M")) }
            span.sub {
                (l.t("done ", "klaar ")) (local(shared, best.end, "%H:%M")) (l.t(", about ", ", ongeveer ")) (format!("€{:.2}", best.cost))
                @if best.first_cost - best.cost > 0.005 {
                    " (€" (format!("{:.2}", best.first_cost)) (l.t(" starting at ", " bij een start om ")) (local(shared, best.first_start, "%H:%M")) ")"
                }
                @if best.estimated_price { (l.t("; tomorrow's prices are estimated", "; de prijzen van morgen zijn geschat")) }
            }
        }
    }
}

/// The local weather station's readings, and how they corrected the forecast.
fn station_line(shared: &Shared) -> Markup {
    let l = shared.lang();
    let reading = *shared.station.lock().expect("lock poisoned");
    let Some(reading) = reading else {
        return html! {};
    };
    let correction = *shared.weather_correction.lock().expect("lock poisoned");
    let o = reading.now;
    let parts: Vec<String> = [
        o.temperature.map(|t| format!("{t:.1} °C")),
        o.humidity.map(|h| format!("{h:.0} %")),
        o.wind
            .map(|w| format!("{w:.1} m/s {}", l.t("wind", "wind"))),
        o.ghi.map(|g| format!("{g:.0} W/m²")),
    ]
    .into_iter()
    .flatten()
    .collect();
    let mut changes: Vec<String> = Vec::new();
    if let Some(c) = correction {
        if let Some(t) = c.temperature.filter(|t| t.abs() >= 0.1) {
            changes.push(format!("{t:+.1} °C"));
        }
        if let Some(h) = c.humidity.filter(|h| h.abs() >= 1.0) {
            changes.push(format!("{h:+.0} % {}", l.t("humidity", "luchtvochtigheid")));
        }
        if let Some(r) = c.irradiance.filter(|r| (r - 1.0).abs() >= 0.02) {
            changes.push(format!("{} ×{r:.2}", l.t("sunshine", "zon")));
        }
    }
    html! {
        p.muted {
            (l.t("Weather station (", "Weerstation (")) (local(shared, reading.at, "%H:%M")) "): " (parts.join(", ")) ". "
            @if changes.is_empty() {
                (l.t("The forecast agrees with the last hour it measured.", "De verwachting klopt met het laatste uur dat het station mat."))
            } @else {
                (l.t("The next hours' forecast is corrected by ", "De verwachting voor de komende uren is gecorrigeerd met ")) (changes.join(", "))
                (l.t(", fading out (temperature and humidity over a few hours, sunshine within about an hour).", ", uitdovend (temperatuur en luchtvochtigheid over een paar uur, zon binnen ongeveer een uur)."))
            }
        }
    }
}

fn plan_section(shared: &Shared, view: &PlanView, now: Timestamp) -> Markup {
    let l = shared.lang();
    let slots: Vec<Timestamp> = view.plan.slots.iter().map(|s| s.slot.start()).collect();
    let shade_from = view.plan.slots.iter().position(|s| s.estimated_price);
    let kw = |w: f64| Some(w / 1000.0);
    let prices = Chart {
        slots: &slots,
        series: vec![
            Series::new(
                l.t("buy", "levering"),
                "s-buy",
                Kind::Step,
                view.plan.slots.iter().map(|s| Some(s.prices.buy.0)),
            ),
            Series::new(
                l.t("sell", "teruglevering"),
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
        lang: l,
    };
    let power = Chart {
        slots: &slots,
        series: vec![
            Series::new(
                l.t("battery (+ charging)", "accu (+ laden)"),
                "s-bat",
                Kind::Bars,
                view.plan.slots.iter().map(|s| kw(s.battery_ac.0)),
            ),
            Series::new(
                l.t("PV forecast", "PV-verwachting"),
                "s-pv",
                Kind::Line,
                view.forecasts.iter().map(|f| kw(f.pv.0)),
            ),
            Series::new(
                l.t("load forecast", "verbruiksverwachting"),
                "s-load",
                Kind::Line,
                view.forecasts.iter().map(|f| kw(f.load.0)),
            ),
            Series::new(
                l.t("of which heat pump", "waarvan warmtepomp"),
                "s-hp",
                Kind::Line,
                view.heat_pump.iter().map(|h| h.map(|w| w.0 / 1000.0)),
            )
            .dashed(),
            Series::new(
                l.t("grid (+ import)", "net (+ afname)"),
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
        lang: l,
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
        lang: l,
    };
    html! {
        section {
            h2 { (l.t("Plan", "Planning")) }
            p.muted { (l.t("The shaded part uses estimated prices; only the current quarter hour would be executed.", "Het gearceerde deel gebruikt geschatte prijzen; alleen het huidige kwartier wordt uitgevoerd.")) }
            (station_line(shared))
            (prices.render(&shared.tz))
            (power.render(&shared.tz))
            (soc.render(&shared.tz))
        }
    }
}

fn history_section(shared: &Shared, history: &[HistorySlot], now: Timestamp) -> Markup {
    let l = shared.lang();
    if history.is_empty() {
        return html! { section { h2 { (l.t("Last 24 hours", "Afgelopen 24 uur")) } p.muted { (l.t("Nothing recorded yet.", "Nog niets gemeten.")) } } };
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
            Series::new(
                l.t("measured", "gemeten"),
                "s-grid",
                Kind::Step,
                values(|h| Some(h.grid)),
            ),
            Series::new(
                l.t("dess-oxide plan", "planning dess-oxide"),
                "s-plan",
                Kind::Step,
                values(|h| h.planned_grid),
            )
            .dashed(),
            Series::new(
                l.t("ESS setpoint (DAO)", "ESS-setpoint (DAO)"),
                "s-dao",
                Kind::Step,
                values(|h| h.setpoint),
            ),
        ],
        unit: (l.t("grid kW", "net kW")),
        height: 200.0,
        now: Some(now),
        shade_from: None,
        y_range: None,
        lang: l,
    };
    let load = Chart {
        slots: &slots,
        series: vec![
            Series::new(
                l.t("load measured", "verbruik gemeten"),
                "s-load",
                Kind::Step,
                values(|h| Some(h.load)),
            ),
            Series::new(
                l.t("load forecast", "verbruiksverwachting"),
                "s-forecast",
                Kind::Step,
                values(|h| h.forecast_load),
            )
            .dashed(),
            Series::new(
                l.t("PV measured", "PV gemeten"),
                "s-pv",
                Kind::Step,
                values(|h| Some(h.pv)),
            ),
            Series::new(
                l.t("PV forecast", "PV-verwachting"),
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
        lang: l,
    };
    html! {
        section {
            h2 { (l.t("Last 24 hours", "Afgelopen 24 uur")) }
            p.muted { (l.t("What happened, what dess-oxide planned for it, and the setpoint DAO actually ran. No setpoint line: ESS was in external control (DAO's bypass, battery idle).", "Wat er gebeurde, wat dess-oxide ervoor had gepland, en het setpoint dat DAO echt draaide. Geen setpointlijn: ESS stond op externe sturing (de bypass van DAO, accu stil).")) }
            (grid.render(&shared.tz))
            (load.render(&shared.tz))
            (forecast_errors(l, history))
        }
    }
}

fn money_section(
    shared: &Shared,
    money: &[(&'static str, Money)],
    comparison: Option<Comparison>,
) -> Markup {
    let l = shared.lang();
    if money.iter().all(|(_, m)| m.hours == 0.0) {
        return html! {};
    }
    let replayed = comparison.filter(|c| c.hours > 0.0).map(|c| {
        html! {
            h3 { (l.t("The last week, replayed", "De afgelopen week, nagespeeld")) }
            p.muted {
                (l.t("dess-oxide's own plans, made with the forecasts it had at the time, and its policy, run over the ", "De eigen planningen van dess-oxide, gemaakt met de verwachtingen van dat moment, en de sturing, nagespeeld met "))
                (l.t("same load, PV and prices as actually happened (", "hetzelfde verbruik, dezelfde PV en dezelfde prijzen als er echt waren (")) (format!("{:.0}", c.hours)) (l.t(" hours", " uur"))
                @if let Some(at) = c.made_at { (l.t(", replayed ", ", nagespeeld ")) (local(shared, at, "%a %H:%M")) }
                (l.t("). Each is net of the change in stored energy. Perfect foresight is the best any strategy could do.", "). Steeds na verrekening van de verandering in opgeslagen energie. Perfecte voorkennis is het beste dat welke strategie dan ook kan halen."))
            }
            div.scroll {
                table {
                    thead { tr { th { "" } th { (l.t("cost", "kosten")) } th { (l.t("saved against no battery", "bespaard t.o.v. geen accu")) } } }
                    tbody {
                        @for (label, cost) in [
                            ((l.t("what happened", "wat er gebeurde")), c.actual),
                            ((l.t("dess-oxide, replayed", "dess-oxide, nagespeeld")), c.replayed),
                            ((l.t("perfect foresight", "perfecte voorkennis")), c.perfect),
                            ((l.t("without the battery", "zonder accu")), c.without_battery),
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
            h2 { (l.t("Money", "Kosten")) }
            p.muted {
                (l.t("What was imported at the buy price minus what was exported at the sell price, from the recordings. ", "Wat er is afgenomen tegen de leveringsprijs, min wat er is teruggeleverd tegen de terugleverprijs, uit de metingen. "))
                (l.t("Without the battery: the same load and PV each quarter hour. ", "Zonder accu: hetzelfde verbruik en dezelfde PV per kwartier. "))
                (l.t("While DAO is in control, this is DAO's result.", "Zolang DAO stuurt, is dit het resultaat van DAO."))
            }
            div.scroll {
                table {
                    thead { tr { th { "" } th { (l.t("cost", "kosten")) } th { (l.t("without the battery", "zonder accu")) } th { (l.t("saved", "bespaard")) } th { (l.t("recorded", "gemeten")) } } }
                    tbody {
                        @for (label, m) in money {
                            tr {
                                td { (label) }
                                td { (eur(m.actual)) }
                                td { (eur(m.without_battery)) }
                                td { (eur(m.without_battery - m.actual)) }
                                td { (format!("{:.1} {}", m.hours, l.t("h", "u"))) }
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

fn accuracy_section(l: Lang, accuracy: &[LeadAccuracy], days: &[crate::accuracy::Day]) -> Markup {
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
            h2 { (l.t("Forecast accuracy", "Nauwkeurigheid van de verwachtingen")) }
            @if !days.is_empty() {
                p.muted {
                    (l.t("Per day: what the last plan before midnight expected → what happened, and the forecast's error. ", "Per dag: wat de laatste planning vóór middernacht verwachtte → wat er gebeurde, en de afwijking van de verwachting. "))
                    (l.t("PV counts only quarter hours with the PV on. Base load and heat pump are split once the heat pump ", "PV telt alleen kwartieren met de PV aan. Basisverbruik en warmtepomp worden gesplitst zodra het warmtepompmodel "))
                    (l.t("model makes the forecast and the meter's hourly statistics are in (they come in every six hours).", "de verwachting maakt en de uurstatistieken van de meter binnen zijn (die komen elke zes uur)."))
                }
                div.scroll {
                    table {
                        thead { tr { th { (l.t("day", "dag")) } th { "PV" } th { (l.t("base load", "basisverbruik")) } th { (l.t("heat pump", "warmtepomp")) } th { (l.t("house load", "huisverbruik")) } th { (l.t("hours", "uren")) } } }
                        tbody {
                            @for day in days {
                                tr {
                                    td { (date_label(l, day.date)) }
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
                (l.t("The last 7 days, by how far ahead the forecast was made: mean absolute error, and bias ", "De afgelopen 7 dagen, naar hoe ver vooruit de verwachting was gemaakt: gemiddelde absolute fout, en afwijking "))
                (l.t("(positive = forecast too high), as a share of the actual mean. PV counts daylight slots with PV on.", "(positief = verwachting te hoog), als deel van het werkelijke gemiddelde. PV telt kwartieren met daglicht en de PV aan."))
            }
            div.scroll {
                table {
                    thead { tr { th { (l.t("ahead", "vooruit")) } th { (l.t("load error", "fout verbruik")) } th { (l.t("load bias", "afwijking verbruik")) } th { (l.t("PV error", "fout PV")) } th { (l.t("PV bias", "afwijking PV")) } th { (l.t("forecasts", "verwachtingen")) } } }
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
    let l = shared.lang();
    let [pv, heat_pump, load] = models;
    let number = |v: &serde_json::Value| v.as_f64().unwrap_or(f64::NAN);
    html! {
        section {
            h2 { (l.t("Learned models", "Geleerde modellen")) }
            p.muted { (l.t("Trained shortly after startup and nightly. A model is only used when it beats its baseline on held-out days.", "Getraind kort na het opstarten en elke nacht. Een model wordt pas gebruikt als het beter is dan zijn basislijn op achtergehouden dagen.")) }
            h3 { "PV" }
            @match pv {
                None => p.muted { (l.t("Not trained yet: it needs two weeks of history with weather, and [[pv]] arrays to start from.", "Nog niet getraind: het heeft twee weken historie met weer nodig, en panelenvelden (pv) om mee te beginnen.")) },
                Some(model) => {
                    (model_summary(shared, model, "configured_validation_mae_kwh", l.t("the configured arrays", "de ingestelde panelenvelden")))
                    div.scroll {
                        table {
                            thead { tr { th { (l.t("array", "veld")) } th { (l.t("effective kWp", "effectief kWp")) } th { (l.t("tilt", "hellingshoek")) } th { (l.t("azimuth", "azimut")) } } }
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
                        (l.t("Effective kWp includes system losses; the learned arrays needn't match the physical strings. ", "Effectief kWp is inclusief systeemverliezen; de geleerde velden hoeven niet overeen te komen met de echte strings. "))
                        @if kwp * 1.1 < cap {
                            (l.t("The output never gets near the inverter's limit, so the fitted limit (", "De opbrengst komt nooit in de buurt van de grens van de omvormer, dus de geschatte grens ("))
                            (format!("{cap:.1} kW")) (l.t(") only sits above everything recorded and has no effect.", ") ligt alleen boven alles wat gemeten is en heeft geen effect."))
                        } @else {
                            (l.t("Output levels off at about ", "De opbrengst vlakt af bij ongeveer ")) (format!("{cap:.1} kW")) (l.t(": the inverter's limit as the recordings show it.", ": de grens van de omvormer zoals de metingen die laten zien."))
                        }
                    }
                }
            }
            h3 { (l.t("Heat pump", "Warmtepomp")) }
            @match heat_pump.as_ref().and_then(|m| crate::training::hp_model_from_json(&m.params).map(|hp| (m, hp))) {
                None => p.muted { (l.t("Not trained yet: it needs two weeks of the heat pump meter's history (history.heat_pump).", "Nog niet getraind: het heeft twee weken historie van de warmtepompmeter nodig (history.heat_pump).")) },
                Some((stored, hp)) => {
                    (model_summary(shared, stored, "baseline_mae_kwh", l.t("last week's same hours", "dezelfde uren vorige week")))
                    ul {
                        li { (l.t("Heating stops above about ", "Verwarmen stopt boven ongeveer ")) strong { (format!("{:.1} °C", hp.balance_c)) } (l.t(" outdoors, smoothed by the house's thermal lag.", " buiten, gedempt door de traagheid van het huis.")) }
                        li {
                            (l.t("Heating at a steady 0 °C takes about ", "Verwarmen bij constant 0 °C kost ongeveer ")) strong { (format!("{:.1} kW", hp.heating_kw(0.0))) }
                            " (" (format!("{:.0}", hp.heating_kw(0.0) * 24.0)) (l.t(" kWh a day), at −7 °C about ", " kWh per dag), bij −7 °C ongeveer "))
                            (format!("{:.1} kW", hp.heating_kw(-7.0))) " (" (format!("{:.0}", hp.heating_kw(-7.0) * 24.0)) (l.t(" kWh a day); wind adds ", " kWh per dag); wind voegt "))
                            (format!("{:.0}", hp.wind_factor * 100.0)) (l.t(" % per m/s.", " % per m/s toe."))
                        }
                        li { (l.t("Frost: +", "Rijp: +")) (format!("{:.1}", hp.frost_factor * 100.0)) (l.t(" % use per hPa of frost potential.", " % verbruik per hPa rijppotentieel.")) }
                        li { (l.t("Standby: ", "Stand-by: ")) (format!("{:.0} W", hp.standby_kwh * 1000.0)) "." }
                        @if shared.config.openamber().is_none() {
                            li { (l.t("Hot water: ", "Tapwater: ")) (format!("{:.1}", hp.hot_water_kwh.iter().sum::<f64>())) (l.t(" kWh a day, most around ", " kWh per dag, meestal rond ")) (hot_water_peak(&hp.hot_water_kwh)) (l.t(". Learned from the total by time of day: nothing tells it which hours were hot water.", ". Geleerd uit het totaal per uur van de dag: niets vertelt welke uren tapwater waren.")) }
                        }
                    }
                    p.muted { (l.t("All of this is electricity: without a heat meter (such as a flow meter on OpenAmber) the heat output and the COP aren't known.", "Dit is allemaal elektriciteit: zonder warmtemeter (zoals een flowmeter aan OpenAmber) zijn de warmteafgifte en de COP niet bekend.")) }
                }
            }
            (hot_water_section(shared))
            (history_coverage(shared))
            h3 { (l.t("Base load", "Basisverbruik")) }
            @match load {
                None => p.muted { (l.t("Not trained yet: it needs two weeks of load history.", "Nog niet getraind: het heeft twee weken verbruikshistorie nodig.")) },
                Some(model) => (model_summary(shared, model, "baseline_mae_kwh", l.t("the same hour on the same weekday over the last four weeks", "hetzelfde uur op dezelfde weekdag over de afgelopen vier weken"))),
            }
        }
    }
}

/// Hot water from OpenAmber's mode: the model, and the measured split per day.
fn hot_water_section(shared: &Shared) -> Markup {
    let l = shared.lang();
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
        h3 { (l.t("Hot water (OpenAmber)", "Tapwater (OpenAmber)")) }
        @match &model {
            Some(m) => p {
                (l.t("From ", "Uit ")) (m.days) (l.t(" days with OpenAmber's mode: ", " dagen met de modus van OpenAmber: ")) strong { (format!("{:.1} kWh", m.base_kwh)) }
                (l.t(" a day at 15 °C and warmer", " per dag bij 15 °C en warmer"))
                @if m.per_degree_kwh > 0.0 { ", " (format!("{:+.2}", m.per_degree_kwh)) (l.t(" kWh per degree colder", " kWh per graad kouder")) }
                (l.t(", mostly around ", ", meestal rond ")) (hot_water_peak(&m.profile)) (l.t(" (daily error ", " (fout per dag ")) (format!("{:.2}", m.daily_mae)) " kWh)."
                @if m.legionella_kwh > 0.0 { (l.t(" A legionella run takes ", " Een legionellarun kost ")) (format!("{:.1} kWh", m.legionella_kwh)) "." }
                (l.t(" Heating is learned without it, and it's forecast on its own.", " Verwarmen wordt zonder tapwater geleerd, en tapwater wordt apart voorspeld."))
            },
            None => p.muted { (l.t("Hot water gets its own forecast after a week of days with OpenAmber's mode (the recorder's history counts).", "Tapwater krijgt een eigen verwachting na een week aan dagen met de modus van OpenAmber (de historie van de recorder telt mee).")) },
        }
        @if let Some(at) = next { p.muted { (l.t("Next legionella run: ", "Volgende legionellarun: ")) (local(shared, at, "%a %d %b %H:%M")) "." } }
        @if !days.is_empty() {
            div.scroll {
                table {
                    thead { tr { th { (l.t("day", "dag")) } th { (l.t("heating", "verwarming")) } th { (l.t("hot water", "tapwater")) } th { "legionella" } th { (l.t("mode known", "modus bekend")) } } }
                    tbody {
                        @for (date, [heating, hot_water, legionella, hours]) in days.iter().rev() {
                            tr {
                                td { (date_label(l, *date)) }
                                td { (format!("{heating:.1} kWh")) }
                                td { (format!("{hot_water:.1} kWh")) }
                                td { @if *legionella > 0.005 { (format!("{legionella:.1} kWh")) } @else { "–" } }
                                td { (format!("{hours:.0} {}", l.t("h", "u"))) }
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
    let l = shared.lang();
    let coverage = lock(&shared.store).ha_coverage().unwrap_or_default();
    let roles = shared.config.history.entities();
    if coverage.is_empty() || roles.is_empty() {
        return html! {};
    }
    html! {
        details {
            summary { (l.t("History imported from Home Assistant", "Historie geïmporteerd uit Home Assistant")) }
            div.scroll {
                table {
                    thead { tr { th { "sensor" } th { (l.t("from", "vanaf")) } th { (l.t("until", "tot")) } th { (l.t("hours", "uren")) } } }
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
                                    None => { td colspan="3" { (l.t("nothing imported", "niets geïmporteerd")) } }
                                }
                            }
                        }
                    }
                }
            }
            p.muted {
                (l.t("Weather is archived from 2024-07-01. The heat pump model trains on the hours with its meter and weather; ", "Het weer is gearchiveerd vanaf 2024-07-01. Het warmtepompmodel traint op de uren met zijn meter en weer; "))
                (l.t("the base load on the hours where every sensor has a value (or dess-oxide recorded the house itself).", "het basisverbruik op de uren waarin elke sensor een waarde heeft (of waarin dess-oxide het huis zelf heeft gemeten)."))
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
    let l = shared.lang();
    let number = |key: &str| model.metrics[key].as_f64().unwrap_or(f64::NAN);
    html! {
        p {
            (l.t("Trained ", "Getraind ")) (local(shared, model.trained_at, "%a %d %b %H:%M")) (l.t(" on ", " op ")) (model.metrics["hours"]) (l.t(" hours. Held-out error ", " uur. Fout op achtergehouden dagen "))
            (format!("{:.3}", number("validation_mae_kwh"))) (l.t(" kWh/h, against ", " kWh/u, tegen ")) (format!("{:.3}", number(baseline_key)))
            (l.t(" for ", " voor ")) (baseline) ": "
            @if model.promoted { strong { (l.t("in use", "in gebruik")) } } @else { (l.t("not better yet, so not used", "nog niet beter, dus niet in gebruik")) }
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
    l: Lang,
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
        h3 { (l.t("Battery and inverters", "Accu en omvormers")) }
        p {
            (l.t("Usable capacity ", "Bruikbare capaciteit ")) strong { (kwh(battery.capacity.0)) }
            @match capacity.filter(|c| c.usable_wh().is_some()) {
                Some(c) => {
                    (l.t(", learned from ", ", geleerd uit ")) (c.stretches) (l.t(" long charge or discharge stretches", " lange laad- of ontlaadreeksen"))
                    @if let (Some(charge), Some(discharge)) = (c.charge_wh, c.discharge_wh) {
                        (l.t(" (charging ", " (laden ")) (kwh(charge)) (l.t(", discharging ", ", ontladen ")) (kwh(discharge))
                        @if let Some(rt) = c.round_trip() { (l.t(": the cells' own round trip is ", ": het eigen rondrendement van de cellen is ")) (format!("{:.1} %", rt * 100.0)) }
                        ")"
                    }
                    (l.t(", unless set in the options.", ", tenzij ingesteld in de opties."))
                }
                None => (l.t(", from the options or the GX device until it has seen a 30 % stretch each way.", ", uit de opties of het GX-apparaat totdat er in beide richtingen een reeks van 30 % is gezien.")),
            }
        }
        p {
            (l.t("Charging curve: ", "Laadcurve: ")) (if learned(true) { l.t("learned", "geleerd") } else { l.t("prior (not enough steady data yet)", "aanname (nog niet genoeg stabiele gegevens)") })
            (l.t("; discharging: ", "; ontladen: ")) (if learned(false) { l.t("learned", "geleerd") } else { l.t("prior", "aanname") })
            (l.t(". Standby ", ". Stand-by ")) (format!("{:.0} W", battery.standby.0))
            (if losses.is_some_and(|l| l.standby.is_some()) { l.t(" (learned)", " (geleerd)") } else { l.t(" (prior)", " (aanname)") }) "."
            @if let Some(bypass) = battery.bypass_draw {
                (l.t(" In bypass (ESS in external control, the battery idle) the inverters draw ", " In bypass (ESS op externe sturing, accu stil) gebruiken de omvormers "))
                (format!("{:.0} W", bypass.0)) (if bypass_learned { l.t(" (measured)", " (gemeten)") } else { l.t(" (prior)", " (aanname)") })
                (l.t(", so when the battery has nothing worthwhile to do the plan holds it in bypass rather than trickling.", ", dus als de accu niets zinnigs te doen heeft, zet de planning hem in bypass in plaats van druppelsgewijs te laden of ontladen."))
            }
        }
        div.scroll {
            table {
                thead { tr { th { (l.t("AC power", "AC-vermogen")) } th { (l.t("charge efficiency", "laadrendement")) } th { (l.t("discharge efficiency", "ontlaadrendement")) } } }
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
        p.muted { (l.t("Conversion efficiency without the standby draw, which the planner counts separately.", "Omzettingsrendement zonder het stand-byverbruik, dat de planner apart meerekent.")) }
    }
}

/// Mean absolute error of the baseline forecasts over the history.
fn forecast_errors(l: Lang, history: &[HistorySlot]) -> Markup {
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
            (l.t("Mean absolute forecast error: ", "Gemiddelde absolute fout van de verwachting: "))
            @match load { Some((e, n)) => { (l.t("load ", "verbruik ")) (format!("{e:.2} kW")) " over " (n) (l.t(" slots", " kwartieren")) }, None => (l.t("load –", "verbruik –")) }
            "; "
            @match pv { Some((e, n)) => { "PV " (format!("{e:.2} kW")) " over " (n) (l.t(" slots", " kwartieren")) }, None => "PV –" }
            (l.t(". Each uses the learned model once it's in use (see below), else the baseline.", ". Elk gebruikt het geleerde model zodra dat in gebruik is (zie hieronder), anders de basislijn."))
        }
    }
}

fn slot_table(shared: &Shared, view: &PlanView) -> Markup {
    let l = shared.lang();
    html! {
        details {
            summary { (l.t("All ", "Alle ")) (view.plan.slots.len()) (l.t(" planned slots", " geplande kwartieren")) }
            div.scroll {
                table {
                    thead { tr { th { (l.t("slot", "kwartier")) } th { (l.t("buy", "levering")) } th { (l.t("sell", "teruglevering")) } th { (l.t("load", "verbruik")) } th { "PV" } th { (l.t("battery", "accu")) } th { (l.t("grid", "net")) } th { "SoC" } th { (l.t("PV on", "PV aan")) } } }
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
                                td { (if s.pv_on { l.t("on", "aan") } else { l.t("off", "uit") }) }
                            }
                        }
                    }
                }
            }
        }
    }
}

fn local(shared: &Shared, at: Timestamp, format: &str) -> String {
    shared
        .lang()
        .strftime(&at.to_zoned(shared.tz.clone()), format)
}

/// Dutch numbers: a decimal comma between digits in the page's text, but
/// not inside tags (attributes, the charts' data) or scripts and styles.
fn decimal_commas(page: &str) -> String {
    let mut out = String::with_capacity(page.len());
    let mut chars = page.char_indices().peekable();
    let mut in_tag = false;
    let mut raw_until: Option<&str> = None;
    let mut previous = ' ';
    while let Some((i, c)) = chars.next() {
        if let Some(end) = raw_until {
            if page[i..].starts_with(end) {
                raw_until = None;
            }
        } else if c == '<' {
            in_tag = true;
            if page[i..].starts_with("<script") {
                raw_until = Some("</script>");
            } else if page[i..].starts_with("<style") {
                raw_until = Some("</style>");
            }
        } else if c == '>' {
            in_tag = false;
        } else if c == '.'
            && !in_tag
            && previous.is_ascii_digit()
            && chars.peek().is_some_and(|(_, next)| next.is_ascii_digit())
        {
            out.push(',');
            previous = c;
            continue;
        }
        out.push(c);
        previous = c;
    }
    out
}

/// Why selling pays less than buying, when the tariff makes it so.
fn sell_note(shared: &Shared, slot: Slot) -> Option<&'static str> {
    let l = shared.lang();
    let tariff = shared.tariff.as_ref()?;
    let date = slot.start().to_zoned(tariff.time_zone.clone()).date();
    let netted = tariff.net_metering_until.is_some_and(|until| date <= until);
    if !netted {
        return Some(l.t(
            "net metering has ended: no energy tax back",
            "salderen is voorbij: geen energiebelasting terug",
        ));
    }
    let markups = (
        tariff.markup_buy.at(date).ok()?,
        tariff.markup_sell.at(date).ok()?,
    );
    ((markups.0 - markups.1).0.abs() > 1e-6).then(|| {
        l.t(
            "the markups for buying and selling differ",
            "de opslagen voor levering en teruglevering verschillen",
        )
    })
}

/// A day as "Sun 27 Sep" (or "zo 27 sep").
fn date_label(l: Lang, date: jiff::civil::Date) -> String {
    date.to_zoned(jiff::tz::TimeZone::UTC)
        .map_or_else(|_| date.to_string(), |z| l.strftime(&z, "%a %d %b"))
}

/// Why control isn't acting, in the page's language.
fn reason(l: Lang, reason: &str) -> String {
    if l == Lang::En {
        return reason.to_owned();
    }
    let nl = match reason {
        "switched off on this page" => "uitgezet op deze pagina",
        "Victron Dynamic ESS is enabled; switch it off first" => {
            "Victrons Dynamic ESS staat aan; zet die eerst uit"
        }
        "ESS isn't set to regulate the total of all phases (Hub4Mode 1)" => {
            "ESS regelt niet het totaal van alle fasen (Hub4Mode 1)"
        }
        "something else is writing the ESS setpoint (DAO's automations?); waiting for it to stop" => {
            "iets anders schrijft het ESS-setpoint (de automatiseringen van DAO?); wacht tot dat stopt"
        }
        "no recent data from the GX device" => "geen recente gegevens van het GX-apparaat",
        "the grid is down: plain ESS runs the island" => {
            "het net is weg: gewone ESS draait het eiland"
        }
        "the latest plan is too old" => "de laatste planning is te oud",
        "no plan yet" => "nog geen planning",
        "the plan doesn't cover now" => "de planning dekt dit moment niet",
        other if other.starts_with("ESS is in external control") => {
            "ESS staat op externe sturing (Hub4Mode 3), bijvoorbeeld de bypass van DAO; zet hem terug op \"Optimized, total of all phases\""
        }
        other => other,
    };
    nl.to_owned()
}

fn severity_label(l: Lang, severity: Severity) -> &'static str {
    match severity {
        Severity::Blocker => l.t("Blocker:", "Blokkade:"),
        Severity::Warning => l.t("Warning:", "Waarschuwing:"),
        Severity::Info => "Info:",
    }
}

/// Hover tooltips and legend toggles for the charts, and a refresh every
/// minute that waits while someone is looking at a value. Progressive: the
/// page works without it.
const SCRIPT: &str = r#"
document.addEventListener('DOMContentLoaded', () => {
  const locale = document.documentElement.lang || 'en';
  const fixed = (v, d) => v.toLocaleString(locale, { minimumFractionDigits: d, maximumFractionDigits: d });
  const fmt = v => v == null ? '–' : fixed(v, Math.abs(v) >= 100 ? 0 : Math.abs(v) >= 10 ? 1 : 2);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dutch_numbers_only_in_text() {
        let page = r#"<p class="x">€0.379 and 12.5 kW</p><script>let a = 1.5;</script><figure data-chart="{&quot;v&quot;:[0.5]}"><svg><text>0.2</text></svg></figure>"#;
        assert_eq!(
            decimal_commas(page),
            r#"<p class="x">€0,379 and 12,5 kW</p><script>let a = 1.5;</script><figure data-chart="{&quot;v&quot;:[0.5]}"><svg><text>0,2</text></svg></figure>"#
        );
    }
}
