//! What the planner knows about the battery and the inverter/chargers.
//!
//! Losses are split into a constant standby draw (the inverters are on either
//! way, so it doesn't depend on decisions) and a conversion loss that grows
//! with power: `loss(P) = b·|P| + c·P²` on the AC side. M2 learns these from
//! the recorded efficiency bins; until then a prior for MultiPlus-II units is
//! used.

use crate::units::{WattHours, Watts};

/// Power-dependent conversion loss, `b·|P| + c·P²` with `P` the AC power.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LossCurve {
    pub linear: f64,
    /// Per watt: the loss at `P` includes `quadratic · P²` watts.
    pub quadratic: f64,
}

impl LossCurve {
    /// AC power needed to put `dc` W (≥ 0) into the battery, or `None` if no
    /// AC power can deliver that much.
    pub fn ac_for_charging(self, dc: f64) -> Option<f64> {
        // ac − (b·ac + c·ac²) = dc  →  c·ac² − (1 − b)·ac + dc = 0, smaller root.
        let (b, c) = (self.linear, self.quadratic);
        if c == 0.0 {
            return Some(dc / (1.0 - b));
        }
        let discriminant = (1.0 - b).powi(2) - 4.0 * c * dc;
        (discriminant >= 0.0).then(|| ((1.0 - b) - discriminant.sqrt()) / (2.0 * c))
    }

