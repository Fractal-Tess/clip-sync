use std::fmt;

use time::{OffsetDateTime, format_description::well_known::Rfc3339};

use clip_sync_ipc::protocol::HistoryItem;

pub const DEFAULT_HISTORY_RESULT_LIMIT: u32 = 100;
pub const MAX_HISTORY_RESULT_LIMIT: u32 = 500;
pub const MAX_HISTORY_QUERY_BYTES: usize = 4096;
const MAX_QUERY_TOKENS: usize = 64;

#[derive(Clone, Default, PartialEq, Eq)]
pub struct HistoryQuery {
    terms: Vec<String>,
    devices: Vec<String>,
    types: Vec<String>,
    before_millis: Option<u64>,
    pinned: Option<bool>,
    min_size: Option<u64>,
    max_size: Option<u64>,
}

impl fmt::Debug for HistoryQuery {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("HistoryQuery([REDACTED])")
    }
}

impl HistoryQuery {
    /// Parses free text and typed history filters.
    ///
    /// Free-text terms are conjunctive. Commas and whitespace separate filters.
    /// Repeated `device:` and `type:` filters are also conjunctive. `d:`, `t:`,
    /// and `p:` abbreviate `device:`, `type:`, and `pinned:` respectively.
    ///
    /// # Errors
    ///
    /// Returns a position-aware error for malformed quoting, invalid filter
    /// values, contradictory bounds, or queries exceeding the public limits.
    pub fn parse(input: &str) -> Result<Self, QueryError> {
        if input.len() > MAX_HISTORY_QUERY_BYTES {
            return Err(QueryError::new(
                MAX_HISTORY_QUERY_BYTES,
                QueryErrorKind::QueryTooLong,
            ));
        }

        let tokens = tokenize(input)?;
        if tokens.len() > MAX_QUERY_TOKENS {
            return Err(QueryError::new(input.len(), QueryErrorKind::TooManyTokens));
        }

        let mut query = Self::default();
        for token in &tokens {
            query.push_token(token)?;
        }
        if query
            .min_size
            .zip(query.max_size)
            .is_some_and(|(minimum, maximum)| minimum > maximum)
        {
            return Err(QueryError::new(
                input.len(),
                QueryErrorKind::ContradictorySizeBounds,
            ));
        }
        Ok(query)
    }

    fn push_token(&mut self, token: &Token) -> Result<(), QueryError> {
        let Some((name, value)) = token.value.split_once(':') else {
            self.terms.push(normalize(&token.value));
            return Ok(());
        };
        let name = name.to_ascii_lowercase();
        match name.as_str() {
            "device" | "d" => self.devices.push(non_empty_normalized(token, value)?),
            "type" | "t" => self.types.push(non_empty_normalized(token, value)?),
            "before" => {
                require_value(token, value)?;
                let before = parse_before(value)
                    .ok_or_else(|| QueryError::new(token.offset, QueryErrorKind::InvalidBefore))?;
                self.before_millis = Some(
                    self.before_millis
                        .map_or(before, |current| current.min(before)),
                );
            }
            "pinned" | "p" => {
                let pinned = match value.to_ascii_lowercase().as_str() {
                    "true" => true,
                    "false" => false,
                    _ => {
                        return Err(QueryError::new(token.offset, QueryErrorKind::InvalidPinned));
                    }
                };
                if self.pinned.is_some_and(|current| current != pinned) {
                    return Err(QueryError::new(
                        token.offset,
                        QueryErrorKind::ConflictingPinned,
                    ));
                }
                self.pinned = Some(pinned);
            }
            "min-size" => {
                require_value(token, value)?;
                let size = parse_size(value)
                    .ok_or_else(|| QueryError::new(token.offset, QueryErrorKind::InvalidSize))?;
                self.min_size = Some(self.min_size.map_or(size, |current| current.max(size)));
            }
            "max-size" => {
                require_value(token, value)?;
                let size = parse_size(value)
                    .ok_or_else(|| QueryError::new(token.offset, QueryErrorKind::InvalidSize))?;
                self.max_size = Some(self.max_size.map_or(size, |current| current.min(size)));
            }
            _ => self.terms.push(normalize(&token.value)),
        }
        Ok(())
    }
}

#[derive(Clone)]
struct IndexedHistoryItem {
    item: HistoryItem,
    preview: String,
    source_node: String,
    source_device: String,
    mime_types: Vec<String>,
    content_id: String,
}

