use std::collections::{BTreeMap, HashSet};

use anyhow::Result;
use substreams::{
    hex, log,
    store::{StoreGet, StoreGetRaw},
};
use substreams_ethereum::pb::eth::v2 as eth;
use tycho_substreams::prelude::*;

use crate::{
    keys::contract_id,
    sy::{
        class_attributes, classify_sy, is_class_attribute, Classification, CREATION_PAUSED, PAUSED,
        TOKEN_IN_CLASS_PREFIX, TOKEN_OUT_CLASS_PREFIX,
    },
    transfers::{is_market, new_sy_components},
};

/// `Paused(address)`, which OpenZeppelin's `Pausable` emits and every pausable SY inherits.
const PAUSED_TOPIC: [u8; 32] =
    hex!("62e78cea01bee320cd4e420270b5ea74000d11b0c9f74754ebdbfc544b05a258");
/// `Unpaused(address)`.
const UNPAUSED_TOPIC: [u8; 32] =
    hex!("5db9ee0a495bf2e6ff9c91a7834c1ba4fdd244a5e8aa4e537bd38aeae4b073aa");
/// EIP-1967 `Upgraded(address)`, emitted by an SY's proxy when its implementation changes.
const UPGRADED_TOPIC: [u8; 32] =
    hex!("bc7cd75a20ee27fd9adebab32041f755214dbc6bffa90cc0225b39da2e5c2d3b");

/// The SY state that is not read per block: each token's class, and whether the SY is paused.
///
/// Both come from creation and from events, so they cost nothing per block:
///
/// - At creation, the classes the profile found and `paused()` as read then.
/// - `Paused` and `Unpaused` set `paused`, which refuses every quote against the SY while set.
/// - `Upgraded` and `Unpaused` re-run the classification at the end of the block. Every listed
///   token that answers its probe gets its class updated, or deleted if it no longer matches either
///   closed form. A token whose probe does not answer keeps the class it had: upgrades usually
///   happen while an SY is paused, and a paused SY reverts its previews. Re-running on `Unpaused`
///   is what classifies such an upgrade once the SY answers again. Only the tokens the component
///   lists can be classified: its token list is fixed at creation.
///
/// A new SY's own events in its creation block are skipped: its creation state is read at the end
/// of that block and already includes them.
#[substreams::handlers::map]
pub fn map_sy_state(
    block: eth::Block,
    new_components: BlockTransactionProtocolComponents,
    components_store: StoreGetRaw,
) -> Result<BlockChanges> {
    let mut changes: BTreeMap<u64, TransactionChanges> = BTreeMap::new();
    let mut record = |tx: &Transaction, change: EntityChanges| {
        changes
            .entry(tx.index)
            .or_insert_with(|| TransactionChanges { tx: Some(tx.clone()), ..Default::default() })
            .entity_changes
            .push(change);
    };

    let created = new_sy_components(&new_components);
    let seeded: HashSet<&str> = created
        .iter()
        .map(|(_, component)| component.id.as_str())
        .collect();
    for (tx, component) in &created {
        record(tx, creation_state(component));
    }

    // The last upgrade or unpause per SY in this block, keyed by SY so a re-profile runs once.
    let mut reprofile: BTreeMap<String, Transaction> = BTreeMap::new();
    for tx in block.transactions() {
        for log in tx.logs_with_calls().map(|(log, _)| log) {
            let Some(topic) = log.topics.first() else { continue };
            let sy_id = contract_id(&log.address);
            if seeded.contains(sy_id.as_str()) ||
                !components_store.has_last(&sy_id) ||
                is_market(&components_store, &sy_id)
            {
                continue;
            }
            let paused = if topic.as_slice() == PAUSED_TOPIC {
                true
            } else if topic.as_slice() == UNPAUSED_TOPIC {
                reprofile.insert(sy_id.clone(), tx.into());
                false
            } else {
                if topic.as_slice() == UPGRADED_TOPIC {
                    reprofile.insert(sy_id, tx.into());
                }
                continue;
            };
            record(
                &tx.into(),
                EntityChanges {
                    component_id: sy_id,
                    attributes: vec![update(PAUSED, vec![u8::from(paused)])],
                },
            );
        }
    }

    for (sy_id, tx) in reprofile {
        let Some(tokens) = components_store.get_last(&sy_id) else { continue };
        let tokens: Vec<Vec<u8>> = serde_sibor::from_bytes(&tokens)
            .expect("deserializing component tokens from the component store");
        let address =
            hex::decode(sy_id.trim_start_matches("0x")).expect("SY component id is not hex");
        let Some(classification) = classify_sy(&address) else {
            // Deleting every class would retire the SY for good on what may be a transient
            // failure, so the classes it had stay until the next upgrade re-runs this.
            log::info!("SY {} did not classify on re-profile, keeping its classes", sy_id);
            continue;
        };
        record(
            &tx,
            EntityChanges {
                component_id: sy_id,
                attributes: reclassified(&tokens, &classification),
            },
        );
    }

    Ok(BlockChanges { changes: changes.into_values().collect(), ..Default::default() })
}

