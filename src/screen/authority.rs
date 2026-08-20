use serde_json::Value;
use tracing::debug;

use super::MintFacts;
use crate::config::ScreenConfig;
use crate::rpc::SolanaRpc;
use crate::types::{CheckResult, Pubkey};

/// Extensions that provably cannot interfere with your ability to sell.
///
/// This is an ALLOWLIST, deliberately. A denylist of known-bad names silently
/// passes every extension invented after this code was written, and — worse —
/// passes `unparseableExtension`, which is what the RPC emits when the node
/// itself cannot decode the extension. "The node does not know what this is"
/// must never read as "safe" in the module whose whole contract is to fail
/// closed.
///
/// pump.fun mints are Token-2022 and carry metadataPointer + tokenMetadata, so
/// these two are what keep the largest venue tradable.
const BENIGN_EXTENSIONS: &[&str] = &[
    "metadataPointer",
    "tokenMetadata",
    "groupPointer",
    "groupMemberPointer",
    "tokenGroup",
    "tokenGroupMember",
];

/// Named only so a rejection can say WHY rather than "unrecognised". Absence
/// from this list is not safety - absence from BENIGN_EXTENSIONS is the test.
const KNOWN_HOSTILE: &[(&str, &str)] = &[
    ("transferHook", "arbitrary program runs on every transfer and can revert your sell"),
    ("transferFeeConfig", "the fee can be raised to 100% after you are in"),
    ("permanentDelegate", "author can move your tokens out of your wallet at will"),
    ("defaultAccountState", "new accounts can be created frozen"),
    ("nonTransferable", "you can never sell, by construction"),
    ("mintCloseAuthority", "the mint can be closed out from under the market"),
    ("pausableConfig", "author can pause every transfer - a freeze by another name"),
    ("confidentialTransferMint", "balances are hidden, so no holder check means anything"),
    ("interestBearingConfig", "UI amounts drift from raw amounts, breaking size maths"),
    ("scaledUiAmountConfig", "UI amounts are rescaled at will, breaking size maths"),
];

