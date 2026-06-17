//! Bitcoin network classification from an address string.
//!
//! Used by the agent-payment-connect gate (`plans/agent-payment-connect-scope.md`
//! §6.1): a *scoped* key may connect a BTCPay store only when its target network
//! is non-mainnet. Greenfield's `GET /api/v1/server/info` carries no chain-type
//! field, so we determine the network from a **network-encoding artifact** — the
//! store's on-chain receive address — and classify by its prefix.
//!
//! Validated against a live regtest BTCPay 2.x: `wallet/address` returns a
//! `bcrt1…` address on regtest (see `onboarding-harness/stage2/btcpay-regtest/`).
//!
//! **Fail-closed:** an unrecognized / empty address yields `None`; the caller
//! MUST treat `None` as mainnet (deny the scoped connect). Never assume
//! non-mainnet from absence of evidence.

/// The Bitcoin network a BTCPay store settles on. Only the mainnet-vs-rest
/// distinction gates the scoped connect, but the specific non-mainnet variant
/// is kept for audit/logging.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BitcoinNetwork {
    Mainnet,
    /// testnet3 — shares the `tb1` HRP and `m`/`n`/`2` base58 versions with signet.
    Testnet,
    /// Signet — indistinguishable from testnet by address alone (`tb1`), so the
    /// address classifier never yields this; reserved for a future
    /// derivation-scheme-based path. Kept distinct because it is a real,
    /// non-mainnet network the gate must allow.
    Signet,
    Regtest,
}

impl BitcoinNetwork {
    pub fn as_str(self) -> &'static str {
        match self {
            BitcoinNetwork::Mainnet => "mainnet",
            BitcoinNetwork::Testnet => "testnet",
            BitcoinNetwork::Signet => "signet",
            BitcoinNetwork::Regtest => "regtest",
        }
    }

    /// The only question the connect gate actually asks.
    pub fn is_mainnet(self) -> bool {
        matches!(self, BitcoinNetwork::Mainnet)
    }
}

/// Classify a Bitcoin address by its network-encoding prefix. Returns `None`
/// when the prefix is unrecognized or the string is empty — the caller
/// **fails closed** (treats `None` as mainnet).
///
/// bech32/bech32m HRP: `bcrt1…`=regtest, `tb1…`=testnet/signet, `bc1…`=mainnet.
/// Legacy base58: `1`/`3`=mainnet, `m`/`n`/`2`=test/regtest (the `tb1`/base58
/// test versions are shared by testnet, signet, and regtest — all non-mainnet,
/// which is all the gate needs; only the bech32 `bcrt1` HRP pins regtest
/// specifically).
pub fn classify_address_network(addr: &str) -> Option<BitcoinNetwork> {
    let s = addr.trim();
    if s.is_empty() {
        return None;
    }
    // bech32/bech32m — HRP is case-insensitive. Check `bcrt1` before `bc1`
    // (it is not a prefix of the others, but order makes the intent explicit).
    let lower = s.to_ascii_lowercase();
    if lower.starts_with("bcrt1") {
        return Some(BitcoinNetwork::Regtest);
    }
    if lower.starts_with("tb1") {
        // testnet and signet share the `tb` HRP and are indistinguishable from
        // the address alone. Both non-mainnet; report Testnet.
        return Some(BitcoinNetwork::Testnet);
    }
    if lower.starts_with("bc1") {
        return Some(BitcoinNetwork::Mainnet);
    }
    // Legacy base58check — version byte encoded in the leading character.
    // Only classify when the whole string is a *plausible* base58 address
    // (correct alphabet + length): otherwise arbitrary text that merely begins
    // with `n`/`m`/`2` (e.g. "not-an-address") would be mis-read as non-mainnet
    // and the gate would fail OPEN. Junk falls through to `None` (fail closed).
    // Case-sensitive, so classify off the original string.
    if (26..=35).contains(&s.len()) && s.chars().all(is_base58) {
        return match s.chars().next() {
            Some('1') | Some('3') => Some(BitcoinNetwork::Mainnet),
            Some('m') | Some('n') | Some('2') => Some(BitcoinNetwork::Testnet),
            _ => None,
        };
    }
    None
}