/// A new SY's classes and pause flag, taken from what its profile read at creation.
fn creation_state(component: &ProtocolComponent) -> EntityChanges {
    let mut attributes = Vec::new();
    for attribute in &component.static_att {
        if is_class_attribute(&attribute.name) {
            attributes.push(update(&attribute.name, attribute.value.clone()));
        }
    }
    let paused = component
        .get_attribute_value(CREATION_PAUSED)
        .unwrap_or_else(|| panic!("SY {} was created without its pause seed", component.id));
    attributes.push(update(PAUSED, paused));
    EntityChanges { component_id: component.id.clone(), attributes }
}

/// Every class attribute of the listed tokens after a re-classification: an update for each token
/// that classified, a deletion for each listed token and direction that answered its probe and no
/// longer matches, and nothing for one that did not answer, which keeps its class.
fn reclassified(listed: &[Vec<u8>], classification: &Classification) -> Vec<Attribute> {
    let Classification { tokens_in, tokens_out, undetermined_in, undetermined_out, .. } =
        classification;
    let keep = |classes: &[(Vec<u8>, crate::sy::TokenClass)]| {
        classes
            .iter()
            .filter(|(token, _)| listed.contains(token))
            .cloned()
            .collect::<Vec<_>>()
    };
    let classified = class_attributes(&keep(tokens_in), &keep(tokens_out));

    let mut attributes = Vec::new();
    for token in listed {
        for (prefix, undetermined) in
            [(TOKEN_IN_CLASS_PREFIX, undetermined_in), (TOKEN_OUT_CLASS_PREFIX, undetermined_out)]
        {
            let name = format!("{prefix}{}", hex::encode(token));
            match classified
                .iter()
                .find(|(classified_name, _)| *classified_name == name)
            {
                Some((_, value)) => attributes.push(update(&name, value.clone())),
                None if undetermined.contains(token) => {}
                None => attributes.push(Attribute {
                    name,
                    value: vec![],
                    change: ChangeType::Deletion.into(),
                }),
            }
        }
    }
    attributes
}

