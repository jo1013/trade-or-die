pub struct Strategy {
    pub min_edge: f64,
    pub max_bet_fraction: f64,
    /// Kelly multiplier: 1.0 = full Kelly, 0.5 = half Kelly (default).
    /// Half-Kelly reduces variance ~75% while sacrificing only ~25% expected growth.
    pub kelly_fraction: f64,
}

impl Strategy {
    pub fn new(min_edge: f64, max_bet_fraction: f64) -> Self {
        Self {
            min_edge,
            max_bet_fraction,
            kelly_fraction: 0.5, // Half-Kelly default
        }
    }

    pub fn with_kelly_fraction(mut self, fraction: f64) -> Self {
        self.kelly_fraction = fraction.clamp(0.1, 1.0);
        self
    }

    /// Returns the Kelly-sized bet fraction (already scaled by kelly_fraction).
    /// Uses self.min_edge as the default edge threshold.
    pub fn calculate_kelly_bet(&self, market_price: f64, ai_probability: f64) -> f64 {
        self.calculate_kelly_bet_with_edge(market_price, ai_probability, self.min_edge)
    }

    /// Returns the Kelly-sized bet fraction with a custom min_edge threshold.
    /// Used for category-specific edge thresholds (e.g. sports 8%, politics 12%).
    pub fn calculate_kelly_bet_with_edge(&self, market_price: f64, ai_probability: f64, min_edge: f64) -> f64 {
        if market_price <= 0.01 || market_price >= 0.99 {
            return 0.0;
        }

        let p = ai_probability.clamp(0.001, 0.999);
        let edge = (p - market_price).abs();

        if edge < min_edge {
            return 0.0;
        }

        let raw_kelly = if p > market_price {
            // BUY: market undervalues YES
            let b = (1.0 / market_price) - 1.0;
            let q = 1.0 - p;
            (p * b - q) / b
        } else {
            // SELL: market overvalues YES → equivalent to BUY NO
            let no_price = 1.0 - market_price;
            let no_prob = 1.0 - p;
            let b = (1.0 / no_price) - 1.0;
            let q_no = 1.0 - no_prob;
            (no_prob * b - q_no) / b
        };

        (raw_kelly * self.kelly_fraction).clamp(0.0, self.max_bet_fraction)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn half_kelly_halves_full_kelly() {
        // Use high max_bet_fraction so clamp doesn't interfere
        let full = Strategy { min_edge: 0.08, max_bet_fraction: 1.0, kelly_fraction: 1.0 };
        let half = Strategy { min_edge: 0.08, max_bet_fraction: 1.0, kelly_fraction: 0.5 };

        // Price 0.40, AI prob 0.60 → strong edge
        let full_bet = full.calculate_kelly_bet(0.40, 0.60);
        let half_bet = half.calculate_kelly_bet(0.40, 0.60);

        assert!(full_bet > 0.0, "full kelly should produce a bet");
        assert!((half_bet - full_bet * 0.5).abs() < 1e-10, "half kelly should be exactly half");
    }

    #[test]
    fn no_edge_returns_zero() {
        let s = Strategy::new(0.10, 0.10);
        // Price 0.50, AI prob 0.52 → edge 2% < min_edge 10%
        assert_eq!(s.calculate_kelly_bet(0.50, 0.52), 0.0);
    }

    #[test]
    fn sell_side_kelly() {
        let s = Strategy::new(0.08, 0.25).with_kelly_fraction(0.5);
        // Price 0.70, AI prob 0.50 → edge 20%, SELL side (buy NO)
        let bet = s.calculate_kelly_bet(0.70, 0.50);
        assert!(bet > 0.0, "should bet on NO side");
        assert!(bet <= 0.25, "should not exceed max_bet_fraction");
    }

    #[test]
    fn extreme_prices_return_zero() {
        let s = Strategy::new(0.08, 0.10);
        assert_eq!(s.calculate_kelly_bet(0.005, 0.50), 0.0);
        assert_eq!(s.calculate_kelly_bet(0.995, 0.50), 0.0);
    }

    #[test]
    fn builder_clamps_fraction() {
        let s = Strategy::new(0.08, 0.10).with_kelly_fraction(2.0);
        assert_eq!(s.kelly_fraction, 1.0);
        let s2 = Strategy::new(0.08, 0.10).with_kelly_fraction(0.01);
        assert_eq!(s2.kelly_fraction, 0.1);
    }

    #[test]
    fn custom_edge_threshold_filters_correctly() {
        let s = Strategy::new(0.10, 0.25).with_kelly_fraction(0.5);

        // edge = |0.65 - 0.50| = 0.15 > default min_edge 0.10 → should bet
        assert!(s.calculate_kelly_bet(0.50, 0.65) > 0.0);

        // With custom min_edge 0.12 (politics) → edge 0.15 > 0.12 → should bet
        assert!(s.calculate_kelly_bet_with_edge(0.50, 0.65, 0.12) > 0.0);

        // With custom min_edge 0.20 → edge 0.15 < 0.20 → no bet
        assert_eq!(s.calculate_kelly_bet_with_edge(0.50, 0.65, 0.20), 0.0);

        // With custom min_edge 0.08 (sports) → edge 0.15 > 0.08 → should bet
        assert!(s.calculate_kelly_bet_with_edge(0.50, 0.65, 0.08) > 0.0);

        // Verify sports threshold is more permissive: edge 0.09 passes sports but not politics
        assert!(s.calculate_kelly_bet_with_edge(0.50, 0.59, 0.08) > 0.0);  // sports: 0.09 > 0.08
        assert_eq!(s.calculate_kelly_bet_with_edge(0.50, 0.59, 0.12), 0.0); // politics: 0.09 < 0.12
    }
}
