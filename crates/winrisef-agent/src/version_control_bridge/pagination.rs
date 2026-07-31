use anyhow::Context;
use serde::Serialize;
use serde_json::Value;

const CONTROL_RESPONSE_BUDGET: usize = 60 * 1024;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PageResponse<T> {
    items: T,
    next_skip: usize,
    has_more: bool,
}

pub(super) fn bounded_page<T: Serialize>(
    mut items: Vec<T>,
    page_state: impl Fn(usize) -> (usize, bool),
    oversized_message: &str,
    encode_context: &'static str,
) -> anyhow::Result<Value> {
    let keep = fit_page_prefix(&items, &page_state, oversized_message)?;
    items.truncate(keep);
    let (next_skip, has_more) = page_state(keep);
    serde_json::to_value(PageResponse {
        items,
        next_skip,
        has_more,
    })
    .context(encode_context)
}

fn fit_page_prefix<T: Serialize>(
    items: &[T],
    page_state: &impl Fn(usize) -> (usize, bool),
    oversized_message: &str,
) -> anyhow::Result<usize> {
    let serialized_size = |len: usize| -> anyhow::Result<usize> {
        let (next_skip, has_more) = page_state(len);
        Ok(serde_json::to_vec(&PageResponse {
            items: &items[..len],
            next_skip,
            has_more,
        })?
        .len())
    };
    if items.is_empty() || serialized_size(items.len())? <= CONTROL_RESPONSE_BUDGET {
        return Ok(items.len());
    }
    anyhow::ensure!(
        serialized_size(1)? <= CONTROL_RESPONSE_BUDGET,
        "{oversized_message}"
    );

    let mut low = 1;
    let mut high = items.len() - 1;
    while low < high {
        let middle = low + (high - low).div_ceil(2);
        if serialized_size(middle)? <= CONTROL_RESPONSE_BUDGET {
            low = middle;
        } else {
            high = middle - 1;
        }
    }
    Ok(low)
}

#[cfg(test)]
mod tests {
    use super::{CONTROL_RESPONSE_BUDGET, bounded_page};

    #[test]
    fn keeps_the_largest_serialized_prefix_within_budget() {
        let item = "x".repeat(CONTROL_RESPONSE_BUDGET / 2);
        let page = bounded_page(
            vec![item.clone(), item, "tail".to_owned()],
            |len| (len, len < 3),
            "item too large",
            "encode page",
        )
        .unwrap();
        assert_eq!(page["items"].as_array().unwrap().len(), 1);
        assert_eq!(page["nextSkip"], 1);
        assert_eq!(page["hasMore"], true);
    }
}
