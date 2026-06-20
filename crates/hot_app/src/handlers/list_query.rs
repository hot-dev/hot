use ahash::AHashMap;
use chrono::{DateTime, Utc};
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct PageParams {
    pub current_page_num: i64,
    pub page_size: i64,
    pub offset: i64,
}

impl PageParams {
    pub fn parse(params: &AHashMap<String, String>, page_size: i64) -> Self {
        let current_page_num = params
            .get("p")
            .and_then(|p| p.parse::<i64>().ok())
            .filter(|p| *p > 0)
            .unwrap_or(1);

        Self {
            current_page_num,
            page_size,
            offset: (current_page_num - 1) * page_size,
        }
    }
}

#[derive(Debug, Clone)]
pub struct PaginationWindow {
    pub total_pages: i64,
    pub has_next_page: bool,
    pub has_prev_page: bool,
    pub start_page: i64,
    pub end_page: i64,
}

impl PaginationWindow {
    pub fn new(total_items: i64, page: &PageParams) -> Self {
        let total_pages = if total_items > 0 {
            (total_items + page.page_size - 1) / page.page_size
        } else {
            1
        };
        let has_next_page = page.current_page_num < total_pages;
        let has_prev_page = page.current_page_num > 1;
        let start_page = std::cmp::max(1, page.current_page_num - 2);
        let end_page = std::cmp::min(total_pages, page.current_page_num + 2);

        Self {
            total_pages,
            has_next_page,
            has_prev_page,
            start_page,
            end_page,
        }
    }
}

#[derive(Debug, Clone)]
pub struct SearchAndTimeRange {
    pub selected_time_range: String,
    pub search_query: String,
    pub time_range_cutoff: Option<DateTime<Utc>>,
}

impl SearchAndTimeRange {
    pub fn parse(params: &AHashMap<String, String>, now: DateTime<Utc>) -> Self {
        let selected_time_range = selected_value(params, "time_range", "all");
        let search_query = selected_value(params, "search", "");
        let time_range_cutoff = hot::time_range::parse_time_range_cutoff(
            params.get("time_range").map(|s| s.as_str()),
            now,
        );

        Self {
            selected_time_range,
            search_query,
            time_range_cutoff,
        }
    }

    pub fn search_term(&self) -> Option<&str> {
        non_empty_str(&self.search_query)
    }
}

pub fn selected_value(params: &AHashMap<String, String>, key: &str, default: &str) -> String {
    params
        .get(key)
        .map(|s| s.to_string())
        .unwrap_or_else(|| default.to_string())
}

pub fn selected_csv_or_default(
    params: &AHashMap<String, String>,
    key: &str,
    default: &[&str],
) -> Vec<String> {
    match params.get(key).map(|s| s.as_str()) {
        Some("") | None => default.iter().map(|s| s.to_string()).collect(),
        Some(value) => split_csv(value),
    }
}

pub fn selected_csv_or_empty(params: &AHashMap<String, String>, key: &str) -> Vec<String> {
    params
        .get(key)
        .map(|value| split_csv(value))
        .unwrap_or_default()
}

pub fn filter_unless_all(selected: &[String], all_count: usize) -> Option<Vec<&str>> {
    if selected.len() == all_count {
        None
    } else {
        Some(selected.iter().map(|s| s.as_str()).collect())
    }
}

pub fn non_empty_filter(selected: &[String]) -> Option<Vec<&str>> {
    if selected.is_empty() {
        None
    } else {
        Some(selected.iter().map(|s| s.as_str()).collect())
    }
}

pub fn optional_uuid_param(params: &AHashMap<String, String>, key: &str) -> (String, Option<Uuid>) {
    let selected = selected_value(params, key, "");
    let uuid = non_empty_str(&selected).and_then(|s| Uuid::parse_str(s).ok());
    (selected, uuid)
}

pub fn non_empty_str(value: &str) -> Option<&str> {
    if value.is_empty() { None } else { Some(value) }
}

