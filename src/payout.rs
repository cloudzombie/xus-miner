//! Miner-local signatures for non-custodial pool payouts.
//!
//! The pool pushes a `payout` notice naming the transfers the current share
//! window requires. This module signs those ordinary transfers with a seed
//! that never leaves the process. The pool checks the signatures and relays
//! the transactions; it does not receive the seed.
//!
//! The signed body is the canonical Borsh transaction the chain verifies:
//! signer string, Ed25519 public-key scheme byte `0x00` and key, little-endian
//! nonce, transfer action byte `0x00`, recipient string, and little-endian
//! amount. When the notice carries a signing domain, the signature is over
//! `b"sov:tx:v1" ‖ 0x00 ‖ chain_id ‖ 0x00 ‖ genesis ‖ body` instead. The
//! transaction id stays the hash of the unframed body. Seed
//! `11` repeated (32 bytes) is the published compatibility vector: its
//! implicit account is
//! `8ce53d0a623b12aac68d7a18ba9c31af1b11f56352e73959c0b484d06d10dfb7`.

use crate::diag;
use crate::wire::validate_account_id;
#[cfg(test)]
use ed25519_dalek::{Signature, VerifyingKey};
use ed25519_dalek::{Signer, SigningKey};
use serde_json::{json, Value};
use std::fs;
use std::path::Path;

const TX_DOMAIN_TAG: &[u8] = b"sov:tx:v1";
const MAX_TRANSFERS: usize = 64;
const MAX_CHAIN_ID_BYTES: usize = 128;
const MAX_SEED_FILE_BYTES: u64 = 128;
/// Placeholder login used when the operator did not choose an account.
const DEFAULT_LOGIN: &str = "xus-miner";

/// Published implicit account of the all-`0x11` seed. Pinned by the pool.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) const VECTOR_IMPLICIT_ACCOUNT: &str =
    "8ce53d0a623b12aac68d7a18ba9c31af1b11f56352e73959c0b484d06d10dfb7";

/// Ed25519 payout key. Debug output names only the public implicit account.
pub(crate) struct PayoutSigner {
    signing: SigningKey,
    public: [u8; 32],
    pub(crate) implicit_account: String,
}

impl Clone for PayoutSigner {
    fn clone(&self) -> Self {
        Self::from_seed(self.signing.to_bytes())
    }
}

impl PartialEq for PayoutSigner {
    fn eq(&self, other: &Self) -> bool {
        self.public == other.public
    }
}

impl Eq for PayoutSigner {}

impl std::fmt::Debug for PayoutSigner {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PayoutSigner")
            .field("implicit_account", &self.implicit_account)
            .finish()
    }
}

impl PayoutSigner {
    pub(crate) fn from_seed(mut seed: [u8; 32]) -> Self {
        let signing = SigningKey::from_bytes(&seed);
        wipe_bytes(&mut seed);
        let public = signing.verifying_key().to_bytes();
        let implicit_account = implicit_account_id(&public);
        Self {
            signing,
            public,
            implicit_account,
        }
    }

    pub(crate) fn from_hex(raw: &str) -> Result<Self, String> {
        Ok(Self::from_seed(parse_seed_bytes(raw.as_bytes())?))
    }

    pub(crate) fn from_file(path: &Path) -> Result<Self, String> {
        if path_embeds_seed(&path.to_string_lossy()) {
            return Err("seed file path must be a filesystem path, not the seed itself".into());
        }
        let metadata =
            fs::symlink_metadata(path).map_err(|_| "cannot read seed file".to_string())?;
        if metadata.file_type().is_symlink() {
            return Err("seed file must be a regular file, not a symbolic link".into());
        }
        if !metadata.is_file() {
            return Err("seed file must be a regular file".into());
        }
        if metadata.len() > MAX_SEED_FILE_BYTES {
            return Err("seed file is larger than 128 bytes".into());
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if metadata.permissions().mode() & 0o077 != 0 {
                return Err("seed file must be readable only by its owner (mode 0600)".into());
            }
        }
        let mut bytes = fs::read(path).map_err(|_| "cannot read seed file".to_string())?;
        let parsed = parse_seed_bytes(&bytes);
        wipe_bytes(&mut bytes);
        Ok(Self::from_seed(parsed?))
    }