    /// AC power delivered when taking `dc` W (≥ 0) out of the battery.
    pub fn ac_from_discharging(self, dc: f64) -> f64 {
        // ac + b·ac + c·ac² = dc  →  c·ac² + (1 + b)·ac − dc = 0, positive root.
        let (b, c) = (self.linear, self.quadratic);
        if c == 0.0 {
            return dc / (1.0 + b);
        }
        (-(1.0 + b) + ((1.0 + b).powi(2) + 4.0 * c * dc).sqrt()) / (2.0 * c)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct BatteryModel {
    /// Usable energy between 0 and 100 % SoC, at the battery terminals.
    pub capacity: WattHours,
    pub charge_loss: LossCurve,
    pub discharge_loss: LossCurve,
    /// Constant draw of the inverter/chargers while they're on.
    pub standby: Watts,
    /// Their draw in bypass (ESS external control with nothing asked of
    /// them: the battery idle, the grid passing through), if the executor
    /// can use bypass. Lower than `standby`, so a battery that has nothing
    /// worthwhile to do is better off in bypass than trickling.
    pub bypass_draw: Option<Watts>,
    /// Most AC power the system can take in while charging.
    pub max_charge_ac: Watts,
    /// Most AC power the system can deliver while discharging.
    pub max_discharge_ac: Watts,
    /// The cells' own efficiency, each way: of the DC energy at the terminals
    /// this share ends up stored, and taking energy out costs `1 / this`.
    pub cell_efficiency: f64,
}

impl BatteryModel {
    /// Prior for `units` MultiPlus-II 48/5000s, until M2 learns the real curve.
    ///
    /// Fitted to the stage table in George's DAO config, with the standby draw
    /// (about 20 W per unit) taken out. Resistive losses scale with the power
    /// per unit, so the quadratic term shrinks with more units.
    pub fn multiplus_ii_prior(
        capacity: WattHours,
        units: u32,
        max_charge_ac: Watts,
        max_discharge_ac: Watts,
    ) -> Self {
        let n = f64::from(units.max(1));
        Self {
            capacity,
            charge_loss: LossCurve {
                linear: 0.027,
                quadratic: 2.3e-5 / n,
            },
            discharge_loss: LossCurve {
                linear: 0.0,
                quadratic: 2.55e-5 / n,
            },
            standby: Watts(20.0 * n),
            bypass_draw: Some(Watts(8.0 * n)),
            max_charge_ac,
            max_discharge_ac,
            // LFP cells lose about 4 % over a round trip, until measured.
            cell_efficiency: 0.98,
        }
    }

    /// How fast the stored energy changes for AC power `ac` (positive =
    /// charging): the inverters' conversion losses and the cells' own.
    pub fn dc_for_ac(&self, ac: Watts) -> Watts {
        let p = ac.0.abs();
        let cells = self.cell_efficiency.clamp(0.5, 1.0);
        if ac.0 >= 0.0 {
            let c = self.charge_loss;
            Watts((p - c.linear * p - c.quadratic * p * p) * cells)
        } else {
            let d = self.discharge_loss;
            Watts(-(p + d.linear * p + d.quadratic * p * p) / cells)
        }
    }

    /// AC power (positive = charging) that changes the stored energy by
    /// `delta` over `hours`, or `None` if that exceeds the limits.
    pub fn ac_power_for(&self, delta: WattHours, hours: f64) -> Option<Watts> {
        let cells = self.cell_efficiency.clamp(0.5, 1.0);
        let stored = delta.0 / hours;
        // At the terminals: more going in than gets stored, less coming out.
        let ac = if stored >= 0.0 {
            self.charge_loss.ac_for_charging(stored / cells)?
        } else {
            -self.discharge_loss.ac_from_discharging(-stored * cells)
        };
        (ac <= self.max_charge_ac.0 + 1e-9 && -ac <= self.max_discharge_ac.0 + 1e-9)
            .then_some(Watts(ac))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stored_energy_counts_the_cells_too() {
        let mut b = prior();
        let with_cells = b.dc_for_ac(Watts(3000.0)).0;
        b.cell_efficiency = 1.0;
        let without = b.dc_for_ac(Watts(3000.0)).0;
        assert!((with_cells - without * 0.98).abs() < 1e-9);
        // And back: storing that much takes the same AC power.
        b.cell_efficiency = 0.98;
        let ac = b
            .ac_power_for(WattHours(with_cells * 0.25), 0.25)
            .unwrap()
            .0;
        assert!((ac - 3000.0).abs() < 1e-6, "{ac}");
    }

    fn prior() -> BatteryModel {
        BatteryModel::multiplus_ii_prior(WattHours(32_000.0), 3, Watts(10_000.0), Watts(11_000.0))
    }

    #[test]
    fn prior_is_close_to_the_dao_table() {
        let m = prior();
        // Efficiency including standby, as DAO's stages express it.
        let charge_eta = |ac: f64| {
            let loss = m.charge_loss.linear * ac + m.charge_loss.quadratic * ac * ac + m.standby.0;
            (ac - loss) / ac
        };
        assert!((charge_eta(3000.0) - 0.93).abs() < 0.01);
        assert!((charge_eta(10_000.0) - 0.89).abs() < 0.01);
    }

    #[test]
    fn charging_and_discharging_invert_the_loss() {
        let curve = prior().charge_loss;
        let ac = curve.ac_for_charging(3000.0).unwrap();
        let loss = curve.linear * ac + curve.quadratic * ac * ac;
        assert!((ac - loss - 3000.0).abs() < 1e-6);

        let curve = prior().discharge_loss;
        let ac = curve.ac_from_discharging(3000.0);
        let loss = curve.linear * ac + curve.quadratic * ac * ac;
        assert!((ac + loss - 3000.0).abs() < 1e-6);
        assert!(ac < 3000.0);
    }

    #[test]
    fn limits_are_enforced_on_the_ac_side() {
        let m = prior();
        assert!(m.ac_power_for(WattHours(2000.0), 0.25).is_some()); // 8 kW DC
        assert!(m.ac_power_for(WattHours(2500.0), 0.25).is_none()); // 10 kW DC needs > 10 kW AC
        assert!(m.ac_power_for(WattHours(-2500.0), 0.25).is_some());
        assert_eq!(m.ac_power_for(WattHours::ZERO, 0.25), Some(Watts(0.0)));
    }
}
