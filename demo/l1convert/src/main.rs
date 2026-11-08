//! Converts the L1 (Minotari) console wallet's burn proof JSON into the
//! engine-side claim proof JSON that `sealed-bid-demo bootstrap` consumes.
//!
//! This is a separate crate because the L1 crates (tari_sidechain 6.x) and the
//! current L2 client stack (tari_ootle 0.49 / ootle-rs 0.31) cannot be
//! resolved in one dependency graph; the engine-side MinotariBurnClaimProof
//! wire format is identical across both stacks, so it crosses as JSON.
//!
//! Usage: l1convert <burn-proof.json> <out-claim.json>
//! The output file contains {"claim_proof": {...}, "encrypted_data": "<hex>"}.

use serde::{Deserialize, Serialize};
use tari_engine_types::confidential::MinotariBurnClaimProof;

/// The JSON file the L1 console wallet writes into burn_proofs/.
type BurnProofFile = tari_sidechain::CompleteClaimBurnProof;

// Adapted from tari-project/tari-ootle applications/tari_ootle_app_utilities
// src/burn_claim_proof.rs (BSD-3-Clause): converts the L1 proof into the
// engine-side claim proof.
fn claim_proof_from_l1(proof: &tari_sidechain::BurnClaimProof) -> Result<MinotariBurnClaimProof, String> {
    use ootle_byte_type::ToByteType;
    use tari_common_types::burn_proof::{BurnOutputProof, MmrInclusionProof};
    use tari_engine_types::confidential::{
        BurnOutput,
        BurnOutputFeatures,
        BurnOutputInclusionProof,
        BurnSidechainId,
        MmrInclusionProof as ClaimMmrInclusionProof,
    };
    use tari_transaction_components::transaction_components::{OutputFeatures, OutputType, SideChainFeatureData};
    use tari_template_lib::types::Hash32;

    fn bytes<const N: usize>(field: &str, bytes: Vec<u8>) -> Result<bounded_vec::BoundedVec<u8, 1, N>, String> {
        let len = bytes.len();
        bounded_vec::BoundedVec::<u8, 1, N>::from_vec(bytes)
            .map_err(|_| format!("burn output {field} length {len} is out of range"))
    }

    fn mmr_proof(proof: &MmrInclusionProof) -> ClaimMmrInclusionProof {
        let hashes = |hashes: &[tari_common_types::types::FixedHash]| {
            hashes
                .iter()
                .map(|hash| Hash32::from_array(hash.into_array()))
                .collect()
        };
        ClaimMmrInclusionProof {
            leaf_index: proof.leaf_index,
            mmr_size: proof.mmr_size,
            path: hashes(&proof.path),
            peaks: hashes(&proof.peaks),
        }
    }

    let BurnOutputProof {
        block_hash,
        output,
        normal_output_proof,
        normal_output_mr,
        block_output_proof,
        ..
    } = &proof.output_proof;

    let features: OutputFeatures =
        borsh::from_slice(&output.features).map_err(|e| format!("invalid burn output features: {e}"))?;
    if features.output_type != OutputType::Burn {
        return Err(format!("the output is a {} output, not a burn", features.output_type));
    }
    let sidechain_feature = features
        .sidechain_feature
        .as_ref()
        .ok_or("the burn output has no sidechain feature, so it names no claimant")?;
    let SideChainFeatureData::ConfidentialOutput(confidential_output) = &sidechain_feature.data else {
        return Err("the burn output's sidechain feature is not a confidential output".to_string());
    };

    Ok(MinotariBurnClaimProof {
        commitment: output.commitment.to_byte_type(),
        ownership_proof: proof.ownership_proof.to_byte_type(),
        value: proof.value,
        output: BurnOutput {
            version: output.version,
            features: BurnOutputFeatures {
                version: features.version.as_u8(),
                maturity: features.maturity,
                claim_public_key: confidential_output.claim_public_key.to_byte_type(),
                sidechain_id: sidechain_feature.sidechain_id().map(|id| BurnSidechainId {
                    public_key: id.public_key().to_byte_type(),
                    knowledge_proof: id.knowledge_proof().to_byte_type(),
                }),
                range_proof_type: features.range_proof_type.as_byte(),
            },
            rangeproof_hash: Hash32::from_array(output.rangeproof_hash.into_array()),
            script: bytes("script", output.script.clone())?,
            sender_offset_public_key: output.sender_offset_public_key.to_byte_type(),
            metadata_signature: bytes(
                "metadata signature",
                borsh::to_vec(&output.metadata_signature).map_err(|e| e.to_string())?,
            )?,
            covenant: bytes("covenant", output.covenant.clone())?,
            encrypted_data: bytes("encrypted data", output.encrypted_data.clone())?,
            minimum_value_promise: output.minimum_value_promise,
        },
        inclusion_proof: BurnOutputInclusionProof {
            block_hash: Hash32::from_array(block_hash.into_array()),
            normal_output_proof: mmr_proof(normal_output_proof),
            normal_output_mr: Hash32::from_array(normal_output_mr.into_array()),
            block_output_proof: mmr_proof(block_output_proof),
        },
    })
}

#[derive(Serialize, Deserialize)]
struct ConvertedClaim {
    claim_proof: MinotariBurnClaimProof,
    encrypted_data: String,
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let in_path = args.first().expect("usage: l1convert <burn-proof.json> <out-claim.json>");
    let out_path = args.get(1).expect("usage: l1convert <burn-proof.json> <out-claim.json>");
    let proof: BurnProofFile =
        serde_json::from_str(&std::fs::read_to_string(in_path).expect("read burn proof file"))
            .expect("parse burn proof file");
    let claim_proof = claim_proof_from_l1(&proof.claim_proof).expect("convert L1 burn proof");
    let converted = ConvertedClaim {
        claim_proof,
        encrypted_data: proof.encrypted_data.iter().map(|b| format!("{b:02x}")).collect(),
    };
    std::fs::write(out_path, serde_json::to_string_pretty(&converted).unwrap())
        .expect("write converted claim");
    println!("converted claim written to {out_path}");
}
