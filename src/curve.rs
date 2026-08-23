//! Pricing a pump.fun token from its bonding curve account, without an aggregator.
//!
//! Jupiter was the binding constraint on this bot: roughly one quote per second
//! against ~1300 candidates an hour, which produced 18-26% screen timeouts and
//! starved the migration venue entirely. It is also a third-party round trip
//! sitting in the middle of the hot path, measured at ~170ms.
//!
//! For a brand-new pump.fun mint that price is also unnecessary. A router that
//! aggregates dozens of venues is the wrong tool for a token that has exactly
//! one venue, whose price is a pure function of two numbers held in a single
//! account. One getAccountInfo answers it, from an RPC that is not the
//! bottleneck, with no queue in front of it.
//!
//! The curve is a constant product. Selling `dt` tokens into reserves
//! (`vt`, `vs`) returns:
//!
//!     sol_out = vs - (vs * vt) / (vt + dt)
//!
//! which is impact-inclusive at the size actually being sold - the same
//! property that made the Jupiter mark honest, kept for free.
//!
//! WHAT THIS DELIBERATELY DOES NOT REPLACE: the routing check in
//! screen/routing.rs. That exists to prove a SELL is possible at all - the
//! honeypot test - and a curve read cannot answer it, because the curve says
//! what the maths would pay, not whether the token's own transfer rules will
//! let the trade settle. Pricing moves here; the safety question stays there.

use anyhow::{anyhow, Result};

/// Anchor prefixes every account with an 8-byte discriminator.
const DISCRIMINATOR: usize = 8;
const LAMPORTS_PER_SOL: f64 = 1_000_000_000.0;

/// The reserves that define the curve's price, as held on chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BondingCurve {
    pub virtual_token_reserves: u64,
    pub virtual_sol_reserves: u64,
    pub real_token_reserves: u64,
    pub real_sol_reserves: u64,
    pub token_total_supply: u64,
    /// Set once the curve has filled and the token has migrated to a pool.
    /// After that this account no longer prices anything tradeable.
    pub complete: bool,
}

impl BondingCurve {
    /// Decode the account data. Layout is discriminator, then five u64 in
    /// little-endian, then a bool.
    pub fn parse(data: &[u8]) -> Result<Self> {
        let need = DISCRIMINATOR + 5 * 8 + 1;
        if data.len() < need {
            return Err(anyhow!(
                "bonding curve account too short: {} bytes, need {need}",
                data.len()
            ));
        }
        let at = |i: usize| -> u64 {
            let o = DISCRIMINATOR + i * 8;
            u64::from_le_bytes(data[o..o + 8].try_into().expect("8 bytes checked above"))
        };
        Ok(Self {
            virtual_token_reserves: at(0),
            virtual_sol_reserves: at(1),
            real_token_reserves: at(2),
            real_sol_reserves: at(3),
            token_total_supply: at(4),
            complete: data[DISCRIMINATOR + 40] != 0,
        })
    }

    /// SOL received for selling `tokens` whole tokens into the curve, divided by
    /// the tokens sold - i.e. the achievable average price, impact included.
    ///
    /// Returns None when the curve cannot price the trade rather than a
    /// misleading zero: a completed curve holds no tradeable liquidity, and
    /// empty reserves are not a price of zero, they are an absence of one.
    pub fn sell_price(&self, tokens: f64, decimals: u8) -> Option<f64> {
        if self.complete || tokens <= 0.0 {
            return None;
        }
        let vt = self.virtual_token_reserves as f64;
        let vs = self.virtual_sol_reserves as f64;
        if vt <= 0.0 || vs <= 0.0 {
            return None;
        }
        let raw_in = tokens * 10f64.powi(decimals as i32);
        if !raw_in.is_finite() || raw_in <= 0.0 {
            return None;
        }
        let sol_out = vs - (vs * vt) / (vt + raw_in);
        if !sol_out.is_finite() || sol_out <= 0.0 {
            return None;
        }
        let price = (sol_out / LAMPORTS_PER_SOL) / tokens;
        if price.is_finite() && price > 0.0 {
            Some(price)
        } else {
            None
        }
    }
}

