pub struct Strategy {
    pub min_edge: f64,
    pub max_bet_fraction: f64,
}

impl Strategy {
    pub fn new(min_edge: f64, max_bet_fraction: f64) -> Self {
        Self {
            min_edge,
            max_bet_fraction,
        }
    }

    /// Returns (kelly_fraction, direction). direction: 1=BUY, -1=SELL, 0=SKIP
    pub fn calculate_kelly_bet(&self, market_price: f64, ai_probability: f64) -> f64 {
        if market_price <= 0.01 || market_price >= 0.99 {
            return 0.0;
        }

        let p = ai_probability.clamp(0.001, 0.999);
        let edge = (p - market_price).abs();

        if edge < self.min_edge {
            return 0.0;
        }

        if p > market_price {
            // BUY: market undervalues YES
            let b = (1.0 / market_price) - 1.0;
            let q = 1.0 - p;
            let kelly_f = (p * b - q) / b;
            kelly_f.clamp(0.0, self.max_bet_fraction)
        } else {
            // SELL: market overvalues YES → equivalent to BUY NO
            let no_price = 1.0 - market_price;
            let no_prob = 1.0 - p;
            let b = (1.0 / no_price) - 1.0;
            let q_no = 1.0 - no_prob;
            let kelly_f = (no_prob * b - q_no) / b;
            kelly_f.clamp(0.0, self.max_bet_fraction)
        }
    }
}
