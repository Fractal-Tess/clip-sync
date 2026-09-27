use std::fmt;

use clip_sync_ipc::protocol::HistoryItem;

pub const DEFAULT_HISTORY_RESULT_LIMIT: u32 = 100;
pub const MAX_HISTORY_RESULT_LIMIT: u32 = 500;
pub const MAX_HISTORY_QUERY_BYTES: usize = 4096;

/// Case-insensitive words that must all appear in an item's preview, MIME
/// types, or source device. It holds clipboard text, so it is never logged.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct HistoryQuery {
    terms: Vec<String>,
}

impl fmt::Debug for HistoryQuery {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("HistoryQuery([REDACTED])")
    }
}

impl HistoryQuery {
    /// Splits the typed filter into words.
    ///
    /// # Errors
    ///
    /// Returns an error when the query exceeds [`MAX_HISTORY_QUERY_BYTES`].
    pub fn parse(input: &str) -> Result<Self, QueryTooLong> {
        if input.len() > MAX_HISTORY_QUERY_BYTES {
            return Err(QueryTooLong);
        }
        Ok(Self {
            terms: input.split_whitespace().map(str::to_lowercase).collect(),
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QueryTooLong;

impl fmt::Display for QueryTooLong {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "history query exceeds {MAX_HISTORY_QUERY_BYTES} bytes"
        )
    }
}

impl std::error::Error for QueryTooLong {}

#[derive(Clone)]
struct IndexedHistoryItem {
    item: HistoryItem,
    /// Lowercased preview, MIME types, and source names, searched as one.
    haystack: String,
}

impl IndexedHistoryItem {
    fn new(item: HistoryItem) -> Self {
        let mut haystack = item.preview.to_lowercase();
        for field in item
            .mime_types
            .iter()
            .chain([&item.source_device, &item.source_node])
        {
            haystack.push('\n');
            haystack.push_str(&field.to_lowercase());
        }
        Self { item, haystack }
    }

    fn matches(&self, query: &HistoryQuery) -> bool {
        query.terms.iter().all(|term| self.haystack.contains(term))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HistorySearchPage {
    pub items: Vec<HistoryItem>,
    pub total: u64,
}

#[derive(Clone, Default)]
pub struct HistorySearchIndex {
    entries: Vec<IndexedHistoryItem>,
}

impl fmt::Debug for HistorySearchIndex {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("HistorySearchIndex")
            .field("entries", &self.entries.len())
            .finish()
    }
}

impl HistorySearchIndex {
    #[must_use]
    pub fn new(mut items: Vec<HistoryItem>) -> Self {
        items.sort_unstable_by(|left, right| {
            right
                .physical_millis
                .cmp(&left.physical_millis)
                .then_with(|| left.content_id.cmp(&right.content_id))
        });
        Self {
            entries: items.into_iter().map(IndexedHistoryItem::new).collect(),
        }
    }

    #[must_use]
    pub fn page(
        &self,
        query: &HistoryQuery,
        offset: u32,
        requested_limit: u32,
    ) -> HistorySearchPage {
        let limit = bounded_result_limit(requested_limit);
        let offset = u64::from(offset);
        let mut items = Vec::with_capacity(limit);
        let mut total = 0_u64;

        for entry in self.entries.iter().filter(|entry| entry.matches(query)) {
            let matching_index = total;
            total = total.saturating_add(1);
            if matching_index >= offset && items.len() < limit {
                items.push(entry.item.clone());
            }
        }

        HistorySearchPage { items, total }
    }
}

fn bounded_result_limit(requested_limit: u32) -> usize {
    let limit = if requested_limit == 0 {
        DEFAULT_HISTORY_RESULT_LIMIT
    } else {
        requested_limit.min(MAX_HISTORY_RESULT_LIMIT)
    };
    usize::try_from(limit).unwrap_or(MAX_HISTORY_RESULT_LIMIT as usize)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(content_id: &str, preview: &str, millis: u64) -> HistoryItem {
        HistoryItem {
            content_id: content_id.to_owned(),
            preview: preview.to_owned(),
            mime_types: vec!["text/plain".to_owned()],
            logical_size: 1,
            source_node: "node".to_owned(),
            pinned: false,
            physical_millis: millis,
            source_device: "kiwi".to_owned(),
            origin_millis: None,
            remote: false,
            pinned_millis: None,
        }
    }

    fn ids(page: &HistorySearchPage) -> Vec<&str> {
        page.items
            .iter()
            .map(|item| item.content_id.as_str())
            .collect()
    }

    #[test]
    fn every_word_must_match_case_insensitively() {
        let index = HistorySearchIndex::new(vec![
            item("a", "Release notes draft", 1),
            item("b", "release checklist", 2),
        ]);
        let query = HistoryQuery::parse("  RELEASE   notes ").unwrap();
        assert_eq!(ids(&index.page(&query, 0, 0)), ["a"]);
    }

    #[test]
    fn words_match_device_and_type_too() {
        let index = HistorySearchIndex::new(vec![item("a", "hello", 1)]);
        assert_eq!(
            index
                .page(&HistoryQuery::parse("kiwi").unwrap(), 0, 0)
                .total,
            1
        );
        assert_eq!(
            index
                .page(&HistoryQuery::parse("text/plain").unwrap(), 0, 0)
                .total,
            1
        );
    }

    #[test]
    fn empty_query_pages_newest_first() {
        let index = HistorySearchIndex::new(vec![
            item("old", "x", 1),
            item("new", "x", 3),
            item("mid", "x", 2),
        ]);
        let page = index.page(&HistoryQuery::default(), 1, 1);
        assert_eq!(ids(&page), ["mid"]);
        assert_eq!(page.total, 3);
    }

    #[test]
    fn oversized_query_is_rejected() {
        let long = "x".repeat(MAX_HISTORY_QUERY_BYTES + 1);
        assert_eq!(HistoryQuery::parse(&long), Err(QueryTooLong));
    }
}