fn update(name: &str, value: Vec<u8>) -> Attribute {
    Attribute { name: name.to_string(), value, change: ChangeType::Update.into() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sy::TokenClass;

    const SY: [u8; 20] = [0xcb; 20];
    const WSTETH: [u8; 20] = [0x7f; 20];
    const STETH: [u8; 20] = [0xae; 20];

    fn classification(
        tokens_in: &[(Vec<u8>, TokenClass)],
        tokens_out: &[(Vec<u8>, TokenClass)],
        undetermined: &[Vec<u8>],
    ) -> Classification {
        Classification {
            decimals: 18,
            asset: STETH.to_vec(),
            asset_decimals: 18,
            tokens_in: tokens_in.to_vec(),
            tokens_out: tokens_out.to_vec(),
            undetermined_in: undetermined.to_vec(),
            undetermined_out: undetermined.to_vec(),
        }
    }

    fn by_name(attributes: &[Attribute]) -> Vec<(String, bool)> {
        let mut out: Vec<(String, bool)> = attributes
            .iter()
            .map(|a| (a.name.clone(), a.change == i32::from(ChangeType::Deletion)))
            .collect();
        out.sort();
        out
    }

    /// A token that stops matching either closed form after an upgrade loses its class rather than
    /// keeping the one it had, and a token that never had one is deleted harmlessly.
    #[test]
    fn a_reclassification_deletes_what_no_longer_classifies() {
        let listed = vec![SY.to_vec(), WSTETH.to_vec(), STETH.to_vec()];
        let attributes = reclassified(
            &listed,
            &classification(
                &[(WSTETH.to_vec(), TokenClass::OneToOne)],
                &[(WSTETH.to_vec(), TokenClass::OneToOne)],
                &[],
            ),
        );
        let wsteth = hex::encode(WSTETH);
        let steth = hex::encode(STETH);
        let sy = hex::encode(SY);
        assert_eq!(
            by_name(&attributes),
            vec![
                (format!("token_in_class_0x{wsteth}"), false),
                (format!("token_in_class_0x{steth}"), true),
                (format!("token_in_class_0x{sy}"), true),
                (format!("token_out_class_0x{wsteth}"), false),
                (format!("token_out_class_0x{steth}"), true),
                (format!("token_out_class_0x{sy}"), true),
            ]
        );
    }

    /// A token an upgrade adds is not on the component's fixed token list, so it gets no class.
    #[test]
    fn a_reclassification_ignores_tokens_the_component_does_not_list() {
        let attributes = reclassified(
            &[WSTETH.to_vec()],
            &classification(
                &[(STETH.to_vec(), TokenClass::IndexRate), (WSTETH.to_vec(), TokenClass::OneToOne)],
                &[],
                &[],
            ),
        );
        assert!(attributes
            .iter()
            .all(|a| !a.name.contains(&hex::encode(STETH))));
    }

    /// An upgrade made while the SY is paused re-profiles against previews that revert. Those
    /// tokens keep the classes they had instead of losing them for good; the re-profile on
    /// `Unpaused` classifies them once they answer.
    #[test]
    fn a_token_whose_probe_did_not_answer_keeps_its_class() {
        let listed = vec![SY.to_vec(), WSTETH.to_vec(), STETH.to_vec()];
        let attributes = reclassified(
            &listed,
            &classification(
                &[(WSTETH.to_vec(), TokenClass::OneToOne)],
                &[],
                &[STETH.to_vec(), WSTETH.to_vec()],
            ),
        );
        let wsteth = hex::encode(WSTETH);
        let sy = hex::encode(SY);
        assert_eq!(
            by_name(&attributes),
            vec![
                (format!("token_in_class_0x{wsteth}"), false),
                (format!("token_in_class_0x{sy}"), true),
                (format!("token_out_class_0x{sy}"), true),
            ]
        );
    }

    #[test]
    fn creation_state_carries_the_classes_and_the_pause_flag() {
        let component = ProtocolComponent::new("0xcbcbcbcbcbcbcbcbcbcbcbcbcbcbcbcbcbcbcbcb")
            .with_attributes(&[
                ("sy_decimals", vec![18u8]),
                (
                    "token_in_class_0x7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f",
                    b"one_to_one".to_vec(),
                ),
                (CREATION_PAUSED, vec![1u8]),
            ]);
        let state = creation_state(&component);
        assert_eq!(
            by_name(&state.attributes),
            vec![
                ("paused".to_string(), false),
                ("token_in_class_0x7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f".to_string(), false),
            ]
        );
        let paused = state
            .attributes
            .iter()
            .find(|a| a.name == PAUSED)
            .unwrap();
        assert_eq!(paused.value, vec![1u8]);
    }
}