fn split_csv(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params(pairs: &[(&str, &str)]) -> AHashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn page_params_defaults_to_first_page() {
        let page = PageParams::parse(&params(&[]), 20);
        assert_eq!(page.current_page_num, 1);
        assert_eq!(page.offset, 0);
        assert_eq!(page.page_size, 20);
    }

    #[test]
    fn page_params_computes_offset() {
        let page = PageParams::parse(&params(&[("p", "3")]), 20);
        assert_eq!(page.current_page_num, 3);
        assert_eq!(page.offset, 40);
    }

    #[test]
    fn page_params_rejects_non_positive_and_garbage() {
        for raw in ["0", "-2", "abc", ""] {
            let page = PageParams::parse(&params(&[("p", raw)]), 20);
            assert_eq!(page.current_page_num, 1, "raw={raw}");
            assert_eq!(page.offset, 0, "raw={raw}");
        }
    }

    #[test]
    fn pagination_window_empty_results() {
        let page = PageParams::parse(&params(&[]), 20);
        let window = PaginationWindow::new(0, &page);
        assert_eq!(window.total_pages, 1);
        assert!(!window.has_next_page);
        assert!(!window.has_prev_page);
        assert_eq!(window.start_page, 1);
        assert_eq!(window.end_page, 1);
    }

    #[test]
    fn pagination_window_rounds_up_partial_page() {
        let page = PageParams::parse(&params(&[]), 20);
        let window = PaginationWindow::new(21, &page);
        assert_eq!(window.total_pages, 2);
        assert!(window.has_next_page);
        assert!(!window.has_prev_page);
    }

    #[test]
    fn pagination_window_clamps_surrounding_range() {
        let page = PageParams::parse(&params(&[("p", "5")]), 10);
        let window = PaginationWindow::new(100, &page); // 10 pages
        assert_eq!(window.total_pages, 10);
        assert!(window.has_next_page);
        assert!(window.has_prev_page);
        assert_eq!(window.start_page, 3);
        assert_eq!(window.end_page, 7);
    }

    #[test]
    fn pagination_window_clamps_to_bounds_on_last_page() {
        let page = PageParams::parse(&params(&[("p", "10")]), 10);
        let window = PaginationWindow::new(100, &page);
        assert_eq!(window.start_page, 8);
        assert_eq!(window.end_page, 10);
        assert!(!window.has_next_page);
    }

    #[test]
    fn selected_csv_default_when_missing_or_blank() {
        let default = ["a", "b"];
        assert_eq!(
            selected_csv_or_default(&params(&[]), "statuses", &default),
            vec!["a", "b"]
        );
        assert_eq!(
            selected_csv_or_default(&params(&[("statuses", "")]), "statuses", &default),
            vec!["a", "b"]
        );
        assert_eq!(
            selected_csv_or_default(&params(&[("statuses", "x, y ,")]), "statuses", &default),
            vec!["x", "y"]
        );
    }

    #[test]
    fn filter_unless_all_returns_none_when_everything_selected() {
        let selected = vec!["a".to_string(), "b".to_string()];
        assert!(filter_unless_all(&selected, 2).is_none());
        assert_eq!(filter_unless_all(&selected, 3), Some(vec!["a", "b"]));
    }

    #[test]
    fn non_empty_filter_behaviour() {
        assert!(non_empty_filter(&[]).is_none());
        let selected = vec!["x".to_string()];
        assert_eq!(non_empty_filter(&selected), Some(vec!["x"]));
    }

    #[test]
    fn optional_uuid_param_parses_valid_and_rejects_invalid() {
        let valid = "550e8400-e29b-41d4-a716-446655440000";
        let (raw, uuid) = optional_uuid_param(&params(&[("project", valid)]), "project");
        assert_eq!(raw, valid);
        assert_eq!(uuid, Some(Uuid::parse_str(valid).unwrap()));

        let (raw, uuid) = optional_uuid_param(&params(&[("project", "nope")]), "project");
        assert_eq!(raw, "nope");
        assert!(uuid.is_none());

        let (raw, uuid) = optional_uuid_param(&params(&[]), "project");
        assert_eq!(raw, "");
        assert!(uuid.is_none());
    }

    #[test]
    fn search_and_time_range_parses_search_term() {
        let now = Utc::now();
        let parsed = SearchAndTimeRange::parse(&params(&[("search", "hello")]), now);
        assert_eq!(parsed.search_query, "hello");
        assert_eq!(parsed.search_term(), Some("hello"));
        assert_eq!(parsed.selected_time_range, "all");

        let empty = SearchAndTimeRange::parse(&params(&[]), now);
        assert_eq!(empty.search_term(), None);
    }
}
