use substreams::store::{StoreNew, StoreSet, StoreSetInt64};
use tycho_substreams::prelude::*;

use crate::{
    keys::sy_rate_needed_key,
    sy::{is_class_attribute, TokenClass},
};

/// Records, per SY, whether any of its tokens is converted at the exchange rate.
///
/// Only such an SY needs its rate read when no live market prices against it: a `one_to_one`
/// conversion is a decimal rescale and never looks at the rate. Written whenever `map_sy_state`
/// emits the SY's classes, at creation and on each re-classification.
#[substreams::handlers::store]
pub fn store_sy_rate_needed(sy_state: BlockChanges, store: StoreSetInt64) {
    for tx_changes in sy_state.changes {
        for change in tx_changes.entity_changes {
            let Some(needed) = needs_rate(&change.attributes) else { continue };
            store.set(0, sy_rate_needed_key(&change.component_id), &i64::from(needed));
        }
    }
}

/// Whether a set of class attributes includes a rate-converted token; `None` when the change
/// carries no classes at all, as a pause does.
fn needs_rate(attributes: &[Attribute]) -> Option<bool> {
    let mut classes = attributes
        .iter()
        .filter(|attribute| is_class_attribute(&attribute.name))
        .peekable();
    classes.peek()?;
    Some(classes.any(|attribute| {
        attribute.change == i32::from(ChangeType::Update) &&
            attribute.value ==
                TokenClass::IndexRate
                    .as_str()
                    .as_bytes()
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attribute(name: &str, value: &str, change: ChangeType) -> Attribute {
        Attribute {
            name: name.to_string(),
            value: value.as_bytes().to_vec(),
            change: change.into(),
        }
    }

    #[test]
    fn a_rate_converted_token_needs_the_rate() {
        let attributes = [
            attribute("token_in_class_0xaa", "one_to_one", ChangeType::Update),
            attribute("token_out_class_0xbb", "index_rate", ChangeType::Update),
        ];
        assert_eq!(needs_rate(&attributes), Some(true));
    }

    #[test]
    fn one_to_one_tokens_alone_need_no_rate() {
        let attributes = [attribute("token_in_class_0xaa", "one_to_one", ChangeType::Update)];
        assert_eq!(needs_rate(&attributes), Some(false));
    }

    /// A re-classification that deletes the only rate-converted token stops the reads.
    #[test]
    fn a_deleted_rate_converted_class_needs_no_rate() {
        let attributes = [
            attribute("token_in_class_0xaa", "one_to_one", ChangeType::Update),
            attribute("token_out_class_0xbb", "", ChangeType::Deletion),
        ];
        assert_eq!(needs_rate(&attributes), Some(false));
    }

    #[test]
    fn a_pause_says_nothing_about_the_rate() {
        assert_eq!(needs_rate(&[attribute("paused", "\u{1}", ChangeType::Update)]), None);
    }
}