/// Minimal base64 decoder.
///
/// getAccountInfo returns account data base64-encoded and this crate has no
/// base64 dependency. Twenty lines is a better trade than another crate on the
/// hot path, and the alphabet is fixed.
pub fn base64_decode(s: &str) -> Result<Vec<u8>> {
    fn val(c: u8) -> Option<u32> {
        match c {
            b'A'..=b'Z' => Some((c - b'A') as u32),
            b'a'..=b'z' => Some((c - b'a') as u32 + 26),
            b'0'..=b'9' => Some((c - b'0') as u32 + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let bytes: Vec<u8> = s.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    let mut acc: u32 = 0;
    let mut bits = 0u32;
    for &b in &bytes {
        if b == b'=' {
            break;
        }
        let v = val(b).ok_or_else(|| anyhow!("invalid base64 byte {b:#x}"))?;
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build an account body the way the chain lays it out.
    fn account(vt: u64, vs: u64, complete: bool) -> Vec<u8> {
        let mut d = vec![0u8; DISCRIMINATOR];
        d.extend_from_slice(&vt.to_le_bytes());
        d.extend_from_slice(&vs.to_le_bytes());
        d.extend_from_slice(&0u64.to_le_bytes());
        d.extend_from_slice(&0u64.to_le_bytes());
        d.extend_from_slice(&1_000_000_000_000_000u64.to_le_bytes());
        d.push(complete as u8);
        d
    }

    // The values a freshly created pump.fun curve actually carries.
    const FRESH_VT: u64 = 1_073_000_000_000_000;
    const FRESH_VS: u64 = 30_000_000_000;

    #[test]
    fn a_fresh_curve_parses_to_its_known_reserves() {
        let c = BondingCurve::parse(&account(FRESH_VT, FRESH_VS, false)).unwrap();
        assert_eq!(c.virtual_token_reserves, FRESH_VT);
        assert_eq!(c.virtual_sol_reserves, FRESH_VS);
        assert!(!c.complete);
    }

    #[test]
    fn a_truncated_account_is_an_error_not_a_guess() {
        assert!(BondingCurve::parse(&[0u8; 20]).is_err());
        assert!(BondingCurve::parse(&[]).is_err());
    }

    // Sanity against the real market: a fresh curve prices a token at roughly
    // 30 SOL / 1.073e9 tokens, about 2.8e-8 SOL each. Every live entry price
    // this bot journalled sat in that neighbourhood.
    #[test]
    fn a_fresh_curve_prices_near_the_observed_launch_price() {
        let c = BondingCurve::parse(&account(FRESH_VT, FRESH_VS, false)).unwrap();
        let p = c.sell_price(1.0, 6).expect("fresh curve prices");
        assert!(
            (2.0e-8..4.0e-8).contains(&p),
            "priced {p:e}, expected ~2.8e-8"
        );
    }

    // Impact must be size-dependent, which is the whole reason the Jupiter mark
    // was taken at the held size. Selling more must fetch a worse average price.
    #[test]
    fn selling_more_gets_a_worse_average_price() {
        let c = BondingCurve::parse(&account(FRESH_VT, FRESH_VS, false)).unwrap();
        let small = c.sell_price(1_000.0, 6).unwrap();
        let large = c.sell_price(50_000_000.0, 6).unwrap();
        assert!(large < small, "impact not applied: {large:e} vs {small:e}");
    }

    // A completed curve has migrated; it holds nothing to sell into. Returning
    // a price here would mark positions against liquidity that has moved.
    #[test]
    fn a_completed_curve_refuses_to_price() {
        let c = BondingCurve::parse(&account(FRESH_VT, FRESH_VS, true)).unwrap();
        assert!(c.sell_price(1.0, 6).is_none());
    }

    #[test]
    fn empty_reserves_are_no_price_rather_than_a_zero() {
        let c = BondingCurve::parse(&account(0, 0, false)).unwrap();
        assert!(c.sell_price(1.0, 6).is_none());
    }

    #[test]
    fn nonsense_sizes_do_not_produce_a_price() {
        let c = BondingCurve::parse(&account(FRESH_VT, FRESH_VS, false)).unwrap();
        assert!(c.sell_price(0.0, 6).is_none());
        assert!(c.sell_price(-5.0, 6).is_none());
    }

    #[test]
    fn base64_round_trips_the_shapes_the_rpc_returns() {
        assert_eq!(base64_decode("").unwrap(), Vec::<u8>::new());
        assert_eq!(base64_decode("QQ==").unwrap(), b"A");
        assert_eq!(base64_decode("QUI=").unwrap(), b"AB");
        assert_eq!(base64_decode("QUJD").unwrap(), b"ABC");
        // whitespace appears in some responses and must not corrupt the output
        assert_eq!(base64_decode("QUJD\n").unwrap(), b"ABC");
    }

    #[test]
    fn base64_rejects_junk_rather_than_returning_partial_data() {
        assert!(base64_decode("!!!!").is_err());
    }

    #[test]
    fn a_real_account_body_survives_the_full_round_trip() {
        use std::fmt::Write;
        let raw = account(FRESH_VT, FRESH_VS, false);
        // encode it the way the RPC would
        const A: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut enc = String::new();
        for chunk in raw.chunks(3) {
            let b = [
                chunk[0],
                *chunk.get(1).unwrap_or(&0),
                *chunk.get(2).unwrap_or(&0),
            ];
            let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
            for i in 0..4 {
                if i <= chunk.len() {
                    let _ = write!(enc, "{}", A[((n >> (18 - i * 6)) & 63) as usize] as char);
                } else {
                    enc.push('=');
                }
            }
        }
        let back = base64_decode(&enc).unwrap();
        let c = BondingCurve::parse(&back).unwrap();
        assert_eq!(c.virtual_sol_reserves, FRESH_VS);
        assert_eq!(c.virtual_token_reserves, FRESH_VT);
    }
}