    /// Remember every textual form of the seed so a later log line cannot
    /// persist it. The copies live only inside the redaction list.
    pub(crate) fn protect_logs(&self) {
        let mut seed = self.signing.to_bytes();
        let mut lower = hex::encode(seed);
        let mut upper = lower.to_ascii_uppercase();
        let mut prefixed_lower = format!("0x{lower}");
        let mut prefixed_upper = format!("0X{upper}");
        diag::protect_secret(&lower);
        diag::protect_secret(&upper);
        diag::protect_secret(&prefixed_lower);
        diag::protect_secret(&prefixed_upper);
        wipe_secret_string(&mut lower);
        wipe_secret_string(&mut upper);
        wipe_secret_string(&mut prefixed_lower);
        wipe_secret_string(&mut prefixed_upper);
        wipe_bytes(&mut seed);
    }

    fn sign_bytes(&self, message: &[u8]) -> [u8; 64] {
        self.signing.sign(message).to_bytes()
    }
}

/// What the miner should do with one `payout` notice.
pub(crate) enum PayoutAction {
    /// The notice needs no signature (already satisfied, empty window, or not
    /// addressed to this login).
    Ignore {
        reason: &'static str,
    },
    Submit(PayoutSubmission),
}

pub(crate) struct PayoutSubmission {
    pub fingerprint: String,
    pub transfers: usize,
    pub account: String,
    pub txs: Vec<Value>,
}

/// Sign the transfers in a pool `payout` notice, or explain why there is
/// nothing to send. The returned JSON is the `txs` array of a `payout_txs`
/// request. It contains the public key and signatures, never the seed.
pub(crate) fn decide_payout(
    signer: &PayoutSigner,
    login_account: &str,
    params: &Value,
) -> Result<PayoutAction, String> {
    if params
        .get("satisfied")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return Ok(PayoutAction::Ignore {
            reason: "satisfied",
        });
    }
    let Some(account) = params.get("account").and_then(Value::as_str) else {
        return Err("payout notice is missing string `account`".into());
    };
    if account != login_account {
        return Ok(PayoutAction::Ignore { reason: "account" });
    }
    let Some(transfers) = params.get("transfers").and_then(Value::as_array) else {
        return Err("payout notice is missing array `transfers`".into());
    };
    if transfers.is_empty() {
        return Ok(PayoutAction::Ignore { reason: "empty" });
    }
    if transfers.len() > MAX_TRANSFERS {
        return Err(format!(
            "payout notice has {} transfers; the maximum is {MAX_TRANSFERS}",
            transfers.len()
        ));
    }
    let nonce = params
        .get("nextNonce")
        .and_then(Value::as_u64)
        .ok_or_else(|| "payout notice is missing integer `nextNonce`".to_string())?;
    let domain = parse_domain(params.get("signingDomain"))?;
    let mut txs = Vec::with_capacity(transfers.len());
    for (index, leg) in transfers.iter().enumerate() {
        let to = leg
            .get("to")
            .and_then(Value::as_str)
            .ok_or_else(|| "payout transfer is missing string `to`".to_string())?;
        validate_account_id(to).map_err(|error| format!("payout recipient: {error}"))?;
        let amount_text = leg
            .get("amount")
            .and_then(Value::as_str)
            .ok_or_else(|| "payout transfer is missing string `amount`".to_string())?;
        let amount: u128 = amount_text
            .parse()
            .map_err(|_| "payout amount is not a grain count".to_string())?;
        if amount == 0 {
            return Err("payout amount must be greater than zero".into());
        }
        let nonce = nonce
            .checked_add(u64::try_from(index).unwrap_or(u64::MAX))
            .ok_or_else(|| "payout nonce overflowed".to_string())?;
        txs.push(signed_transfer(
            signer,
            login_account,
            nonce,
            to,
            amount,
            domain.as_ref(),
        ));
    }
    Ok(PayoutAction::Submit(PayoutSubmission {
        fingerprint: params.to_string(),
        transfers: txs.len(),
        account: login_account.to_owned(),
        txs,
    }))
}