pub async fn check(
    rpc: &SolanaRpc,
    cfg: &ScreenConfig,
    mint: &Pubkey,
) -> (Vec<CheckResult>, Option<MintFacts>) {
    let mut out = Vec::new();

    let resp = match rpc.get_mint_account(mint).await {
        Ok(v) => v,
        Err(e) => {
            out.push(CheckResult::unavailable(
                "mint_account",
                format!("could not read mint account: {e}"),
            ));
            return (out, None);
        }
    };

    let value = resp.get("value");
    if value.map(|v| v.is_null()).unwrap_or(true) {
        out.push(CheckResult::fail("mint_account", "mint account does not exist"));
        return (out, None);
    }
    let value = value.unwrap();

    let program = value
        .get("owner")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();

    let parsed = value.get("data").and_then(|d| d.get("parsed"));
    let info = match parsed.and_then(|p| p.get("info")) {
        Some(i) => i,
        None => {
            out.push(CheckResult::fail(
                "mint_account",
                "mint account is not parseable as an SPL mint",
            ));
            return (out, None);
        }
    };

    let program_label = value
        .get("data")
        .and_then(|d| d.get("program"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let is_token_2022 = program_label == "spl-token-2022";
    debug!(%mint, %program, is_token_2022, "mint program");

    let decimals = match info.get("decimals").and_then(|v| v.as_u64()) {
        Some(d) => d as u8,
        None => {
            out.push(CheckResult::fail("mint_account", "mint has no decimals field"));
            return (out, None);
        }
    };

    let supply_raw = info
        .get("supply")
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse::<u128>().ok())
        .unwrap_or(0);

    // --- mint authority -------------------------------------------------
    // A live mint authority means the dev can print unlimited supply and
    // dilute your position to zero at any moment.
    let mint_authority = info.get("mintAuthority");
    let mint_renounced = mint_authority.map(|v| v.is_null()).unwrap_or(false);
    if cfg.require_mint_authority_renounced {
        if mint_renounced {
            out.push(CheckResult::pass("mint_authority", "renounced"));
        } else {
            let who = mint_authority
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string();
            out.push(CheckResult::fail(
                "mint_authority",
                format!("still held by {who} - supply can be inflated"),
            ));
        }
    }

    // --- freeze authority -----------------------------------------------
    // A live freeze authority means the dev can freeze your token account and
    // you simply never sell. This is the cleanest honeypot on Solana.
    let freeze_authority = info.get("freezeAuthority");
    let freeze_renounced = freeze_authority.map(|v| v.is_null()).unwrap_or(false);
    if cfg.require_freeze_authority_renounced {
        if freeze_renounced {
            out.push(CheckResult::pass("freeze_authority", "renounced"));
        } else {
            let who = freeze_authority
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string();
            out.push(CheckResult::fail(
                "freeze_authority",
                format!("still held by {who} - your account can be frozen"),
            ));
        }
    }

    // --- Token-2022 extensions ------------------------------------------
    if cfg.reject_token2022_extensions {
        let disqualifying = disqualifying_extensions(info);
        if disqualifying.is_empty() {
            out.push(CheckResult::pass(
                "token2022_extensions",
                "none, or all on the benign allowlist",
            ));
        } else {
            out.push(CheckResult::fail(
                "token2022_extensions",
                format!("disqualifying extensions: {}", disqualifying.join("; ")),
            ));
        }
    }

    if supply_raw == 0 {
        out.push(CheckResult::fail("supply", "reported supply is zero"));
    }

    (out, Some(MintFacts { decimals, supply_raw, is_token_2022 }))
}

/// Every extension that is not explicitly benign, with a reason where we have one.
fn disqualifying_extensions(info: &Value) -> Vec<String> {
    let mut found = Vec::new();
    if let Some(exts) = info.get("extensions").and_then(|v| v.as_array()) {
        for ext in exts {
            // A nameless entry is itself unrecognisable, so it disqualifies.
            let name = ext
                .get("extension")
                .and_then(|v| v.as_str())
                .unwrap_or("<unnamed>");
            if BENIGN_EXTENSIONS.contains(&name) {
                continue;
            }
            match KNOWN_HOSTILE.iter().find(|(n, _)| *n == name) {
                Some((n, why)) => found.push(format!("{n} ({why})")),
                // A zero-rate fee today can be raised tomorrow, and an unknown
                // extension can do anything at all. Both are rejections.
                None => found.push(format!("{name} (unrecognised, not on the allowlist)")),
            }
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn info(exts: &[&str]) -> Value {
        json!({
            "extensions": exts.iter().map(|e| json!({ "extension": e })).collect::<Vec<_>>()
        })
    }

    #[test]
    fn pump_fun_metadata_extensions_are_benign() {
        // Every pump.fun mint is Token-2022 carrying exactly these two,
        // confirmed on live traffic. Rejecting them would silently disqualify
        // the busiest venue on Solana while looking like it was working.
        assert!(disqualifying_extensions(&info(&["metadataPointer", "tokenMetadata"])).is_empty());
    }

    #[test]
    fn a_mint_with_no_extensions_is_benign() {
        assert!(disqualifying_extensions(&json!({})).is_empty());
    }

    #[test]
    fn a_known_hostile_extension_is_rejected_with_its_reason() {
        let out = disqualifying_extensions(&info(&["transferHook"]));
        assert_eq!(out.len(), 1);
        assert!(out[0].contains("transferHook"));
        assert!(out[0].contains("revert your sell"), "got {out:?}");
    }

    // REGRESSION: this was a denylist, so every extension invented after the
    // code was written passed as "none hostile".
    #[test]
    fn an_unrecognised_extension_is_rejected() {
        let out = disqualifying_extensions(&info(&["somethingInventedNextYear"]));
        assert_eq!(out.len(), 1);
        assert!(out[0].contains("unrecognised"), "got {out:?}");
    }

    // REGRESSION: this marker is the RPC saying it cannot decode the extension.
    // Under a denylist, "the node does not know what this is" read as safe.
    #[test]
    fn an_undecodable_extension_is_rejected() {
        assert_eq!(disqualifying_extensions(&info(&["unparseableExtension"])).len(), 1);
    }

    #[test]
    fn a_nameless_extension_entry_is_rejected() {
        assert_eq!(
            disqualifying_extensions(&json!({ "extensions": [{ "state": {} }] })).len(),
            1
        );
    }

    #[test]
    fn benign_extensions_do_not_mask_a_hostile_one() {
        let out = disqualifying_extensions(&info(&[
            "metadataPointer",
            "permanentDelegate",
            "tokenMetadata",
        ]));
        assert_eq!(out.len(), 1);
        assert!(out[0].contains("permanentDelegate"));
    }
}