impl IndexedHistoryItem {
    fn new(item: HistoryItem) -> Self {
        Self {
            preview: normalize(&item.preview),
            source_node: normalize(&item.source_node),
            source_device: normalize(&item.source_device),
            mime_types: item
                .mime_types
                .iter()
                .map(|value| normalize(value))
                .collect(),
            content_id: normalize(&item.content_id),
            item,
        }
    }

    fn matches(&self, query: &HistoryQuery) -> bool {
        query
            .before_millis
            .is_none_or(|before| self.item.physical_millis < before)
            && query.pinned.is_none_or(|pinned| self.item.pinned == pinned)
            && query
                .min_size
                .is_none_or(|minimum| self.item.logical_size >= minimum)
            && query
                .max_size
                .is_none_or(|maximum| self.item.logical_size <= maximum)
            && query.devices.iter().all(|device| {
                self.source_node.contains(device) || self.source_device.contains(device)
            })
            && query
                .types
                .iter()
                .all(|kind| matches_type(&self.mime_types, kind))
            && query.terms.iter().all(|term| {
                self.preview.contains(term)
                    || self.source_node.contains(term)
                    || self.source_device.contains(term)
                    || self.content_id.contains(term)
                    || self
                        .mime_types
                        .iter()
                        .any(|mime_type| mime_type.contains(term))
            })
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

    #[must_use]
    pub fn search(&self, query: &HistoryQuery, requested_limit: u32) -> Vec<HistoryItem> {
        self.entries
            .iter()
            .filter(|entry| entry.matches(query))
            .take(bounded_result_limit(requested_limit))
            .map(|entry| entry.item.clone())
            .collect()
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

#[derive(Clone, Debug, PartialEq, Eq)]
struct Token {
    value: String,
    offset: usize,
}

fn tokenize(input: &str) -> Result<Vec<Token>, QueryError> {
    let mut tokens = Vec::new();
    let mut chars = input.char_indices().peekable();
    while let Some(&(offset, character)) = chars.peek() {
        if character.is_whitespace() || character == ',' {
            chars.next();
            continue;
        }

        let mut value = String::new();
        let mut quote = None;
        while let Some(&(position, character)) = chars.peek() {
            if quote.is_none() && (character.is_whitespace() || character == ',') {
                break;
            }
            chars.next();
            match character {
                '"' | '\'' if quote.is_none() => quote = Some((character, position)),
                character if quote.is_some_and(|(delimiter, _)| delimiter == character) => {
                    quote = None;
                }
                '\\' => {
                    let Some((_, escaped)) = chars.next() else {
                        return Err(QueryError::new(position, QueryErrorKind::TrailingEscape));
                    };
                    value.push(escaped);
                }
                _ => value.push(character),
            }
        }
        if let Some((_, position)) = quote {
            return Err(QueryError::new(position, QueryErrorKind::UnclosedQuote));
        }
        if !value.is_empty() {
            tokens.push(Token { value, offset });
        }
    }
    Ok(tokens)
}

fn non_empty_normalized(token: &Token, value: &str) -> Result<String, QueryError> {
    require_value(token, value)?;
    Ok(normalize(value))
}

fn require_value(token: &Token, value: &str) -> Result<(), QueryError> {
    if value.is_empty() {
        Err(QueryError::new(
            token.offset,
            QueryErrorKind::MissingFilterValue,
        ))
    } else {
        Ok(())
    }
}

fn normalize(value: &str) -> String {
    value.to_lowercase()
}

fn matches_type(mime_types: &[String], kind: &str) -> bool {
    mime_types.iter().any(|mime_type| match kind {
        "file" | "files" => mime_type == "text/uri-list",
        "image" => mime_type.starts_with("image/"),
        "text" => {
            mime_type.starts_with("text/")
                || matches!(mime_type.as_str(), "string" | "text" | "utf8_string")
        }
        _ => mime_type.contains(kind),
    })
}
fn parse_before(value: &str) -> Option<u64> {
    if value.bytes().all(|byte| byte.is_ascii_digit()) {
        return value.parse().ok();
    }
    let timestamp = OffsetDateTime::parse(value, &Rfc3339).ok()?;
    u64::try_from(timestamp.unix_timestamp_nanos().div_euclid(1_000_000)).ok()
}

fn parse_size(value: &str) -> Option<u64> {
    let split_at = value
        .find(|character: char| !character.is_ascii_digit())
        .unwrap_or(value.len());
    if split_at == 0 {
        return None;
    }
    let number = value[..split_at].parse::<u64>().ok()?;
    let suffix = value[split_at..].to_ascii_lowercase();
    let multiplier = match suffix.as_str() {
        "" | "b" => 1,
        "kb" => 1_000,
        "kib" => 1_024,
        "mb" => 1_000_000,
        "mib" => 1_048_576,
        "gb" => 1_000_000_000,
        "gib" => 1_073_741_824,
        _ => return None,
    };
    number.checked_mul(multiplier)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum QueryErrorKind {
    QueryTooLong,
    TooManyTokens,
    TrailingEscape,
    UnclosedQuote,
    MissingFilterValue,
    InvalidBefore,
    InvalidPinned,
    ConflictingPinned,
    InvalidSize,
    ContradictorySizeBounds,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QueryError {
    offset: usize,
    kind: QueryErrorKind,
}

impl QueryError {
    const fn new(offset: usize, kind: QueryErrorKind) -> Self {
        Self { offset, kind }
    }
}

impl fmt::Display for QueryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let detail = match self.kind {
            QueryErrorKind::QueryTooLong => "query exceeds the 4096-byte limit",
            QueryErrorKind::TooManyTokens => "query has more than 64 tokens",
            QueryErrorKind::TrailingEscape => "trailing escape",
            QueryErrorKind::UnclosedQuote => "unclosed quote",
            QueryErrorKind::MissingFilterValue => "filter value is missing",
            QueryErrorKind::InvalidBefore => {
                "before expects RFC3339 date/time or unix milliseconds"
            }
            QueryErrorKind::InvalidPinned => "pinned expects true or false",
            QueryErrorKind::ConflictingPinned => "pinned filters conflict",
            QueryErrorKind::InvalidSize => {
                "size expects bytes or a KB, KiB, MB, MiB, GB, or GiB suffix"
            }
            QueryErrorKind::ContradictorySizeBounds => "minimum size is greater than maximum size",
        };
        write!(formatter, "invalid query at byte {}: {detail}", self.offset)
    }
}

impl std::error::Error for QueryError {}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn item(
        content_id: &str,
        preview: &str,
        mime_type: &str,
        size: u64,
        device: &str,
        pinned: bool,
        millis: u64,
    ) -> HistoryItem {
        HistoryItem {
            content_id: content_id.to_owned(),
            preview: preview.to_owned(),
            mime_types: vec![mime_type.to_owned()],
            logical_size: size,
            source_node: format!("node-{device}"),
            pinned,
            source_device: device.to_owned(),
            physical_millis: millis,
            origin_millis: Some(millis),
        }
    }

    #[test]
    fn typed_filters_and_phrases_are_applied_together() {
        let index = HistorySearchIndex::new(vec![
            item(
                "new",
                "Quarterly Build Finished",
                "text/plain",
                4_096,
                "Office Laptop",
                true,
                1_704_067_200_001,
            ),
            item(
                "old",
                "Quarterly Build Finished",
                "text/plain",
                4_096,
                "Office Laptop",
                true,
                1_704_067_199_999,
            ),
        ]);
        let query = HistoryQuery::parse(
            r#""build finished" device:"office laptop" type:text pinned:true min-size:4KiB max-size:5KB before:2024-01-01T00:00:00Z"#,
        )
        .expect("valid query");

        let results = index.search(&query, 100);
        assert_eq!(
            results
                .iter()
                .map(|result| result.content_id.as_str())
                .collect::<Vec<_>>(),
            ["old"]
        );

        let unix_millis = HistoryQuery::parse("before:1704067200000").unwrap();
        assert_eq!(index.search(&unix_millis, 100)[0].content_id, "old");
    }

    #[test]
    fn abbreviated_comma_filters_match_device_type_and_pin_state() {
        let index = HistorySearchIndex::new(vec![
            item("match", "Screenshot", "image/png", 10, "vd", true, 2),
            item(
                "wrong-device",
                "Screenshot",
                "image/png",
                10,
                "kiwi",
                true,
                1,
            ),
            item("wrong-type", "Text", "text/plain", 10, "vd", true, 3),
        ]);

        let query = HistoryQuery::parse("D:vd,t:image,P:true").expect("valid aliases");
        let results = index.search(&query, 100);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].content_id, "match");
    }