/// Replace the default login with the seed's implicit account, and refuse an
/// implicit login that belongs to a different key. A checksummed `xust1`
/// address is reduced to the same 64-hex account the pool will echo.
pub(crate) fn apply_payout_login(user: &str, signer: &PayoutSigner) -> Result<String, String> {
    let (account, worker) = split_login(user.trim());
    let canonical = if account == DEFAULT_LOGIN && worker.is_none() {
        signer.implicit_account.clone()
    } else {
        canonical_account(account)?
    };
    if is_implicit_account(&canonical) && canonical != signer.implicit_account {
        return Err(
            "the --user account is an implicit id that does not match the payout seed".into(),
        );
    }
    if !is_implicit_account(&canonical) {
        validate_account_id(&canonical).map_err(|error| format!("payout login: {error}"))?;
    }
    Ok(match worker {
        Some(worker) if !worker.is_empty() => format!("{canonical}/{worker}"),
        _ => canonical,
    })
}

pub(crate) fn login_account(user: &str) -> String {
    let (account, _) = split_login(user.trim());
    canonical_account(account).unwrap_or_else(|_| account.to_owned())
}

/// True when a settings path or flag value is the seed rather than a path.
pub(crate) fn path_embeds_seed(path: &str) -> bool {
    path.split(['/', '\\']).any(|component| {
        let component = component.trim();
        let stem = component
            .rsplit_once('.')
            .filter(|(_, extension)| {
                !extension.is_empty()
                    && extension.len() <= 5
                    && extension.bytes().all(|byte| byte.is_ascii_alphanumeric())
            })
            .map(|(stem, _)| stem)
            .unwrap_or(component);
        is_seed_text(stem)
    })
}

fn signed_transfer(
    signer: &PayoutSigner,
    login_account: &str,
    nonce: u64,
    to: &str,
    amount: u128,
    domain: Option<&SigningDomain>,
) -> Value {
    let body = transfer_signing_bytes(login_account, &signer.public, nonce, to, amount);
    let preimage = match domain {
        Some(domain) => frame_domain(&domain.chain_id, &domain.genesis, &body),
        None => body,
    };
    let signature = signer.sign_bytes(&preimage);
    json!({
        "transaction": {
            "signer": login_account,
            "public_key": format!("0x{}", hex::encode(signer.public)),
            "nonce": nonce,
            "action": {
                "type": "transfer",
                "to": to,
                "amount": amount.to_string(),
            }
        },
        "signature": format!("0x{}", hex::encode(signature)),
    })
}

struct SigningDomain {
    chain_id: String,
    genesis: [u8; 32],
}

fn parse_domain(value: Option<&Value>) -> Result<Option<SigningDomain>, String> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(value) => {
            let chain_id = value
                .get("chainId")
                .and_then(Value::as_str)
                .ok_or_else(|| "signingDomain.chainId is missing".to_string())?;
            if chain_id.is_empty()
                || chain_id.len() > MAX_CHAIN_ID_BYTES
                || chain_id.bytes().any(|byte| byte < 0x20 || byte == 0x7f)
            {
                return Err("signingDomain.chainId is not a usable network name".into());
            }
            let genesis_text = value
                .get("genesis")
                .and_then(Value::as_str)
                .ok_or_else(|| "signingDomain.genesis is missing".to_string())?;
            let genesis = parse_genesis(genesis_text)?;
            Ok(Some(SigningDomain {
                chain_id: chain_id.to_owned(),
                genesis,
            }))
        }
    }
}

fn parse_genesis(text: &str) -> Result<[u8; 32], String> {
    let text = text.trim().strip_prefix("0x").unwrap_or(text.trim());
    let mut genesis = [0u8; 32];
    if text.len() != 64 {
        return Err("signingDomain.genesis must be 32 bytes".into());
    }
    hex::decode_to_slice(text, &mut genesis)
        .map_err(|_| "signingDomain.genesis must be 32 bytes".to_string())?;
    Ok(genesis)
}

fn transfer_signing_bytes(
    signer: &str,
    public: &[u8; 32],
    nonce: u64,
    to: &str,
    amount: u128,
) -> Vec<u8> {
    let mut out = Vec::new();
    put_str(&mut out, signer);
    out.push(0);
    out.extend_from_slice(public);
    out.extend_from_slice(&nonce.to_le_bytes());
    out.push(0);
    put_str(&mut out, to);
    out.extend_from_slice(&amount.to_le_bytes());
    out
}

