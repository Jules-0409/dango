// Grok Ball standalone engine | MIT License

/// Seedable Mulberry32 PRNG matching JS implementation exactly.
#[derive(Clone, Debug)]
pub struct Mulberry32 {
    state: u32,
}

impl Mulberry32 {
    pub const fn new(seed: u32) -> Self {
        Self { state: seed }
    }

    /// Returns a pseudo-random f64 in `[0.0, 1.0)`.
    pub fn next_f64(&mut self) -> f64 {
        self.state = self.state.wrapping_add(0x6D2B79F5);
        let mut t = (self.state ^ (self.state >> 15)).wrapping_mul(1 | self.state);
        t ^= t.wrapping_add((t ^ (t >> 7)).wrapping_mul(61 | t));
        (t as f64) / 4294967296.0
    }

    /// Random value in `[a, b)`.
    pub fn rand_range(&mut self, a: f64, b: f64) -> f64 {
        a + self.next_f64() * (b - a)
    }

    /// Random boolean with probability `p`.
    pub fn pick_bool(&mut self, p: f64) -> bool {
        self.next_f64() < p
    }

    /// Random sign: -1.0 with 50% probability, +1.0 otherwise.
    pub fn pick_sign(&mut self) -> f64 {
        if self.next_f64() < 0.5 {
            -1.0
        } else {
            1.0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mulberry32_sequence() {
        let mut rng = Mulberry32::new(42);
        let v1 = rng.next_f64();
        let v2 = rng.next_f64();
        assert!(v1 >= 0.0 && v1 < 1.0);
        assert!(v2 >= 0.0 && v2 < 1.0);
        assert_ne!(v1, v2);
    }
}