    #[test]
    fn quoted_and_escaped_commas_remain_filter_values() {
        let index = HistorySearchIndex::new(vec![item(
            "match",
            "Text",
            "text/plain",
            10,
            "office,laptop",
            false,
            1,
        )]);

        assert_eq!(
            index.search(
                &HistoryQuery::parse(r#"d:"office,laptop",t:text"#).unwrap(),
                100
            )[0]
            .content_id,
            "match"
        );
        assert_eq!(
            index.search(
                &HistoryQuery::parse(r"d:office\,laptop,t:text").unwrap(),
                100
            )[0]
            .content_id,
            "match"
        );
    }

    #[test]
    fn newest_first_order_and_limit_are_deterministic() {
        let index = HistorySearchIndex::new(vec![
            item("z", "match", "text/plain", 1, "d", false, 1),
            item("b", "match", "text/plain", 1, "d", false, 2),
            item("a", "match", "text/plain", 1, "d", false, 2),
        ]);
        let results = index.search(&HistoryQuery::parse("match").unwrap(), 2);
        assert_eq!(
            results
                .iter()
                .map(|result| result.content_id.as_str())
                .collect::<Vec<_>>(),
            ["a", "b"]
        );
    }

    #[test]
    fn pages_preserve_order_and_report_total_matches() {
        let index = HistorySearchIndex::new(vec![
            item("five", "match", "text/plain", 1, "d", false, 5),
            item("four", "match", "text/plain", 1, "d", false, 4),
            item("three", "match", "text/plain", 1, "d", false, 3),
            item("two", "match", "text/plain", 1, "d", false, 2),
            item("one", "match", "text/plain", 1, "d", false, 1),
            item("excluded", "other", "text/plain", 1, "d", false, 6),
        ]);
        let query = HistoryQuery::parse("match").unwrap();

        let partial = index.page(&query, 1, 2);
        assert_eq!(partial.total, 5);
        assert_eq!(
            partial
                .items
                .iter()
                .map(|result| result.content_id.as_str())
                .collect::<Vec<_>>(),
            ["four", "three"]
        );

        let final_page = index.page(&query, 4, 2);
        assert_eq!(final_page.total, 5);
        assert_eq!(final_page.items[0].content_id, "one");
        assert_eq!(final_page.items.len(), 1);

        let oversized_offset = index.page(&query, u32::MAX, 2);
        assert_eq!(oversized_offset.total, 5);
        assert!(oversized_offset.items.is_empty());
    }

    #[test]
    fn page_limits_remain_bounded_and_zero_uses_default() {
        let index = HistorySearchIndex::new(
            (0_u64..600)
                .map(|millis| {
                    item(
                        &format!("item-{millis}"),
                        "match",
                        "text/plain",
                        1,
                        "d",
                        false,
                        millis,
                    )
                })
                .collect(),
        );
        let query = HistoryQuery::parse("match").unwrap();

        let bounded = index.page(&query, 0, u32::MAX);
        assert_eq!(bounded.total, 600);
        assert_eq!(bounded.items.len(), MAX_HISTORY_RESULT_LIMIT as usize);

        let defaulted = index.page(&query, 0, 0);
        assert_eq!(defaulted.total, 600);
        assert_eq!(defaulted.items.len(), DEFAULT_HISTORY_RESULT_LIMIT as usize);
    }

    #[test]
    fn invalid_queries_do_not_echo_sensitive_terms() {
        for (query, expected) in [
            (r#"device:"private words"#, "unclosed quote"),
            ("pinned:secret", "pinned expects true or false"),
            ("before:secret", "before expects RFC3339"),
            ("before:1969-12-31T23:59:59.999Z", "before expects RFC3339"),
            ("min-size:secret", "size expects bytes"),
        ] {
            let error = HistoryQuery::parse(query).unwrap_err().to_string();
            assert!(error.contains(expected));
            assert!(!error.contains("private"));
            assert!(!error.contains("secret"));
        }
    }

    #[test]
    fn debug_output_redacts_queries_and_indexed_previews() {
        let query = HistoryQuery::parse("private search terms").unwrap();
        let index = HistorySearchIndex::new(vec![item(
            "id",
            "private clipboard preview",
            "text/plain",
            1,
            "node",
            false,
            1,
        )]);

        assert_eq!(format!("{query:?}"), "HistoryQuery([REDACTED])");
        assert_eq!(format!("{index:?}"), "HistorySearchIndex { entries: 1 }");
    }

    proptest! {
        #[test]
        fn parser_never_panics_and_respects_result_bounds(
            query in "\\PC{0,5000}",
            limit in any::<u32>(),
        ) {
            if let Ok(query) = HistoryQuery::parse(&query) {
                let index = HistorySearchIndex::new(vec![
                    item("a", "arbitrary", "text/plain", 1, "node", false, 1),
                ]);
                let results = index.search(&query, limit);
                prop_assert!(results.len() <= MAX_HISTORY_RESULT_LIMIT as usize);
            }
        }
    }
}