fn frame_domain(chain_id: &str, genesis: &[u8; 32], body: &[u8]) -> Vec<u8> {
    let mut framed = Vec::with_capacity(TX_DOMAIN_TAG.len() + 2 + chain_id.len() + 32 + body.len());
    framed.extend_from_slice(TX_DOMAIN_TAG);
    framed.push(0);
    framed.extend_from_slice(chain_id.as_bytes());
    framed.push(0);
    framed.extend_from_slice(genesis);
    framed.extend_from_slice(body);
    framed
}

fn put_str(out: &mut Vec<u8>, value: &str) {
    let bytes = value.as_bytes();
    let len = u32::try_from(bytes.len()).unwrap_or(u32::MAX);
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(bytes);
}

fn implicit_account_id(public: &[u8; 32]) -> String {
    let mut preimage = Vec::with_capacity(33);
    preimage.push(0);
    preimage.extend_from_slice(public);
    hex::encode(blake3::hash(&preimage).as_bytes())
}

fn split_login(user: &str) -> (&str, Option<&str>) {
    match user.split_once('/') {
        Some((account, worker)) => (account, Some(worker)),
        None => (user, None),
    }
}

fn canonical_account(account: &str) -> Result<String, String> {
    if account.len() >= 5 && account[..5].eq_ignore_ascii_case("xust1") {
        return decode_xust(account);
    }
    if account.len() == 64 && account.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Ok(account.to_ascii_lowercase());
    }
    Ok(account.to_owned())
}

fn decode_xust(raw: &str) -> Result<String, String> {
    let checked = bech32::primitives::decode::CheckedHrpstring::new::<bech32::Bech32m>(raw)
        .map_err(|_| "invalid xust1 payout address".to_string())?;
    if checked.hrp().to_lowercase() != "xust" {
        return Err("payout address prefix is not xust".into());
    }
    let payload: Vec<u8> = checked.byte_iter().collect();
    if payload.len() != 32 {
        return Err("invalid xust1 payout address".into());
    }
    Ok(hex::encode(payload))
}