/// Base58 alphabet membership (Bitcoin's: omits `0`, `O`, `I`, `l`).
fn is_base58(c: char) -> bool {
    matches!(c, '1'..='9' | 'A'..='H' | 'J'..='N' | 'P'..='Z' | 'a'..='k' | 'm'..='z')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bech32_prefixes() {
        // The exact address the live regtest BTCPay 2.x returned.
        assert_eq!(
            classify_address_network("bcrt1qwsh9ua5qeutshvrhz474uduwqlw8gfukfpc8vt"),
            Some(BitcoinNetwork::Regtest)
        );
        assert_eq!(
            classify_address_network("tb1qw508d6qejxtdg4y5r3zarvary0c5xw7kxpjzsx"),
            Some(BitcoinNetwork::Testnet)
        );
        assert_eq!(
            classify_address_network("bc1qw508d6qejxtdg4y5r3zarvary0c5xw7kv8f3t4"),
            Some(BitcoinNetwork::Mainnet)
        );
    }

    #[test]
    fn bech32_is_case_insensitive() {
        assert_eq!(
            classify_address_network("BCRT1QWSH9UA5QEUTSHVRHZ474UDUWQLW8GFUKFPC8VT"),
            Some(BitcoinNetwork::Regtest)
        );
        assert_eq!(
            classify_address_network("BC1QW508D6QEJXTDG4Y5R3ZARVARY0C5XW7KV8F3T4"),
            Some(BitcoinNetwork::Mainnet)
        );
    }

    #[test]
    fn legacy_base58() {
        assert_eq!(classify_address_network("1BvBMSEYstWetqTFn5Au4m4GFg7xJaNVN2"), Some(BitcoinNetwork::Mainnet)); // P2PKH
        assert_eq!(classify_address_network("3J98t1WpEZ73CNmQviecrnyiWrnqRhWNLy"), Some(BitcoinNetwork::Mainnet)); // P2SH
        assert_eq!(classify_address_network("mipcBbFg9gMiCh81Kj8tqqdgoZub1ZJRfn"), Some(BitcoinNetwork::Testnet)); // testnet P2PKH
        assert_eq!(classify_address_network("n2ZNV88uQbede7C5M5jzi6SyG4GVuPpng6"), Some(BitcoinNetwork::Testnet));
        assert_eq!(classify_address_network("2MzQwSSnBHWHqSAqtTVQ6v47XtaisrJa1Vc"), Some(BitcoinNetwork::Testnet)); // test P2SH
    }

    #[test]
    fn fail_closed_on_unknown_or_empty() {
        assert_eq!(classify_address_network(""), None);
        assert_eq!(classify_address_network("   "), None);
        assert_eq!(classify_address_network("not-an-address"), None);
        assert_eq!(classify_address_network("ltc1qxyz"), None); // not bitcoin
        assert_eq!(classify_address_network("zzz"), None);
        // The dangerous direction: a base58-length, all-base58 string that does
        // NOT begin with a version char (1/3/m/n/2) must stay None, never be
        // mis-read as non-mainnet. (And a real mainnet address always begins
        // with 1/3/bc1, so it can never fall into the non-mainnet arms.)
        assert_eq!(classify_address_network("bQ8vZ2mN4pR7sT1uW3xY5zA6dE9fG"), None); // 29 chars, starts 'b'
    }

    #[test]
    fn is_mainnet_only_true_for_mainnet() {
        assert!(BitcoinNetwork::Mainnet.is_mainnet());
        assert!(!BitcoinNetwork::Testnet.is_mainnet());
        assert!(!BitcoinNetwork::Signet.is_mainnet());
        assert!(!BitcoinNetwork::Regtest.is_mainnet());
    }
}