fn is_implicit_account(account: &str) -> bool {
    account.len() == 64
        && account
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn is_seed_text(value: &str) -> bool {
    let value = value
        .trim()
        .strip_prefix("0x")
        .or_else(|| value.trim().strip_prefix("0X"))
        .unwrap_or(value.trim());
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn parse_seed_bytes(raw: &[u8]) -> Result<[u8; 32], String> {
    let mut start = 0;
    let mut end = raw.len();
    while start < end && raw[start].is_ascii_whitespace() {
        start += 1;
    }
    while end > start && raw[end - 1].is_ascii_whitespace() {
        end -= 1;
    }
    let mut slice = &raw[start..end];
    if let Some(rest) = slice
        .strip_prefix(b"0x")
        .or_else(|| slice.strip_prefix(b"0X"))
    {
        slice = rest;
    }
    if slice.len() != 64 || !slice.iter().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("seed must be 32 bytes (64 hex characters)".into());
    }
    let mut seed = [0u8; 32];
    hex::decode_to_slice(slice, &mut seed)
        .map_err(|_| "seed must be 32 bytes (64 hex characters)".to_string())?;
    Ok(seed)
}

fn wipe_bytes(bytes: &mut [u8]) {
    for byte in bytes {
        *byte = 0;
        std::hint::black_box(*byte);
    }
}

pub(crate) fn wipe_secret_string(value: &mut String) {
    let mut bytes = std::mem::take(value).into_bytes();
    wipe_bytes(&mut bytes);
}

/// Confirm a signature the miner produced verifies under the same strict
/// Ed25519 check the pool uses. Test-only so the production path does not
/// need the verifier import to stay live.
#[cfg(test)]
fn verify_strict(public: &[u8; 32], message: &[u8], signature: &[u8; 64]) -> bool {
    let Ok(key) = VerifyingKey::from_bytes(public) else {
        return false;
    };
    let Ok(signature) = Signature::from_slice(signature) else {
        return false;
    };
    key.verify_strict(message, &signature).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn vector_signer() -> PayoutSigner {
        PayoutSigner::from_seed([0x11; 32])
    }

    fn hand_built_preimage(signer: &PayoutSigner) -> Vec<u8> {
        let mut expected = Vec::new();
        let put = |out: &mut Vec<u8>, value: &str| {
            let bytes = value.as_bytes();
            out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
            out.extend_from_slice(bytes);
        };
        put(&mut expected, &signer.implicit_account);
        expected.push(0);
        expected.extend_from_slice(&signer.public);
        expected.extend_from_slice(&7u64.to_le_bytes());
        expected.push(0);
        put(&mut expected, "bob.sov");
        expected.extend_from_slice(&42u128.to_le_bytes());
        expected
    }

    #[test]
    fn all_ones_seed_matches_the_published_implicit_account() {
        let signer = vector_signer();
        assert_eq!(signer.implicit_account, VECTOR_IMPLICIT_ACCOUNT);
        let rendered = format!("{signer:?}");
        assert!(rendered.contains(VECTOR_IMPLICIT_ACCOUNT));
        assert!(!rendered.contains(&"11".repeat(64)));
        assert!(!rendered.contains("17, 17, 17"));
    }

    #[test]
    fn transfer_preimage_matches_the_pool_vector_and_verifies() {
        let signer = vector_signer();
        let body =
            transfer_signing_bytes(&signer.implicit_account, &signer.public, 7, "bob.sov", 42);
        assert_eq!(body, hand_built_preimage(&signer));
        let signature = signer.sign_bytes(&body);
        assert!(verify_strict(&signer.public, &body, &signature));
    }

    #[test]
    fn domain_signature_verifies_only_on_the_framed_preimage() {
        let signer = vector_signer();
        let body =
            transfer_signing_bytes(&signer.implicit_account, &signer.public, 7, "bob.sov", 42);
        let genesis = [0xab; 32];
        let framed = frame_domain("sov-devnet", &genesis, &body);
        assert!(framed.starts_with(b"sov:tx:v1\0sov-devnet\0"));
        assert!(framed.ends_with(&body));
        let signature = signer.sign_bytes(&framed);
        assert!(verify_strict(&signer.public, &framed, &signature));
        assert!(!verify_strict(&signer.public, &body, &signature));
    }

    #[test]
    fn payout_notice_signs_the_window_without_embedding_the_seed() {
        let signer = vector_signer();
        let params = json!({
            "account": signer.implicit_account,
            "satisfied": false,
            "transfers": [
                {"to": "carol.sov", "amount": "550119564"},
                {"to": "alice.sov", "amount": "370272783"}
            ],
            "nextNonce": 7,
            "signingDomain": {"chainId": "sov-devnet", "genesis": "ab".repeat(32)}
        });
        let PayoutAction::Submit(submission) =
            decide_payout(&signer, &signer.implicit_account, &params).unwrap()
        else {
            panic!("expected a signed submission");
        };
        assert_eq!(submission.transfers, 2);
        assert_eq!(submission.txs[0]["transaction"]["nonce"], 7);
        assert_eq!(submission.txs[1]["transaction"]["nonce"], 8);
        assert_eq!(
            submission.txs[0]["transaction"]["action"]["type"],
            "transfer"
        );
        assert_eq!(
            submission.txs[0]["transaction"]["action"]["to"],
            "carol.sov"
        );
        assert_eq!(
            submission.txs[0]["transaction"]["action"]["amount"],
            "550119564"
        );
        assert_eq!(
            submission.txs[0]["transaction"]["signer"],
            signer.implicit_account
        );
        let rendered = serde_json::to_string(&submission.txs).unwrap();
        assert!(!rendered.contains(&"11".repeat(64)));
        assert!(!rendered.contains(&"11".repeat(32)));

        let signature = hex::decode(
            submission.txs[0]["signature"]
                .as_str()
                .unwrap()
                .trim_start_matches("0x"),
        )
        .unwrap();
        let body = transfer_signing_bytes(
            &signer.implicit_account,
            &signer.public,
            7,
            "carol.sov",
            550_119_564,
        );
        let framed = frame_domain("sov-devnet", &[0xab; 32], &body);
        assert!(verify_strict(
            &signer.public,
            &framed,
            &signature.try_into().unwrap()
        ));
    }

    #[test]
    fn satisfied_empty_and_foreign_notices_are_not_signed() {
        let signer = vector_signer();
        let base = json!({
            "account": signer.implicit_account,
            "transfers": [{"to": "bob.sov", "amount": "1"}],
            "nextNonce": 1
        });
        let mut satisfied = base.clone();
        satisfied["satisfied"] = json!(true);
        assert!(matches!(
            decide_payout(&signer, &signer.implicit_account, &satisfied).unwrap(),
            PayoutAction::Ignore {
                reason: "satisfied"
            }
        ));
        let empty = json!({
            "account": signer.implicit_account,
            "satisfied": false,
            "transfers": [],
            "nextNonce": 1
        });
        assert!(matches!(
            decide_payout(&signer, &signer.implicit_account, &empty).unwrap(),
            PayoutAction::Ignore { reason: "empty" }
        ));
        assert!(matches!(
            decide_payout(&signer, "other.sov", &base).unwrap(),
            PayoutAction::Ignore { reason: "account" }
        ));
    }

    #[test]
    fn malformed_seed_errors_do_not_echo_the_input() {
        let secret = "ab".repeat(40);
        let error = PayoutSigner::from_hex(&secret).unwrap_err();
        assert_eq!(error, "seed must be 32 bytes (64 hex characters)");
        assert!(!error.contains(&secret));
        assert!(!error.contains("abab"));
    }

    #[test]
    fn xust1_login_reduces_to_the_same_implicit_account() {
        let signer = vector_signer();
        let payload = hex::decode(&signer.implicit_account).unwrap();
        let hrp = bech32::Hrp::parse("xust").unwrap();
        let address = bech32::encode::<bech32::Bech32m>(hrp, &payload).unwrap();
        assert!(address.to_ascii_lowercase().starts_with("xust1"));
        assert_eq!(
            apply_payout_login(&format!("{address}/rig-1"), &signer).unwrap(),
            format!("{}/rig-1", signer.implicit_account)
        );
    }

    #[test]
    fn implicit_login_must_match_the_seed() {
        let signer = vector_signer();
        assert_eq!(
            apply_payout_login("xus-miner", &signer).unwrap(),
            signer.implicit_account
        );
        assert_eq!(
            apply_payout_login(&format!("{}/rig-1", signer.implicit_account), &signer).unwrap(),
            format!("{}/rig-1", signer.implicit_account)
        );
        let foreign = "22".repeat(32);
        let error = apply_payout_login(&foreign, &signer).unwrap_err();
        assert!(error.contains("does not match"));
        assert!(!error.contains(&foreign));
        assert!(!error.contains(&"11".repeat(64)));
    }

    #[test]
    fn seed_file_rejects_loose_permissions_and_round_trips() {
        let directory = std::env::temp_dir().join(format!(
            "xus-miner-payout-{}-{}",
            std::process::id(),
            "unit"
        ));
        let _ = fs::remove_dir_all(&directory);
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("seed");
        {
            let mut file = fs::File::create(&path).unwrap();
            writeln!(file, "{}", "11".repeat(32)).unwrap();
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
            let error = PayoutSigner::from_file(&path).unwrap_err();
            assert!(error.contains("0600"), "{error}");
            assert!(!error.contains(&"11".repeat(64)));
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        }
        let signer = PayoutSigner::from_file(&path).unwrap();
        assert_eq!(signer.implicit_account, VECTOR_IMPLICIT_ACCOUNT);
        let _ = fs::remove_dir_all(&directory);
    }

    #[test]
    fn a_hex_string_is_not_accepted_as_a_seed_file_path() {
        assert!(path_embeds_seed(&"11".repeat(32)));
        assert!(path_embeds_seed(&format!("/tmp/{}.txt", "aa".repeat(32))));
        assert!(!path_embeds_seed("/home/miner/.xus-miner/payout.seed"));
        let error = PayoutSigner::from_file(Path::new(&"11".repeat(32))).unwrap_err();
        assert!(!error.contains(&"11".repeat(64)));
    }
}
